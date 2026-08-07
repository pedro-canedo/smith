//! The tool-dispatch funnel: permission, hooks, checkpoints, execution.

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::event::{
    AgentEvent, AgentPhase, PermissionDecision, PermissionRequest, ProgressReporter,
};
use crate::message::ContentBlock;
use crate::permission_detail::format_permission_detail;
use crate::subagent::{self};
use crate::tool::{PermissionClass, ToolContext, ToolResult};

use super::executor::{PermissionAsk, QuestionAsk};
use super::{Agent, INTERCEPTED_TOOLS};

/// How many tool calls from one round may be in flight at once.
///
/// Only `ReadOnly` calls are ever run concurrently (see
/// [`Agent::is_concurrency_safe`]), so this bounds reads, globs and greps —
/// work that is dominated by the filesystem, not the CPU.
///
/// Eight, rather than "however many the model asked for":
/// - A model exploring a codebase emits three to six reads in a batch, so
///   eight covers the realistic case without a queue ever forming.
/// - Past roughly eight concurrent traversals a single disk is seek-bound;
///   the extra parallelism buys latency, not throughput, and on a spinning
///   disk actively costs it.
/// - It keeps file-descriptor use trivially bounded. A `glob` over a deep
///   tree holds several directory handles open at once, and an unbounded
///   `join_all` over fifty of them can walk into `EMFILE` against the usual
///   1024 soft limit — a failure mode that shows up as tool calls failing at
///   random, which is far worse than being a little slower.
/// - Eight simultaneously-spinning tool cards is already the most a terminal
///   transcript can show without becoming noise.
pub(super) const MAX_CONCURRENT_TOOLS: usize = 8;

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_one_tool(
        &mut self,
        id: &str,
        name: &str,
        input: serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
        permission_tx: &mpsc::UnboundedSender<PermissionAsk>,
        question_tx: &mpsc::UnboundedSender<QuestionAsk>,
        cancel: CancellationToken,
    ) -> ToolResult {
        // `ask_user` (a modal), `write_tasks` (a checklist) and `task`
        // (delegation) are intercepted below and never reach the class lookup,
        // the plan gate, the permission prompt or `tools.execute`. Named once
        // here so the gates in between can say what they are exempting rather
        // than re-testing the names one at a time.
        let intercepted =
            name == "ask_user" || name == "write_tasks" || name == subagent::TASK_TOOL;

        let class = self
            .tools
            .permission_class(name)
            .unwrap_or(PermissionClass::Dangerous);

        if !intercepted && self.plan_gated && class != PermissionClass::ReadOnly {
            let result = ToolResult::error(
                "Blocked: a plan is awaiting approval. Tell the user to run `/plan approve` (or `/plan reject` to discard it) before this can run.",
            );
            let _ = events.send(AgentEvent::ToolCallStarted {
                id: id.to_string(),
                tool_name: name.to_string(),
                input,
            });
            let _ = events.send(AgentEvent::ToolCallResult {
                id: id.to_string(),
                output: result.content.clone(),
                is_error: true,
            });
            return result;
        }

        // `PreToolUse`, and this is the only defensible place for it — the
        // full argument is in `docs/authorization.md`, the two halves of it
        // are:
        //
        // - *After* the plan gate, because a hook can only ever subtract
        //   authority. Running it above the gate would spawn a process per
        //   call that could never have run, and would put a hook's "allow"
        //   syntactically upstream of the one gate in this system with no
        //   bypass — an invitation to someone later wiring it up as an
        //   override.
        // - *Before* everything below, because a hook that runs after the
        //   permission prompt cannot prevent the prompt (its main job), and a
        //   hook that rewrites arguments after `format_permission_detail` has
        //   built the modal would have the user approve one call and a
        //   different one run. Above `needs_prompt` rather than merely above
        //   the prompt itself, so a session grant, `scratch_scoped` or
        //   `/permission skip` cannot skip the hook along with the modal.
        //
        // The three intercepted tools below reach this too. They have no plan
        // gate to be after, so for them the hook is simply the first gate
        // there is — which is what lets a policy hook see `task`, the one
        // intercepted tool whose effects are not confined to the UI.
        let input = match self.pre_tool_hooks(id, name, input, events, &cancel).await {
            Ok(input) => input,
            Err(blocked) => return blocked,
        };

        // The intercepted tools below return before `ToolExecutor::execute`,
        // which is where every other call is checked against the schema the
        // model was shown. That made the registry's claim to be "the one
        // place" false, and left three tools re-implementing ad-hoc argument
        // parsing — decent today, but the invariant is what the next
        // intercepted tool inherits. Checked here instead, before the
        // interception, so the claim is true again for every path.
        //
        // A rejection is an ordinary tool error for the same reason it is in
        // the registry: the model sees it and a wrong argument is the most
        // correctable thing it can be told.
        if INTERCEPTED_TOOLS.contains(&name) {
            if let Err(message) = self.tools.validate_input(name, &input) {
                let _ = events.send(AgentEvent::ToolCallStarted {
                    id: id.to_string(),
                    tool_name: name.to_string(),
                    input: input.clone(),
                });
                let _ = events.send(AgentEvent::ToolCallResult {
                    id: id.to_string(),
                    output: message.clone(),
                    is_error: true,
                });
                return ToolResult::error(message);
            }
        }

        // Clarifying questions are allowed even while plan-gated.
        if name == "ask_user" {
            return self
                .run_ask_user(id, input, events, question_tx, cancel)
                .await;
        }

        // Bookkeeping only — no side effects, so it's exempt from both the
        // plan gate and the permission prompt, same reasoning as ask_user.
        if name == "write_tasks" {
            return self.run_write_tasks(id, input, events).await;
        }

        // Delegation. Intercepted for a structural reason rather than a UI
        // one: a subagent *is* an `Agent`, built from this agent's provider,
        // tool registry, model and context — none of which a `Tool` can reach
        // from behind `&self` in another crate. Everything the child is
        // allowed to be is decided from this agent's own state, which is
        // exactly what makes budget, depth and the tool set enforceable here
        // and unforgeable from the tool side. Its own tools are read-only, so
        // like `ask_user` it is exempt from the plan gate and the prompt.
        if name == subagent::TASK_TOOL {
            // `task` is classed `ReadOnly` because a child's own tools are,
            // and interactively that is right: the user is watching, and the
            // child can only look. Unattended, nobody is watching and the
            // child spends the user's money — so it has to be named like
            // anything else. `--allowed-tools` is the only control a headless
            // run has, and "spawn a whole agent" is not what a reader expects
            // it to leave open.
            if self.unattended && !self.allowed_session_tools.contains(name) {
                if let Err(refusal) = self
                    .request_permission(id, name, &input, events, permission_tx)
                    .await
                {
                    return refusal;
                }
            }
            return self.run_task(id, input, events, cancel).await;
        }

        // A call the tool itself vouches is confined to the session's scratch
        // directory skips the prompt: there is no user work in there to
        // protect, and prompting for throwaway files is exactly the friction
        // that pushes the model into writing them into the project instead.
        // Checked last — it can touch the filesystem (symlink resolution) and
        // only matters when the call would otherwise prompt. Deliberately
        // *after* the plan gate above: scratch writes are still side effects,
        // and an unapproved plan blocks them like everything else.
        let needs_prompt = class != PermissionClass::ReadOnly
            && !self.allowed_session_tools.contains(name)
            && !self.permission_policy.auto_allows(class)
            // The scratch exemption is a *friction* argument: prompting for
            // throwaway files is what pushes the model into writing them into
            // the project instead. Unattended there is no friction to spare —
            // the channel answers instantly from `--allowed-tools` — so the
            // exemption buys nothing and costs the only gate a headless run
            // has. It was the one case where a Mutating tool ran in a job that
            // named no tools at all.
            && (self.unattended || !self.tools.scratch_scoped(name, &input, &self.tool_ctx));

        if needs_prompt {
            if let Err(refusal) = self
                .request_permission(id, name, &input, events, permission_tx)
                .await
            {
                return refusal;
            }
        }

        self.dispatch_tool(id, name, input, class, events, cancel)
            .await
    }

    /// Puts one call to the permission channel and folds the answer back in.
    ///
    /// `Err` is the refusal to return to the model; `Ok` means proceed. Split
    /// out because two callers need it: the ordinary class-based check, and
    /// `task` under an unattended run, where there is no other gate at all.
    async fn request_permission(
        &mut self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
        permission_tx: &mpsc::UnboundedSender<PermissionAsk>,
    ) -> Result<(), ToolResult> {
        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::WaitingPermission));
        let detail = format_permission_detail(name, input);
        let (tx, rx) = oneshot::channel();
        let _ = events.send(AgentEvent::PermissionPromptNeeded(PermissionRequest {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            detail: detail.clone(),
        }));
        let sent = permission_tx.send(PermissionAsk {
            request: PermissionRequest {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                detail,
            },
            respond_to: tx,
        });
        if sent.is_err() {
            return Err(ToolResult::error("permission channel closed"));
        }
        match rx.await.unwrap_or(PermissionDecision::Deny) {
            PermissionDecision::Deny => Err(ToolResult::error(
                "User denied permission to run this tool.",
            )),
            PermissionDecision::AllowSession => {
                self.allowed_session_tools.insert(name.to_string());
                Ok(())
            }
            PermissionDecision::AllowOnce => Ok(()),
        }
    }

    /// Runs the `PreToolUse` chain for one call, answering with the arguments
    /// to run with — or with the `ToolResult` that ends the call.
    ///
    /// Takes `&self`, which is what lets the concurrent read-only path call it
    /// too. If it needed `&mut self` the hook would silently not fire for
    /// batched reads, and "the hook did not run and nobody said so" is the one
    /// failure this feature must not have.
    async fn pre_tool_hooks(
        &self,
        id: &str,
        name: &str,
        input: serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value, ToolResult> {
        if self.hooks.is_empty() {
            return Ok(input);
        }
        // The same schema check dispatch applies, handed to the hook layer so
        // a rewrite is validated where it happens and the resulting error can
        // name the hook rather than implicating the model.
        let validate = |candidate: &serde_json::Value| self.tools.validate_input(name, candidate);
        let outcome = self
            .hooks
            .pre_tool_use(&self.hook_ctx(), name, input, &validate, cancel)
            .await;

        let Some(denial) = outcome.denial else {
            // Notices ride the tool's own card, so a hook that fired, rewrote
            // or misbehaved is attached to the call it acted on.
            for line in outcome.notices {
                let _ = events.send(AgentEvent::ToolProgress {
                    id: id.to_string(),
                    line,
                });
            }
            return Ok(outcome.input);
        };

        // A blocked call still gets a card: the user has to be able to see
        // that the agent tried, and what stopped it.
        let _ = events.send(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: name.to_string(),
            input: outcome.input,
        });
        for line in outcome.notices {
            let _ = events.send(AgentEvent::ToolProgress {
                id: id.to_string(),
                line,
            });
        }
        let result = ToolResult::error(denial);
        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: true,
        });
        Err(result)
    }

    /// Runs the `PostToolUse` chain over a finished call, folding whatever it
    /// wrote into the result the model will read.
    ///
    /// Never fails the call — see `HookEvent::fails_closed`. The tool has
    /// already run; the only thing left to decide is whether the model is told
    /// the truth about it.
    async fn post_tool_hooks(
        &self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        mut result: ToolResult,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> ToolResult {
        if self.hooks.is_empty() {
            return result;
        }
        let outcome = self
            .hooks
            .post_tool_use(
                &self.hook_ctx(),
                name,
                input,
                &result.content,
                result.is_error,
                cancel,
            )
            .await;
        for line in outcome.notices {
            let _ = events.send(AgentEvent::ToolProgress {
                id: id.to_string(),
                line,
            });
        }
        if let Some(extra) = outcome.extra {
            result.content = format!("{}\n{extra}", result.content);
        }
        result
    }

    /// Whether this call may run alongside others from the same round.
    ///
    /// `PermissionClass::ReadOnly` and nothing else — permanently, not
    /// pending further work. Three reasons, none of which a future tool
    /// changes:
    /// - Nearly all the wall-clock win is here anyway. A model exploring a
    ///   codebase issues reads, globs and greps in batches; it issues writes
    ///   one or two at a time.
    /// - A concurrency bug in a `Mutating` tool is a data-loss bug. Two
    ///   `edit_file` calls racing on one path corrupt it in a way no test
    ///   reliably catches, and the damage is to the user's work, not ours.
    /// - The permission round-trip is serial by construction: you cannot show
    ///   two modals at once. `Mutating` and `Dangerous` calls would spend
    ///   their time queued behind each other's prompts even if the execution
    ///   underneath them were parallel.
    ///
    /// `ask_user`, `write_tasks` and `task` all declare `ReadOnly`, but they
    /// are intercepted by name before dispatch and need `&mut self` (a
    /// checklist to rewrite, a modal to wait on, a delegation budget to
    /// spend) — so they are excluded here and stay on the serial path with
    /// the interception that owns them. For `task` that is also the right
    /// answer on its merits: two children running at once are two provider
    /// conversations billing in parallel, and their progress lines would
    /// interleave on cards the user cannot tell apart.
    pub(super) fn is_concurrency_safe(&self, name: &str) -> bool {
        if name == "ask_user" || name == "write_tasks" || name == subagent::TASK_TOOL {
            return false;
        }
        self.tools.permission_class(name) == Some(PermissionClass::ReadOnly)
    }

    /// Runs one group of concurrency-safe calls, filling each one's slot in
    /// `results`.
    ///
    /// Slots are written by index as answers arrive, so the model still sees
    /// the round's results in the order it asked for them no matter which
    /// call finishes first. `results` is already seeded with "not executed",
    /// so a call this function never gets to still has an answer.
    pub(super) async fn run_concurrent_group(
        &self,
        group: &[usize],
        tool_uses: &[(String, String, serde_json::Value)],
        results: &mut [ContentBlock],
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        let mut running = futures::stream::iter(group.iter().copied().map(|slot| {
            let (id, name, input) = &tool_uses[slot];
            let cancel = cancel.clone();
            async move {
                // A queued call is admitted to the window only when an
                // earlier one frees a place, which makes this the point where
                // an Esc part-way through a group stops the rest of it.
                // Returning `None` leaves the seeded "not executed" answer in
                // place *and* emits no events, so the TUI never sees a card
                // start that will never finish. Calls already in flight are
                // not dropped: they hold the same token, race it themselves,
                // and come back with a real result and a real
                // `ToolCallResult`.
                if cancel.is_cancelled() {
                    return (slot, None);
                }
                // The second path through authorization, and every gate that
                // applies on the serial one has to be considered here too.
                // The plan gate and the permission prompt are no-ops for
                // `ReadOnly` calls, which is why they are absent — but a
                // `PreToolUse` hook is not, and a hook that silently skipped
                // every batched read would be worse than no hook at all.
                let input = match self
                    .pre_tool_hooks(id, name, input.clone(), events, &cancel)
                    .await
                {
                    Ok(input) => input,
                    Err(blocked) => return (slot, Some(blocked)),
                };
                let result = self
                    .dispatch_tool(id, name, input, PermissionClass::ReadOnly, events, cancel)
                    .await;
                (slot, Some(result))
            }
        }))
        .buffer_unordered(MAX_CONCURRENT_TOOLS);

        while let Some((slot, result)) = running.next().await {
            let Some(result) = result else { continue };
            results[slot] = ContentBlock::ToolResult {
                tool_use_id: tool_uses[slot].0.clone(),
                content: result.content,
                is_error: result.is_error,
            };
        }
    }

    /// The dispatch half of a tool call: announce, checkpoint, execute,
    /// redact, answer.
    ///
    /// It takes `&self`, and that is the whole reason concurrency is possible
    /// at all — everything in `run_one_tool` that needs `&mut self` (recording
    /// a session permission grant, rewriting the checklist) happens strictly
    /// before this point, and none of it applies to a `ReadOnly` call.
    async fn dispatch_tool(
        &self,
        id: &str,
        name: &str,
        input: serde_json::Value,
        class: PermissionClass,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> ToolResult {
        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Working));
        let _ = events.send(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: name.to_string(),
            input: input.clone(),
        });
        // The one place a tool learns which call it is: the context the agent
        // holds is session-long and call-agnostic, so the id and its progress
        // channel are stamped onto a per-dispatch clone.
        let ctx = self
            .tool_ctx
            .with_progress(ProgressReporter::new(id, events.clone()));

        // Here and nowhere else. This is the single point every tool call
        // funnels through, it is *after* the plan gate and the permission
        // prompt (a denied call returns above, so it never leaves an object
        // behind), and it is immediately before dispatch — the last instant
        // the old bytes still exist. Putting the hook inside `write_file` and
        // `edit_file` instead would have duplicated that timing decision in
        // every mutating tool and quietly skipped whichever one is added next.
        let snapshotted = self.checkpoint_before(name, &input, class, &ctx).await;

        let result = self
            .tools
            .execute(name, input.clone(), &ctx, cancel.clone())
            .await;
        self.checkpoint_after(&snapshotted).await;

        // `PostToolUse` lives here rather than in `run_one_tool` because this
        // is the point both authorization paths funnel through, and because it
        // is the only place a result exists at all. It runs before the
        // `ToolCallResult` event below so the card the user reads and the
        // string the model reads are the same string.
        //
        // The three tools intercepted in `run_one_tool` never reach here and
        // so observe `PreToolUse` only. That asymmetry is deliberate and
        // documented rather than papered over: their "results" are UI state (a
        // typed answer, a checklist, a child's report) that a post hook's
        // contract — observe what the tool wrote — does not describe.
        let mut result = self
            .post_tool_hooks(id, name, &input, result, events, &cancel)
            .await;

        // The only place raw tool output exists before it fans out to the
        // transcript, the session database and the next provider request.
        // Redacting here covers all three at once — and covers MCP tools for
        // free, which matters because their output is the least trusted of
        // the lot. The gated/denied paths above return strings we wrote
        // ourselves, so they can't carry a secret.
        if !self.redactor.is_empty() {
            result.content = self.redactor.redact(&result.content).into_owned();
        }

        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: result.is_error,
        });
        result
    }

    /// Snapshots whatever this call is about to overwrite, and returns the
    /// paths captured so `checkpoint_after` can revisit them.
    ///
    /// **Nothing in here can fail the tool call.** Every error is reported as
    /// an advisory progress line and swallowed: a user who cannot undo a write
    /// is worse off than before, but a user whose write was *refused* because
    /// we could not prepare to undo it is worse off still — and would have no
    /// way to make progress at all on a read-only `.smith` directory.
    async fn checkpoint_before(
        &self,
        name: &str,
        input: &serde_json::Value,
        class: PermissionClass,
        ctx: &ToolContext,
    ) -> Vec<std::path::PathBuf> {
        let (Some(checkpointer), Some(turn)) = (&self.checkpointer, self.turn_seq) else {
            return Vec::new();
        };
        let session = &self.tool_ctx.session_id;
        let paths = self.tools.snapshot_paths(name, input, ctx);

        // A tool that can change things but won't say what: `run_bash`, and
        // every MCP tool. Recorded so `/rewind` reports the hole rather than
        // implying the turn is fully covered.
        if paths.is_empty() {
            if class != PermissionClass::ReadOnly {
                let _ = checkpointer.note_uncovered(session, turn, name).await;
            }
            return Vec::new();
        }

        let mut captured = Vec::with_capacity(paths.len());
        for path in paths {
            match checkpointer.snapshot_before(session, turn, &path).await {
                Ok(()) => captured.push(path),
                Err(e) => ctx.report_progress(format!(
                    "checkpoint: could not snapshot {} — this write will not be undoable by /rewind ({e})",
                    path.display()
                )),
            }
        }
        captured
    }

    /// Records what the tool left behind, which is the only way `/rewind` can
    /// later tell a hand edit from its own handiwork. Same best-effort rules.
    async fn checkpoint_after(&self, paths: &[std::path::PathBuf]) {
        let (Some(checkpointer), Some(turn)) = (&self.checkpointer, self.turn_seq) else {
            return;
        };
        for path in paths {
            let _ = checkpointer
                .snapshot_after(&self.tool_ctx.session_id, turn, path)
                .await;
        }
    }
}
