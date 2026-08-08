//! `on_agent_event` — the one place an `AgentEvent` becomes UI state.

use smith_core::{AgentEvent, AgentPhase, StopReason};

use super::chatline::{ChatLine, ChatRole};
use super::chrome::Overlay;
use super::labels::{activity_label, group_target, looks_like_approval_request};
use super::modal::{ActivityStatus, Modal, ModelPicker, PermissionModal, PlanModal, QuestionModal};
use super::App;

impl App {
    pub fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantTextDelta(delta) => {
                // First delta of a stream — end any in-flight thinking gap.
                if self.metrics.stream_started_at.is_none() {
                    self.end_thinking();
                }
                self.metrics.note_delta(delta.chars().count() as u32);
                self.in_flight_text
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            AgentEvent::AssistantTurnComplete {
                message,
                stop_reason,
            } => {
                self.in_flight_text = None;
                let is_final = stop_reason != StopReason::ToolUse;
                let text = message.text();

                if !text.is_empty() {
                    let meta = if is_final {
                        self.metrics.started_at.map(|t| {
                            let secs = t.elapsed().as_secs_f32();
                            match self.metrics.tokens_per_sec {
                                Some(rate) => format!(
                                    "{} · {} · {:.1}s · {:.0} tok/s",
                                    self.provider_label, self.model_label, secs, rate
                                ),
                                None => format!(
                                    "{} · {} · {:.1}s",
                                    self.provider_label, self.model_label, secs
                                ),
                            }
                        })
                    } else {
                        None
                    };
                    let plan_text = text.clone();
                    self.lines
                        .push(ChatLine::new(ChatRole::Assistant, text.clone()).with_meta(meta));

                    if is_final && self.plan_turn_active {
                        self.plan_turn_active = false;
                        self.phase = AgentPhase::Planning;
                        self.modal = Modal::Plan(PlanModal {
                            text: plan_text,
                            scroll: 0,
                        });
                    } else if is_final
                        && matches!(self.phase, AgentPhase::Building)
                        && looks_like_approval_request(&text)
                    {
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            "hint: the plan is already approved — call tools to implement it (do not ask for approval in chat)",
                        ));
                    }
                } else if is_final {
                    // The model replied with neither text nor a tool call —
                    // without this, the turn just silently drops back to
                    // idle and it looks like the app hung.
                    self.plan_turn_active = false;
                    let hint = if self.plan_gated {
                        "model returned no output for this turn — plan mode is still active; run /plan reject to exit, or try again"
                    } else {
                        "model returned no output for this turn"
                    };
                    self.lines.push(ChatLine::new(ChatRole::System, hint));
                }

                if is_final {
                    self.metrics.end_stream();
                    if self.loop_active {
                        // Stay busy across iterations — LoopFinished resets
                        // waiting_on_assistant/phase once the whole run ends,
                        // so Esc keeps working in the gap between rounds.
                    } else {
                        self.waiting_on_assistant = false;
                        self.metrics.started_at = None;
                        if self.modal.is_none() {
                            self.phase = AgentPhase::Idle;
                        }
                    }
                } else {
                    // Next provider round starts a fresh stream clock.
                    self.metrics.end_stream();
                    // Model is about to call tools — start thinking timer.
                    self.begin_thinking();
                }
            }
            AgentEvent::ToolCallStarted {
                id,
                tool_name,
                input,
            } => {
                let label = activity_label(&tool_name, &input);
                // ask_user gets its own modal + transcript record (see
                // record_question_answer); write_tasks gets the dedicated
                // checklist panel — neither needs the generic tool-call line.
                if tool_name != "ask_user" && tool_name != "write_tasks" {
                    self.phase = AgentPhase::Working;
                    // Calls of one activity fold into a single card rather
                    // than stacking — `join_groupable_card` owns what keeps a
                    // run open and what closes it.
                    match self.join_groupable_card(&tool_name) {
                        Some(index) => {
                            // The child's row carries only its target: the
                            // header above it already says the activity, and
                            // repeating "Searching the web…" per row is the
                            // noise this exists to remove.
                            self.lines[index].group(id.clone(), group_target(&tool_name, &input));
                        }
                        None => {
                            // Permanent transcript record — the tool card
                            // replaces the old activity strip, so this line is
                            // all we need.
                            self.lines.push(ChatLine::tool(
                                id.clone(),
                                tool_name.clone(),
                                label,
                                input,
                            ));
                        }
                    }
                }
                // End any in-flight thinking gap — the model is acting now.
                self.end_thinking();
                self.tool_call_count += 1;
            }
            AgentEvent::ToolCallResult {
                id,
                output,
                is_error,
            } => {
                let status = if is_error {
                    ActivityStatus::Error
                } else {
                    ActivityStatus::Done
                };
                if let Some(line) = self
                    .lines
                    .iter_mut()
                    .find(|l| l.tool_id() == Some(id.as_str()))
                {
                    line.finish_tool(status, output.clone());
                } else {
                    // Not a card of its own: it was folded into one, so the id
                    // belongs to a child. Searched second because the common
                    // case is the first branch.
                    for line in self.lines.iter_mut() {
                        if line.finish_grouped(&id, status) {
                            break;
                        }
                    }
                }
                // Model starts thinking again after a result — but only once
                // the *last* result of a concurrent round has landed. A round
                // of ReadOnly calls runs in parallel and finishes out of
                // order, so starting the clock on the first one home would
                // report the slowest call's remaining runtime as time the
                // model spent thinking.
                if !self.lines.iter().any(|l| l.is_running_tool()) {
                    self.begin_thinking();
                }
            }
            // A long `run_bash` is otherwise indistinguishable from a hang:
            // the card sat on its spinner for the whole build with nothing to
            // show. Only the newest line is kept — see `set_progress`.
            AgentEvent::ToolProgress { id, line } => {
                if let Some(entry) = self
                    .lines
                    .iter_mut()
                    .rev()
                    .find(|l| l.tool_id() == Some(id.as_str()))
                {
                    entry.set_progress(line);
                }
            }
            AgentEvent::PermissionPromptNeeded(request) => {
                self.phase = AgentPhase::WaitingPermission;
                self.modal = Modal::Permission(PermissionModal { request, scroll: 0 });
            }
            AgentEvent::UserQuestionNeeded(question) => {
                self.phase = AgentPhase::Asking;
                self.modal = Modal::Question(QuestionModal {
                    question,
                    selected: 0,
                    custom: String::new(),
                });
            }
            // The broker resolved an ask — possibly from another frontend.
            // When this TUI answered, its modal is already gone (the key
            // handler closed it), so these arms are about the *other* case:
            // a web approval must dismiss the stale modal here, and say in
            // the transcript who decided, because a permission that resolves
            // itself with no visible cause reads as a bug.
            AgentEvent::PermissionResolved {
                tool_call_id,
                decision,
                source,
            } => {
                let stale = self
                    .modal
                    .permission()
                    .is_some_and(|m| m.request.tool_call_id == tool_call_id);
                if stale {
                    let tool = self
                        .modal
                        .permission()
                        .map(|m| m.request.tool_name.clone())
                        .unwrap_or_default();
                    self.modal = Modal::None;
                    if source == smith_core::AskSource::Web {
                        let verdict = match decision {
                            smith_core::PermissionDecision::AllowOnce => "allowed once",
                            smith_core::PermissionDecision::AllowSession => {
                                "allowed for the session"
                            }
                            smith_core::PermissionDecision::Deny => "denied",
                        };
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            format!("{tool}: {verdict} from the web console"),
                        ));
                    }
                }
            }
            AgentEvent::QuestionResolved { id, source } => {
                let stale = self.modal.question().is_some_and(|m| m.question.id == id);
                if stale {
                    self.modal = Modal::None;
                    if source == smith_core::AskSource::Web {
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            "question answered from the web console".to_string(),
                        ));
                    }
                }
            }
            AgentEvent::PhaseChanged(phase) => {
                // Don't clobber Asking/WaitingPermission while a modal is open.
                if self.modal.is_question() && phase != AgentPhase::Asking {
                    // keep Asking
                } else if self.modal.permission().is_some()
                    && phase != AgentPhase::WaitingPermission
                {
                    // keep WaitingPermission
                } else if self.modal.is_plan() && phase == AgentPhase::Idle {
                    self.phase = AgentPhase::Planning;
                } else if matches!(self.phase, AgentPhase::Building)
                    && matches!(phase, AgentPhase::Thinking)
                {
                    // Keep "building…" chrome during the post-approve model round.
                } else if matches!(self.phase, AgentPhase::Planning)
                    && matches!(phase, AgentPhase::Thinking)
                    && (self.plan_turn_active || self.plan_gated)
                {
                    // Keep "planning…" during a /plan turn.
                } else if self.loop_active
                    && matches!(phase, AgentPhase::Thinking | AgentPhase::Idle)
                {
                    // Keep "looping…" chrome between/within iterations; Working
                    // (tool calls) and permission/question phases still show
                    // through normally.
                } else {
                    self.phase = phase;
                }
            }
            AgentEvent::TokenUsage(usage) => {
                self.usage.input_tokens += usage.input_tokens;
                self.usage.output_tokens += usage.output_tokens;
                if usage.output_tokens > 0 {
                    let started = self.metrics.stream_started_at.or(self.metrics.started_at);
                    if let Some(started) = started {
                        let elapsed = started.elapsed().as_secs_f32().max(0.05);
                        self.metrics.tokens_per_sec = Some(usage.output_tokens as f32 / elapsed);
                    }
                }
            }
            AgentEvent::ContextUsage {
                used,
                window,
                estimated,
            } => {
                // Kept as the agent reported it, not folded into `usage`:
                // `usage` is the session's cumulative token spend, while this
                // is the occupancy of a single window that compaction resets.
                // Adding one to the other would be meaningless.
                self.context = Some((used, window, estimated));
            }
            AgentEvent::UserInterjected(text) => {
                // It is part of the conversation now, so it becomes a real
                // user bubble and leaves the pending list.
                if let Some(at) = self.queued.iter().position(|q| *q == text) {
                    self.queued.remove(at);
                }
                self.lines.push(ChatLine::new(ChatRole::User, text));
                self.request_count += 1;
            }
            AgentEvent::SessionCost {
                usd,
                unpriced_turns,
            } => {
                self.session_cost = Some((usd, unpriced_turns));
            }
            AgentEvent::ResourceUsage(stats) => {
                self.resources = Some(stats);
            }
            AgentEvent::McpStatus(status) => {
                if status.servers.is_empty() {
                    // A panel whose only row says "nothing here" is worse than
                    // a line saying the same thing — `McpStatus::lines` already
                    // phrases it as the actionable hint it should be.
                    for line in status.lines() {
                        self.lines.push(ChatLine::new(ChatRole::System, line));
                    }
                } else {
                    let rows: Vec<Vec<String>> = status
                        .servers
                        .iter()
                        .map(|s| {
                            vec![
                                s.name.clone(),
                                s.transport.to_string(),
                                s.health.as_str().to_string(),
                                format!("{}/{}/{}", s.tools, s.resources, s.prompts),
                                s.detail.clone().unwrap_or_default(),
                            ]
                        })
                        .collect();
                    self.overlay = Some(
                        Overlay::table(
                            "MCP servers",
                            &["server", "transport", "health", "t/r/p", "detail"],
                            &[24, 14, 12, 12, 38],
                            rows,
                        )
                        .with_footer(vec![
                            "t/r/p = tools / resources / prompts  ·  Esc closes".to_string(),
                        ]),
                    );
                }
            }
            AgentEvent::PlanGateChanged { gated } => {
                // The command that triggered this (`/plan <task>` sets it,
                // `/plan approve`/`/plan reject` clear it) already pushed its
                // own confirmation line locally — this just keeps state in
                // sync with the orchestrator's authoritative Agent.
                self.plan_gated = gated;
            }
            AgentEvent::GoalChanged(goal) => {
                self.goal = goal.clone();
                match goal {
                    // Says what it did *and did not* do. A goal rides every
                    // later request's system prompt; it starts no turn of its
                    // own, so `/goal ship the login page` on its own looks
                    // exactly like a command that silently failed. Naming the
                    // one command that acts on it is the difference between a
                    // confusing no-op and a documented one.
                    Some(text) => {
                        self.lines
                            .push(ChatLine::new(ChatRole::System, format!("goal set: {text}")));
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            "it rides every request from here — say what to do next, \
                             or /loop goal to work on it now",
                        ));
                    }
                    None => self
                        .lines
                        .push(ChatLine::new(ChatRole::System, "goal cleared")),
                }
            }
            AgentEvent::LoopIterationStarted {
                iteration,
                max_iterations,
            } => {
                self.loop_progress = Some((iteration, max_iterations));
                self.phase = AgentPhase::Looping;
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("— loop iteration {iteration}/{max_iterations} —"),
                ));
            }
            AgentEvent::LoopFinished { reason, iterations } => {
                self.loop_active = false;
                self.loop_progress = None;
                self.waiting_on_assistant = false;
                self.metrics.clear();
                if self.modal.is_none() {
                    self.phase = AgentPhase::Idle;
                }
                let summary = match reason {
                    smith_core::LoopStopReason::Done => {
                        format!(
                            "loop finished — agent declared done after {iterations} iteration(s)"
                        )
                    }
                    smith_core::LoopStopReason::MaxIterations => {
                        format!("loop stopped — reached the iteration limit ({iterations})")
                    }
                    smith_core::LoopStopReason::Cancelled => {
                        format!("loop cancelled after {iterations} iteration(s)")
                    }
                    smith_core::LoopStopReason::Failed => {
                        format!("loop stopped — a turn failed after {iterations} iteration(s) (see the error above)")
                    }
                };
                self.lines.push(ChatLine::new(ChatRole::System, summary));
            }
            AgentEvent::Rewind(report) => {
                // Rendered by `RewindReport::lines`, not here: the wording of
                // the run_bash caveat and the conflict list is a safety
                // property, and a second copy in the TUI would drift from the
                // one `stream-json` consumers see.
                for line in report.lines() {
                    self.lines.push(ChatLine::new(ChatRole::System, line));
                }
            }
            AgentEvent::TasksUpdated(tasks) => {
                self.tasks = tasks;
            }
            AgentEvent::ModelsAvailable { provider, models } => {
                if models.is_empty() {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        format!(
                            "could not read {provider}'s model list — switch by name: \
                             /model <name> [--save]"
                        ),
                    ));
                    return;
                }
                let selected = models
                    .iter()
                    .position(|m| m.id == self.model_label)
                    .unwrap_or(0);
                self.modal = Modal::Model(ModelPicker {
                    provider,
                    all: models,
                    filter: String::new(),
                    selected,
                    scroll: 0,
                });
            }
            AgentEvent::ModelChanged {
                provider,
                model,
                saved,
            } => {
                if provider != self.provider_label {
                    // Stale local-machine stats from the old provider would
                    // otherwise linger in the sidebar after switching away
                    // from Ollama.
                    self.resources = None;
                }
                self.provider_label = provider.clone();
                self.model_label = model.clone();
                let suffix = if saved { " (saved as default)" } else { "" };
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("switched to {provider}/{model}{suffix}"),
                ));
            }
            AgentEvent::PermissionPolicyChanged { policy, saved } => {
                self.permission_policy = policy;
                let suffix = if saved { " (saved as default)" } else { "" };
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("permission mode: {}{suffix}", policy.as_str()),
                ));
            }
            AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                delay_ms,
                reason,
            } => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!(
                        "{reason} — retrying in {:.1}s (attempt {attempt}/{max_attempts})",
                        delay_ms as f64 / 1000.0
                    ),
                ));
            }
            AgentEvent::TurnLimitReached { detail, .. } => {
                // Same state reset as the Error arm below: the turn is over,
                // so anything still marked in-flight would spin forever.
                self.waiting_on_assistant = false;
                self.in_flight_text = None;
                self.metrics.clear();
                self.plan_turn_active = false;
                self.phase = AgentPhase::Idle;
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("turn stopped: it {detail} — send \"continue\" to keep going"),
                ));
            }
            AgentEvent::Error(err) => {
                self.waiting_on_assistant = false;
                self.in_flight_text = None;
                self.metrics.clear();
                self.plan_turn_active = false;
                self.phase = AgentPhase::Idle;
                // Don't leave any in-flight tool line spinning forever in
                // the transcript if the turn errored out mid-call.
                for line in self.lines.iter_mut() {
                    line.fail_if_running();
                }
                self.lines
                    .push(ChatLine::new(ChatRole::System, format!("error: {err}")));
            }
        }
    }
}
