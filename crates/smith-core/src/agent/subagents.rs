//! Delegating a bounded piece of work to a child `Agent`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, AgentPhase};
use crate::subagent::{self, SubagentDefinition};
use crate::tool::{PermissionPolicy, ToolResult};

use super::limits::TurnLimits;
use super::Agent;

/// Turns what the relay collected into the single `tool_result` the parent's
/// model reads.
///
/// A partial report is still returned, with a note. A child stopped by its
/// budget has usually done most of the work, and throwing that away to return
/// a bare error would make the parent re-delegate the same task from scratch —
/// paying twice for the half it already has. The note is what stops the parent
/// mistaking a partial answer for a complete one.
pub(crate) fn finish_subagent(report: subagent::ChildReport, cancelled: bool) -> ToolResult {
    let body = report.report.trim().to_string();
    if body.is_empty() {
        let reason = if cancelled || report.error.as_deref() == Some("cancelled") {
            "the subagent was cancelled before it reported anything".to_string()
        } else if let Some(limit) = &report.limit {
            format!("the subagent {limit} before reporting anything")
        } else if let Some(error) = &report.error {
            format!("the subagent failed: {error}")
        } else {
            "the subagent returned no report at all".to_string()
        };
        return ToolResult::error(reason);
    }

    let mut note = None;
    if cancelled || report.error.as_deref() == Some("cancelled") {
        note = Some("it was cancelled by the user".to_string());
    } else if let Some(limit) = &report.limit {
        note = Some(format!("it {limit}"));
    } else if let Some(error) = &report.error {
        note = Some(format!("it stopped on an error: {error}"));
    }

    match note {
        None => ToolResult::ok(body),
        Some(note) => ToolResult::ok(format!(
            "{body}\n\n[This report is partial — {note}. Treat anything it does not mention as \
             unchecked rather than absent.]"
        )),
    }
}

impl Agent {
    /// Wall clock the turn in flight still has, for a child to be capped by.
    fn remaining_wall_clock(&self) -> Duration {
        match self.turn_deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            // No turn in flight (a direct `run_one_tool` in a test): the
            // child gets its own cap and nothing more.
            None => subagent::MAX_WALL_CLOCK,
        }
    }

    /// Runs one subagent to completion and answers the parent's `tool_use`
    /// with its final report — and with nothing else it did.
    ///
    /// The child is a full `Agent` built here from this one's provider, tool
    /// registry, context and redactor, but with its *own* history, its own
    /// system prompt, a read-only slice of the tools, its own much smaller
    /// budget, and private permission/question channels that refuse. It shares
    /// nothing mutable with the parent: when it returns, everything it read is
    /// dropped and only `ChildReport::report` survives.
    ///
    /// Returns a `BoxFuture` rather than being an `async fn`, and that is not
    /// style: `run_turn` → `run_one_tool` → `run_task` → `run_turn` is a real
    /// cycle, and an `async fn`'s future is an anonymous type whose `Send`-ness
    /// is *inferred* — so the compiler would have to already know the answer
    /// to work it out. Naming the type here asserts `Send` at one point in the
    /// cycle and breaks it, and the `Box` is what gives the recursive future a
    /// finite size.
    pub(super) fn run_task<'a>(
        &'a mut self,
        id: &'a str,
        input: serde_json::Value,
        events: &'a mpsc::UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Working));
            let _ = events.send(AgentEvent::ToolCallStarted {
                id: id.to_string(),
                tool_name: subagent::TASK_TOOL.to_string(),
                input: input.clone(),
            });
            let answer = |result: ToolResult| {
                let _ = events.send(AgentEvent::ToolCallResult {
                    id: id.to_string(),
                    output: result.content.clone(),
                    is_error: result.is_error,
                });
                result
            };

            let str_arg = |key: &str| {
                input
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            let prompt = str_arg("prompt");
            if prompt.is_empty() {
                return answer(ToolResult::error(
                "task requires a non-empty `prompt`: the subagent sees nothing but this string, \
                 so it must state the whole task and what to report back.",
            ));
            }

            // Checked before anything is built. A child cannot reach this branch
            // through its own tool list (`task` is never in it), but the JSON
            // envelope fallback resolves names against the registry rather than
            // the visible set, so the depth limit is enforced here too.
            if self.subagent_depth >= subagent::MAX_DEPTH {
                return answer(ToolResult::error(
                "You are already a subagent, and subagents cannot delegate further. Do this part \
                 of the work yourself, or report what is still needed so the agent that called \
                 you can delegate it.",
            ));
            }

            let requested = str_arg("subagent_type");
            let def = if requested.is_empty() || requested == subagent::GENERAL_PURPOSE {
                SubagentDefinition::general_purpose()
            } else {
                match self
                    .subagent_definitions
                    .iter()
                    .find(|d| d.name == requested)
                {
                    Some(def) => def.clone(),
                    None => {
                        let mut known = vec![subagent::GENERAL_PURPOSE.to_string()];
                        known.extend(self.subagent_definitions.iter().map(|d| d.name.clone()));
                        return answer(ToolResult::error(format!(
                            "no subagent named `{requested}` is configured. Available: {}.",
                            known.join(", ")
                        )));
                    }
                }
            };

            if self.subagent_tool_budget == 0 {
                return answer(ToolResult::error(
                "not executed — this turn has already spent its whole subagent tool-call budget. \
                 Do the remaining work directly, or finish the turn and let the user continue.",
            ));
            }

            let (allowed, refused) =
                subagent::resolve_tool_set(self.tools.as_ref(), def.tools.as_deref());
            if allowed.is_empty() {
                return answer(ToolResult::error(
                    "a subagent would have no tools at all (nothing read-only is registered), so \
                 delegating this can only cost tokens. Do it directly.",
                ));
            }
            for name in &refused {
                // Advisory, not fatal: the child still runs with what it may have.
                // Silence here would leave a definition quietly doing less than it
                // says for the rest of the session.
                let _ = events.send(AgentEvent::ToolProgress {
                    id: id.to_string(),
                    line: format!(
                        "{}: `{name}` was not granted — subagents only get read-only tools",
                        def.name
                    ),
                });
            }

            let limits = TurnLimits {
                max_turns: subagent::MAX_ROUNDS,
                // Whichever is smaller: this child's own cap, or everything the
                // turn has left to give away.
                max_tool_calls_per_turn: subagent::MAX_TOOL_CALLS.min(self.subagent_tool_budget),
                // A child cannot outlive the turn that is waiting on it.
                max_wall_clock: subagent::MAX_WALL_CLOCK.min(self.remaining_wall_clock()),
            };
            let _ = events.send(AgentEvent::ToolProgress {
                id: id.to_string(),
                line: format!(
                    "{}: started — {} tools, up to {} calls",
                    def.name,
                    allowed.len(),
                    limits.max_tool_calls_per_turn
                ),
            });

            let mut child = Agent::new(
                self.provider.clone(),
                Arc::new(subagent::RestrictedTools::new(
                    self.tools.clone(),
                    allowed.clone(),
                )),
                def.model.clone().unwrap_or_else(|| self.model.clone()),
                // The same session on disk — staging, scratch and checkpoints
                // all stay where the parent's `/rewind` can find them — but a
                // *different reader*. A subagent has its own conversation and
                // its own context window, so a file it read is a file the
                // parent has still never seen. Sharing the read set let a
                // delegated `read_file` satisfy the parent's overwrite guard,
                // which is exactly the guard's job to prevent.
                self.tool_ctx.for_delegate(&format!("task.{}", id)),
            )
            .with_system(subagent::child_system_prompt(&def, &allowed))
            .with_limits(limits)
            .with_retry_policy(self.retry_policy)
            .with_max_tokens(self.max_tokens)
            // The report lands in the parent's history and the parent's
            // transcript, so it goes through the same secret filter everything
            // else does.
            .with_redactor(self.redactor.clone())
            // Never inherited from the parent, even when the parent is running
            // under `skip`. A policy the user set for calls they can see is not
            // consent for calls they cannot.
            .with_permission_policy(PermissionPolicy::Ask);
            child.subagent_depth = self.subagent_depth + 1;
            child.sleeper = self.sleeper.clone();
            child.context_provider = self.context_provider.clone();
            child.compaction = self.compaction;
            // Belt and braces: the child's tools are read-only, so the gate has
            // nothing to block, but an unapproved plan must not become a hole the
            // moment delegation is involved.
            child.plan_gated = self.plan_gated;
            // Hooks *are* inherited, unlike the permission policy right above
            // — and the two are not in tension, because they point the same
            // way. A policy is authority the user granted and a child does not
            // get to inherit; a hook is authority the user withheld, and a
            // child is the last place to relax it. A child's calls are the
            // least-watched calls in the system (one summarised progress line
            // on a card), so if any calls need a policy hook, those do.
            //
            // The surprise — a hook the user wrote for their own calls firing
            // on a child's — is real, and is why the payload carries
            // `"agent": "subagent"` and `depth`: filtering it back out is one
            // line in the hook. The opposite default cannot be filtered back
            // *in*, because a hook that never runs cannot ask to.
            child.hooks = self.hooks.clone();
            // Deliberately no checkpointer: a read-only agent has nothing to
            // snapshot, and allocating a second turn sequence inside the parent's
            // turn would split one user action across two manifests.

            let (child_tx, child_rx) = mpsc::unbounded_channel();
            let (permission_tx, permission_rx) = mpsc::unbounded_channel();
            let (question_tx, question_rx) = mpsc::unbounded_channel();
            tokio::spawn(subagent::refuse_permissions(permission_rx));
            tokio::spawn(subagent::refuse_questions(question_rx));

            // A child of the parent's token: Esc reaches the child immediately,
            // through every layer it is blocked in — and this function still
            // returns a real `ToolResult`, so the parent's `tool_use` gets its
            // `tool_result` exactly as it does for any other tool.
            let child_cancel = cancel.child_token();
            // `run_turn` takes the sender by value and holds the only clones, so
            // the relay's `recv` ends on its own the moment the child is done —
            // no sentinel, no timeout, no chance of the relay outliving it.
            let running = child.run_turn(
                prompt,
                child_tx,
                permission_tx,
                question_tx,
                child_cancel.clone(),
            );
            let watching = subagent::relay_child(&def.name, id, child_rx, events);
            let (_completed, report) = futures::future::join(running, watching).await;

            self.subagent_tool_budget = self.subagent_tool_budget.saturating_sub(report.tool_calls);
            // Billed to this turn, on the child's model. `TurnAccounting` carries
            // one provider/model pair, so a child on a different model is folded
            // in at the parent's row — the tokens are real either way, and
            // dropping them would under-report a session's true cost.
            self.note_side_request_usage(child.session_usage(), &child.model.clone());
            let _ = events.send(AgentEvent::ToolProgress {
                id: id.to_string(),
                line: format!(
                    "{}: finished after {} tool calls",
                    def.name, report.tool_calls
                ),
            });

            answer(finish_subagent(report, cancel.is_cancelled()))
        })
    }
}
