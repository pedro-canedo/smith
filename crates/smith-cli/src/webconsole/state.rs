//! The event pump and the projection a late-joining browser renders from.
//!
//! With the console enabled, the orchestrator's event stream no longer goes
//! straight to the TUI — it goes through [`pump`], which does three things
//! per event, in order: applies it to the [`SessionProjection`] (so a browser
//! that connects mid-turn has state to render before its stream begins),
//! forwards it to the TUI's own unbounded channel (the TUI can never lag),
//! and publishes it on a bounded broadcast for however many SSE subscribers
//! exist (zero, usually).
//!
//! With the console off, none of this runs: the receiver goes to the TUI
//! directly and the path is byte-identical to a smith without this module.
//!
//! # The seq number
//!
//! Every event gets a monotonically increasing sequence number, carried as
//! the SSE `id:` and stored on the projection as "state as of event N". That
//! pair is the whole snapshot/stream consistency story: a client fetches
//! `/api/state`, sees `seq`, and discards stream frames with `id <= seq`; on
//! reconnect or on a `gap` frame it refetches the state and repeats. Order
//! is total because one task assigns the numbers.
//!
//! # Lag is visible, never silent
//!
//! The broadcast buffer is bounded ([`EVENT_BUFFER`]). A subscriber that
//! falls behind loses events — that is the design, because the alternative
//! is a slow browser tab backpressuring an agent — but it *finds out*: the
//! SSE handler turns `Lagged` into a synthetic `gap` frame and the client
//! resnapshots. The TUI is structurally exempt; its channel is unbounded.

use std::sync::{Arc, RwLock};

use serde::Serialize;
use smith_core::{
    AgentEvent, AgentPhase, PermissionRequest, StopReason, Task, Usage, UserQuestion,
};
use tokio::sync::{broadcast, mpsc};

/// Events a lagging SSE subscriber can be behind by before it starts losing
/// them and is told to resnapshot. Sized for bursts (a tool-heavy round
/// emits tens of events, not thousands), not for slow readers.
pub const EVENT_BUFFER: usize = 1024;

/// One event, stamped with its position in the session's stream.
pub type StampedEvent = (u64, AgentEvent);

/// The handles the pump writes and the server reads.
#[derive(Clone)]
pub struct Tee {
    pub projection: Arc<RwLock<SessionProjection>>,
    pub broadcast: broadcast::Sender<StampedEvent>,
}

impl Tee {
    pub fn new(session_id: String, provider: String, model: String) -> Self {
        Self {
            projection: Arc::new(RwLock::new(SessionProjection::new(
                session_id, provider, model,
            ))),
            broadcast: broadcast::channel(EVENT_BUFFER).0,
        }
    }
}

/// Moves events from the orchestrator to the TUI and the console until the
/// orchestrator's sender closes.
pub async fn pump(
    mut from_orchestrator: mpsc::UnboundedReceiver<AgentEvent>,
    to_tui: mpsc::UnboundedSender<AgentEvent>,
    tee: Tee,
) {
    let mut seq = 0u64;
    while let Some(event) = from_orchestrator.recv().await {
        seq += 1;
        // The projection lock is sync and never held across an await; a
        // slow HTTP read cannot stall the pump because readers clone out.
        if let Ok(mut projection) = tee.projection.write() {
            projection.apply(seq, &event);
        }
        // TUI first: it is the primary UI, and its channel cannot refuse.
        let _ = to_tui.send(event.clone());
        // `Err` here means no web subscriber right now, which is the normal
        // state — the projection above is what a later one catches up from.
        let _ = tee.broadcast.send((seq, event));
    }
}

/// One item of the transcript, as the console renders it.
///
/// A deliberately *rendered-shape* type rather than raw `Message`s: the
/// browser needs what the TUI's `on_agent_event` computes — closed items in
/// order, tool cards with their progress folded in — not the provider-shaped
/// content blocks history stores.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    System {
        text: String,
    },
    ToolCard {
        id: String,
        tool_name: String,
        input: serde_json::Value,
        /// Newest-last advisory lines from `ToolProgress`.
        progress: Vec<String>,
        output: Option<String>,
        is_error: bool,
        running: bool,
    },
}

/// `App::on_agent_event` without a screen: the accumulated state a browser
/// opening mid-session needs in one fetch.
#[derive(Debug, Clone, Serialize)]
pub struct SessionProjection {
    /// The stream position this state includes. Frames with `id <= seq` are
    /// already reflected here and the client discards them.
    pub seq: u64,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub phase: AgentPhase,
    pub plan_gated: bool,
    pub transcript: Vec<TranscriptItem>,
    /// The open assistant stream, accumulated delta by delta. Empty when no
    /// reply is in flight.
    pub streaming_text: String,
    pub tasks: Vec<Task>,
    pub pending_permission: Option<PermissionRequest>,
    pub pending_question: Option<UserQuestion>,
    pub usage: Usage,
    /// Read from `SessionCost` events, never recomputed — the console has no
    /// pricing table on purpose (`AgentEvent::SessionCost` says why).
    pub cost_usd: f64,
    pub unpriced_turns: u32,
    /// `(used, window, estimated)` context occupancy, when known.
    pub context: Option<(u32, u32, bool)>,
    pub goal: Option<String>,
}

impl SessionProjection {
    pub fn new(session_id: String, provider: String, model: String) -> Self {
        Self {
            seq: 0,
            session_id,
            provider,
            model,
            phase: AgentPhase::Idle,
            plan_gated: false,
            transcript: Vec::new(),
            streaming_text: String::new(),
            tasks: Vec::new(),
            pending_permission: None,
            pending_question: None,
            usage: Usage::default(),
            cost_usd: 0.0,
            unpriced_turns: 0,
            context: None,
            goal: None,
        }
    }

    /// Folds one event in. The arm set deliberately mirrors
    /// `smith_tui::app::events` — where the TUI keeps a modal, this keeps a
    /// `pending_*` field; where the TUI pushes a `ChatLine`, this pushes a
    /// `TranscriptItem`.
    pub fn apply(&mut self, seq: u64, event: &AgentEvent) {
        self.seq = seq;
        match event {
            AgentEvent::AssistantTextDelta(delta) => {
                self.streaming_text.push_str(delta);
            }
            AgentEvent::AssistantTurnComplete {
                message,
                stop_reason,
            } => {
                self.streaming_text.clear();
                let text = message.text();
                if !text.is_empty() {
                    self.transcript.push(TranscriptItem::Assistant { text });
                }
                if *stop_reason != StopReason::ToolUse && self.pending_ask_is_none() {
                    self.phase = AgentPhase::Idle;
                }
            }
            AgentEvent::ToolCallStarted {
                id,
                tool_name,
                input,
            } => {
                // Same exclusions as the TUI: `ask_user` is the pending
                // question, `write_tasks` is the board — neither is a card.
                if tool_name != "ask_user" && tool_name != "write_tasks" {
                    self.phase = AgentPhase::Working;
                    self.transcript.push(TranscriptItem::ToolCard {
                        id: id.clone(),
                        tool_name: tool_name.clone(),
                        input: input.clone(),
                        progress: Vec::new(),
                        output: None,
                        is_error: false,
                        running: true,
                    });
                }
            }
            AgentEvent::ToolProgress { id, line } => {
                if let Some(TranscriptItem::ToolCard { progress, .. }) = self.tool_card(id) {
                    progress.push(line.clone());
                }
            }
            AgentEvent::ToolCallResult {
                id,
                output,
                is_error,
            } => {
                if let Some(TranscriptItem::ToolCard {
                    output: card_output,
                    is_error: card_error,
                    running,
                    ..
                }) = self.tool_card(id)
                {
                    *card_output = Some(output.clone());
                    *card_error = *is_error;
                    *running = false;
                }
                // A result for the pending permission's call means it was
                // settled (or refused) some other way — cancelled turn,
                // denial — and the modal must not outlive it.
                if self
                    .pending_permission
                    .as_ref()
                    .is_some_and(|p| &p.tool_call_id == id)
                {
                    self.pending_permission = None;
                }
            }
            AgentEvent::PermissionPromptNeeded(request) => {
                self.phase = AgentPhase::WaitingPermission;
                self.pending_permission = Some(request.clone());
            }
            AgentEvent::UserQuestionNeeded(question) => {
                self.phase = AgentPhase::Asking;
                self.pending_question = Some(question.clone());
            }
            AgentEvent::PermissionResolved { tool_call_id, .. } => {
                if self
                    .pending_permission
                    .as_ref()
                    .is_some_and(|p| &p.tool_call_id == tool_call_id)
                {
                    self.pending_permission = None;
                }
            }
            AgentEvent::QuestionResolved { id, .. } => {
                if self.pending_question.as_ref().is_some_and(|q| &q.id == id) {
                    self.pending_question = None;
                }
            }
            AgentEvent::PhaseChanged(phase) => {
                self.phase = *phase;
            }
            AgentEvent::UserInterjected(text) => {
                self.transcript
                    .push(TranscriptItem::User { text: text.clone() });
            }
            AgentEvent::TokenUsage(usage) => {
                self.usage.input_tokens += usage.input_tokens;
                self.usage.output_tokens += usage.output_tokens;
                self.usage.cache_read += usage.cache_read;
                self.usage.cache_write += usage.cache_write;
            }
            AgentEvent::SessionCost {
                usd,
                unpriced_turns,
            } => {
                self.cost_usd = *usd;
                self.unpriced_turns = *unpriced_turns;
            }
            AgentEvent::ContextUsage {
                used,
                window,
                estimated,
            } => {
                self.context = Some((*used, *window, *estimated));
            }
            AgentEvent::ModelChanged {
                provider, model, ..
            } => {
                self.provider = provider.clone();
                self.model = model.clone();
            }
            AgentEvent::PlanGateChanged { gated } => {
                self.plan_gated = *gated;
            }
            AgentEvent::GoalChanged(goal) => {
                self.goal = goal.clone();
            }
            AgentEvent::TasksUpdated(tasks) => {
                self.tasks = tasks.clone();
            }
            AgentEvent::Error(message) => {
                self.transcript.push(TranscriptItem::System {
                    text: format!("error: {message}"),
                });
                self.phase = AgentPhase::Idle;
            }
            AgentEvent::TurnLimitReached { detail, .. } => {
                self.transcript.push(TranscriptItem::System {
                    text: detail.clone(),
                });
            }
            // Chrome the console does not render (yet): model pickers,
            // resource polls, retry notices, rewind reports, MCP tables,
            // loop bookkeeping. The events still stream to the browser —
            // this projection just keeps no state for them.
            AgentEvent::ModelsAvailable { .. }
            | AgentEvent::PermissionPolicyChanged { .. }
            | AgentEvent::ResourceUsage(_)
            | AgentEvent::ProviderRetry { .. }
            | AgentEvent::Rewind(_)
            | AgentEvent::McpStatus(_)
            | AgentEvent::LoopIterationStarted { .. }
            | AgentEvent::LoopFinished { .. } => {}
        }
    }

    fn pending_ask_is_none(&self) -> bool {
        self.pending_permission.is_none() && self.pending_question.is_none()
    }

    fn tool_card(&mut self, id: &str) -> Option<&mut TranscriptItem> {
        self.transcript.iter_mut().rev().find(
            |item| matches!(item, TranscriptItem::ToolCard { id: card_id, .. } if card_id == id),
        )
    }

    /// The user's own submitted message. Not an `AgentEvent` — the
    /// orchestrator never echoes the prompt back — so the server records it
    /// at the same moment it forwards the action.
    pub fn note_user_message(&mut self, text: &str) {
        self.transcript.push(TranscriptItem::User {
            text: text.to_string(),
        });
        self.phase = AgentPhase::Thinking;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smith_core::{ContentBlock, Message, PermissionDecision, Role};

    fn projection() -> SessionProjection {
        SessionProjection::new("s1".into(), "ollama".into(), "qwen".into())
    }

    fn assistant_turn(text: &str, stop: StopReason) -> AgentEvent {
        AgentEvent::AssistantTurnComplete {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
            },
            stop_reason: stop,
        }
    }

    /// The late-joiner contract: replaying a turn's events leaves a
    /// transcript a browser can render without ever having seen the stream.
    #[test]
    fn replaying_a_turn_builds_a_renderable_transcript() {
        let mut p = projection();
        p.note_user_message("read the config");
        p.apply(1, &AgentEvent::AssistantTextDelta("looking".into()));
        p.apply(
            2,
            &AgentEvent::ToolCallStarted {
                id: "c1".into(),
                tool_name: "read_file".into(),
                input: serde_json::json!({"path": "config.toml"}),
            },
        );
        p.apply(
            3,
            &AgentEvent::ToolProgress {
                id: "c1".into(),
                line: "500 lines".into(),
            },
        );
        p.apply(
            4,
            &AgentEvent::ToolCallResult {
                id: "c1".into(),
                output: "contents".into(),
                is_error: false,
            },
        );
        p.apply(5, &assistant_turn("done", StopReason::EndTurn));

        assert_eq!(p.seq, 5);
        assert_eq!(
            p.transcript.len(),
            3,
            "user + card + reply: {:?}",
            p.transcript
        );
        match &p.transcript[1] {
            TranscriptItem::ToolCard {
                progress,
                output,
                running,
                ..
            } => {
                assert_eq!(progress, &["500 lines"]);
                assert_eq!(output.as_deref(), Some("contents"));
                assert!(!running);
            }
            other => panic!("expected a tool card, got {other:?}"),
        }
        assert_eq!(p.phase, AgentPhase::Idle);
    }

    #[test]
    fn a_streaming_delta_accumulates_until_the_turn_completes() {
        let mut p = projection();
        p.apply(1, &AgentEvent::AssistantTextDelta("Ol".into()));
        p.apply(2, &AgentEvent::AssistantTextDelta("á".into()));
        assert_eq!(p.streaming_text, "Olá");
        p.apply(3, &assistant_turn("Olá", StopReason::EndTurn));
        assert_eq!(p.streaming_text, "", "the stream became a closed item");
        assert!(matches!(
            p.transcript.last(),
            Some(TranscriptItem::Assistant { text }) if text == "Olá"
        ));
    }

    /// A browser opening mid-ask renders the modal from the snapshot alone.
    #[test]
    fn a_pending_permission_appears_in_the_snapshot_and_clears_on_resolution() {
        let mut p = projection();
        p.apply(
            1,
            &AgentEvent::PermissionPromptNeeded(PermissionRequest {
                tool_call_id: "c1".into(),
                tool_name: "run_bash".into(),
                detail: "cargo test".into(),
            }),
        );
        assert!(p.pending_permission.is_some());
        assert_eq!(p.phase, AgentPhase::WaitingPermission);

        p.apply(
            2,
            &AgentEvent::PermissionResolved {
                tool_call_id: "c1".into(),
                decision: PermissionDecision::AllowOnce,
                source: smith_core::AskSource::Web,
            },
        );
        assert!(p.pending_permission.is_none());
    }

    #[test]
    fn tasks_updated_replaces_the_snapshot_wholesale() {
        let mut p = projection();
        p.apply(
            1,
            &AgentEvent::TasksUpdated(vec![Task {
                content: "a".into(),
                status: smith_core::TaskStatus::Pending,
            }]),
        );
        p.apply(
            2,
            &AgentEvent::TasksUpdated(vec![Task {
                content: "b".into(),
                status: smith_core::TaskStatus::Completed,
            }]),
        );
        assert_eq!(p.tasks.len(), 1);
        assert_eq!(p.tasks[0].content, "b");
    }

    /// The pump's ordering contract: the TUI sees every event, in order, and
    /// the broadcast carries the same events with their seq stamps.
    #[tokio::test]
    async fn the_tui_receives_every_event_the_pump_saw_in_order() {
        let (orch_tx, orch_rx) = mpsc::unbounded_channel();
        let (tui_tx, mut tui_rx) = mpsc::unbounded_channel();
        let tee = Tee::new("s1".into(), "p".into(), "m".into());
        let mut sub = tee.broadcast.subscribe();
        let pump_task = tokio::spawn(pump(orch_rx, tui_tx, tee.clone()));

        orch_tx
            .send(AgentEvent::AssistantTextDelta("a".into()))
            .unwrap();
        orch_tx
            .send(AgentEvent::AssistantTextDelta("b".into()))
            .unwrap();
        drop(orch_tx);
        pump_task.await.unwrap();

        assert!(matches!(
            tui_rx.recv().await,
            Some(AgentEvent::AssistantTextDelta(d)) if d == "a"
        ));
        assert!(matches!(
            tui_rx.recv().await,
            Some(AgentEvent::AssistantTextDelta(d)) if d == "b"
        ));

        let (seq1, _) = sub.recv().await.unwrap();
        let (seq2, _) = sub.recv().await.unwrap();
        assert_eq!((seq1, seq2), (1, 2));
        assert_eq!(tee.projection.read().unwrap().streaming_text, "ab");
    }

    /// Lag semantics: a subscriber that fell behind gets `Lagged`, which the
    /// SSE layer turns into a visible gap — asserted here at the broadcast
    /// level so the contract is pinned where it is created.
    #[tokio::test]
    async fn a_lagged_subscriber_is_told_how_much_it_missed() {
        let tee = Tee::new("s1".into(), "p".into(), "m".into());
        let mut sub = tee.broadcast.subscribe();
        for i in 0..(EVENT_BUFFER + 10) {
            let _ = tee
                .broadcast
                .send((i as u64, AgentEvent::AssistantTextDelta("x".into())));
        }
        match sub.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => assert!(n >= 10),
            other => panic!("expected Lagged, got {other:?}"),
        }
    }
}
