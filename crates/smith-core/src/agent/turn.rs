//! `Agent::run_turn` — the loop at the centre of the system.

use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, AgentPhase, TurnLimitKind};
use crate::message::{CompletionRequest, ContentBlock, Message, Role, StopReason};

use super::executor::{PermissionAsk, QuestionAsk};
use super::limits::limit_note;
use super::stream::{consume_stream, StreamOutcome};
use super::Agent;

/// Stand-in result recorded for a tool call the turn never got to run. The
/// model reads it, so it says plainly that nothing happened — an empty or
/// vague result would invite it to assume the call succeeded.
const NOT_EXECUTED_CANCELLED: &str = "not executed — the turn was cancelled by the user";

/// The same idea for a call the turn had no budget left to run.
const NOT_EXECUTED_TOOL_BUDGET: &str =
    "not executed — this turn reached its tool-call budget before this call";

/// What one entry of a round's execution plan does. The plan is built in the
/// model's own order before anything runs, so neither the tool-call budget
/// nor the grouping can depend on which call happens to finish first.
enum ToolStep {
    /// A maximal contiguous run of concurrency-safe calls, by slot index.
    Concurrent(Vec<usize>),
    /// One call that must run on its own, with `&mut self` available.
    Serial(usize),
    /// A call the turn had no budget left for.
    OverBudget(usize),
}

impl Agent {
    /// Runs one full user turn to completion: sends the user's message, streams
    /// the reply, and if the model asks for tools, executes them (round-tripping
    /// through `permission_tx` for anything above ReadOnly) until the model
    /// produces a final end-turn response.
    /// Returns `true` if the turn ran to a normal completion, `false` if it
    /// ended early via cancellation or a provider/stream error (an `Error`
    /// event has already been sent either way) — used by the `/loop` driver
    /// to tell "stopped cleanly" apart from "stopped because of a failure".
    pub async fn run_turn(
        &mut self,
        user_text: String,
        events: mpsc::UnboundedSender<AgentEvent>,
        permission_tx: mpsc::UnboundedSender<PermissionAsk>,
        question_tx: mpsc::UnboundedSender<QuestionAsk>,
        cancel: CancellationToken,
    ) -> bool {
        // Before the checkpoint is opened and before anything is pushed to
        // history: a turn a hook refuses must leave no trace at all, and an
        // allocated turn sequence with nothing in it would show up in
        // `/rewind` as an undo that undoes nothing.
        //
        // Only for the agent the user is actually talking to. A subagent's
        // "prompt" is written by the parent *model*; firing an event called
        // `UserPromptSubmit` on it would misreport who said it, and a hook
        // that redacts what the user typed has nothing to do there.
        let user_text = if self.subagent_depth == 0 {
            let outcome = self
                .hooks
                .user_prompt_submit(&self.hook_ctx(), user_text, &cancel)
                .await;
            for notice in &outcome.notices {
                let _ = events.send(AgentEvent::Error(notice.clone()));
            }
            if let Some(denial) = outcome.denial {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::Error(denial));
                return false;
            }
            outcome.prompt
        } else {
            user_text
        };

        // Allocated before anything can run a tool, and held for the whole
        // turn: every file this turn overwrites lands in the same checkpoint,
        // which is what makes "undo that turn" a single operation.
        self.turn_seq = match &self.checkpointer {
            Some(checkpointer) => Some(checkpointer.begin_turn(&self.tool_ctx.session_id).await),
            None => None,
        };

        let user_text = if self.pending_notes.is_empty() {
            user_text
        } else {
            let notes = std::mem::take(&mut self.pending_notes).join("\n");
            format!("{notes}\n\n{user_text}")
        };
        self.messages.push(Message::user_text(user_text));
        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Thinking));
        // Fresh accounting per turn — the caller persists exactly one `turns`
        // row from this, and a leftover from the previous turn would be
        // recorded twice.
        self.last_turn = None;

        // Some providers (Ollama cloud models especially) occasionally end a
        // turn with no text and no tool call right after a tool round — the
        // model just stops instead of writing up the results. Retrying the
        // exact same request a couple of times resolves it far more often
        // than not, and is exactly what a user does by nudging the model
        // ("did you finish?") — do that automatically before giving up.
        const MAX_EMPTY_RETRIES: u32 = 2;
        let mut empty_retries: u32 = 0;

        // Turn budget. Measured from here rather than from the first request
        // so that time spent in permission prompts and backoff sleeps counts
        // too — from the user's side that is all the same wait.
        let started_at = Instant::now();
        // Published on `self` so a subagent spawned mid-turn can be handed the
        // time this turn actually has left instead of a fresh allowance.
        self.turn_deadline = Some(started_at + self.limits.max_wall_clock);
        // Refilled per turn, exactly like `tool_calls` below: the pool bounds
        // one turn's delegation, not the session's.
        self.subagent_tool_budget = self.limits.max_tool_calls_per_turn;
        let mut rounds: u32 = 0;
        let mut tool_calls: u32 = 0;
        // Context size immediately after the last compaction this turn, if
        // there was one — see the guard at the top of the loop.
        let mut compacted_at: Option<u32> = None;

        loop {
            if cancel.is_cancelled() {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::Error("cancelled".into()));
                return false;
            }

            // Anything the user typed since the last round joins the
            // conversation here, as an ordinary user message.
            //
            // At a round boundary specifically: the messages list has just
            // been left consistent (assistant message pushed, every `tool_use`
            // answered), so inserting one cannot strand a tool call. Injecting
            // mid-stream would.
            //
            // It is a plain user message, with no wrapper telling the model
            // what to do with it. Whether "also handle the errors" changes the
            // job in flight or adds a new one is exactly the judgement a model
            // is for, and a framing that pre-decided it would be wrong half
            // the time — the user knows which they meant and wrote it that way.
            for text in self.take_interjections() {
                let _ = events.send(AgentEvent::UserInterjected(text.clone()));
                self.messages.push(Message::user_text(text));
            }

            // Checked here, at a round boundary, because that is the only
            // place history is guaranteed well-formed: every `tool_use` from
            // the previous round already has its matching `tool_result`
            // pushed. A failed compaction leaves history exactly as it was and
            // the turn proceeds normally — see `compact`.
            //
            // The `compacted_at` guard stops a turn compacting the compaction:
            // if a window is small enough that even the carried-over state sits
            // above the threshold, an unguarded check would fire again on the
            // very next round and this time throw the carry-over away. Growth
            // past the post-compaction level is the right condition rather than
            // "once per turn", because a tool-heavy turn genuinely can refill
            // the window and legitimately needs compacting twice.
            if self.should_compact()
                && compacted_at.is_none_or(|after| self.context_usage().used > after)
            {
                if let Ok(outcome) = self.compact(&events, &cancel).await {
                    compacted_at = Some(outcome.tokens_after);
                }
            }

            let request = CompletionRequest {
                model: self.model.clone(),
                system: self.effective_system(),
                messages: self.messages.clone(),
                tools: self.tools.tool_defs(),
                max_tokens: self.max_tokens,
                temperature: None,
            };

            let stream = match self.stream_with_retry(request, &events, &cancel).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                    let _ = events.send(AgentEvent::Error(e.to_string()));
                    return false;
                }
            };

            let outcome = match consume_stream(stream, &events, cancel.clone()).await {
                Ok(v) => v,
                Err(e) => {
                    let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                    let _ = events.send(AgentEvent::Error(e));
                    return false;
                }
            };
            let StreamOutcome {
                message: mut assistant_message,
                mut stop_reason,
                usage,
                reasoning_tags_stripped,
            } = outcome;
            self.reasoning_tags_stripped += reasoning_tags_stripped;

            // Some local models (small/quantized ones especially) don't
            // reliably use the provider's structured tool-calling channel —
            // they print the call as plain JSON text instead, which would
            // otherwise just sit there as a dead assistant message. Recover
            // it into a real tool call so it actually runs.
            if stop_reason == StopReason::EndTurn {
                if let Some(recovered) = self.recover_text_tool_call(&assistant_message) {
                    assistant_message = recovered;
                    stop_reason = StopReason::ToolUse;
                }
            }

            // A genuinely empty, non-tool-use turn is usually the provider
            // stalling rather than the model deliberately having nothing to
            // say — retry in place (nothing was pushed to history yet, so
            // this re-sends the identical request) before surfacing it to
            // the user as "no output".
            if stop_reason == StopReason::EndTurn
                && assistant_message.content.is_empty()
                && empty_retries < MAX_EMPTY_RETRIES
            {
                empty_retries += 1;
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Thinking));
                continue;
            }

            // A cancelled stream leaves a half-built message: any `ToolUse`
            // block in it was never dispatched, and its input JSON may have
            // stopped mid-token. Drop those blocks before the message reaches
            // history — a `tool_use` with no matching `tool_result` makes the
            // *next* request fail outright, so an interrupted turn would
            // otherwise poison the whole session. The text is kept: it's what
            // the model managed to say, and it's still useful context.
            if stop_reason == StopReason::Cancelled {
                assistant_message
                    .content
                    .retain(|b| !matches!(b, ContentBlock::ToolUse { .. }));
            }

            // A provider can legitimately return a completely empty turn
            // (no text, no tool use) — pushing that into history would
            // serialize as `content: null` on the next request, which
            // OpenAI-compatible endpoints (Ollama included) reject outright.
            // Skipping the push just drops the no-op round from history.
            if !assistant_message.content.is_empty() {
                self.messages.push(assistant_message.clone());
            }
            // After the push, so `counted_messages` marks the exact point up
            // to which the provider's own token count is authoritative.
            self.note_usage(usage);
            let _ = events.send(AgentEvent::SessionCost {
                usd: self.session_cost_usd,
                unpriced_turns: self.unpriced_turns,
            });
            self.emit_context(&events);
            let _ = events.send(AgentEvent::AssistantTurnComplete {
                message: assistant_message.clone(),
                stop_reason,
            });

            if stop_reason != StopReason::ToolUse {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                // Cancellation is not a normal completion — callers use the
                // return value to decide whether to keep driving the loop.
                return stop_reason != StopReason::Cancelled;
            }

            empty_retries = 0;
            rounds += 1;

            let tool_uses: Vec<(String, String, serde_json::Value)> = assistant_message
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            // Every `tool_use` must come back with a matching `tool_result`
            // or the provider rejects the next request. Seeding the answers
            // up front and *filling them in* — rather than appending as we
            // go — makes that invariant hold on every exit path, cancellation
            // included, instead of depending on the loop running to the end.
            let mut results: Vec<ContentBlock> = tool_uses
                .iter()
                .map(|(id, _, _)| ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: NOT_EXECUTED_CANCELLED.to_string(),
                    is_error: true,
                })
                .collect();

            // Grouping is decided here, up front, and preserves the model's
            // order: a maximal *contiguous* run of concurrency-safe calls
            // becomes one concurrent group, and anything else splits the run.
            //
            // The alternative — hoisting every ReadOnly call in the round to
            // the front and running the rest afterwards — would produce one
            // bigger group, but it reorders execution relative to what the
            // model asked for, so a read placed *after* a write in the same
            // round would stop seeing that write. Nothing in the protocol
            // promises a round's calls are independent, and a silently stale
            // read is exactly the class of bug that survives a test suite.
            // The cost of splitting instead is real but small: in
            // `[read, write, read, read]` the first read runs alone. Models
            // batch their reads together, which is the case this optimises.
            let mut steps: Vec<ToolStep> = Vec::new();
            for (slot, (_, name, _)) in tool_uses.iter().enumerate() {
                // The one cap that has to bite mid-round: a single round can
                // ask for more calls than the whole turn has left. Spending
                // the budget here, in order, keeps which calls get refused
                // independent of how long any of them takes to run.
                if tool_calls >= self.limits.max_tool_calls_per_turn {
                    steps.push(ToolStep::OverBudget(slot));
                    continue;
                }
                tool_calls += 1;
                match (self.is_concurrency_safe(name), steps.last_mut()) {
                    (true, Some(ToolStep::Concurrent(group))) => group.push(slot),
                    (true, _) => steps.push(ToolStep::Concurrent(vec![slot])),
                    (false, _) => steps.push(ToolStep::Serial(slot)),
                }
            }

            let mut cancelled = false;
            for step in steps {
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }

                match step {
                    // The seeded slot is overwritten rather than left at the
                    // cancellation wording, so the model isn't told the user
                    // stopped it when the user did nothing of the sort.
                    ToolStep::OverBudget(slot) => {
                        results[slot] = ContentBlock::ToolResult {
                            tool_use_id: tool_uses[slot].0.clone(),
                            content: NOT_EXECUTED_TOOL_BUDGET.to_string(),
                            is_error: true,
                        };
                    }
                    ToolStep::Serial(slot) => {
                        let (id, name, input) = &tool_uses[slot];
                        let result = self
                            .run_one_tool(
                                id,
                                name,
                                input.clone(),
                                &events,
                                &permission_tx,
                                &question_tx,
                                cancel.clone(),
                            )
                            .await;
                        results[slot] = ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: result.content,
                            is_error: result.is_error,
                        };
                    }
                    ToolStep::Concurrent(group) => {
                        self.run_concurrent_group(
                            &group,
                            &tool_uses,
                            &mut results,
                            &events,
                            &cancel,
                        )
                        .await;
                        // A group swallows the cancellation itself (it has to
                        // — the calls already in flight still owe the model a
                        // result), so re-check rather than waiting for the top
                        // of the next iteration to notice.
                        if cancel.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                    }
                }
            }

            // Checked here, with the round's results in hand and before they
            // are pushed, so the explanation rides the same user message the
            // tool results do — one message, one push, invariant preserved on
            // this exit path exactly as on the cancellation one.
            let limit = (!cancelled)
                .then(|| self.limit_reached(rounds, tool_calls, started_at))
                .flatten();
            if let Some(kind) = limit {
                results.push(ContentBlock::Text {
                    text: limit_note(kind, &self.limits),
                });
            }

            self.messages.push(Message {
                role: Role::User,
                content: results,
            });
            // Tool results are the single biggest thing that lands in history
            // between responses, so this is where the gauge actually moves.
            self.emit_context(&events);

            if cancelled {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::Error("cancelled".into()));
                return false;
            }

            if let Some(kind) = limit {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::TurnLimitReached {
                    kind,
                    detail: kind.describe(&self.limits),
                });
                // Not a normal completion: `/loop` must not answer a runaway
                // turn by starting another one.
                return false;
            }

            let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Thinking));
        }
    }

    /// Which cap, if any, this turn has now reached. Called once per completed
    /// round; the order fixes which one is reported when several are hit at
    /// the same moment.
    ///
    /// Every comparison is `>=`, so exhausting a budget exactly stops the
    /// turn. Letting one more round run would buy the model a request it has
    /// no tool calls left to spend — and if it then asked for tools anyway,
    /// every one of them would be refused and we would stop on the next check
    /// regardless.
    fn limit_reached(
        &self,
        rounds: u32,
        tool_calls: u32,
        started_at: Instant,
    ) -> Option<TurnLimitKind> {
        if rounds >= self.limits.max_turns {
            Some(TurnLimitKind::Rounds)
        } else if tool_calls >= self.limits.max_tool_calls_per_turn {
            Some(TurnLimitKind::ToolCalls)
        } else if started_at.elapsed() >= self.limits.max_wall_clock {
            Some(TurnLimitKind::WallClock)
        } else {
            None
        }
    }
}
