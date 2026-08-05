use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::message::{Message, StopReason, Usage};
use crate::tool::PermissionPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    /// One-line summary of what the tool will do (e.g. create/overwrite a
    /// path, run a command) — never the full file body. Built by
    /// `format_permission_detail`.
    pub detail: String,
}

/// A clarifying question the agent asks the user (via the `ask_user` tool).
/// Always carries exactly three suggestions; the TUI also offers a free-text
/// fourth option.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestion {
    pub id: String,
    pub prompt: String,
    pub options: [String; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// One step of a checklist the agent keeps visible via the `write_tasks`
/// tool — mirrors how coding-agent CLIs surface a persistent todo list
/// instead of just a transient "thinking…" status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub content: String,
    pub status: TaskStatus,
}

/// High-level agent activity for status chrome (avoids a silent "stuck" UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    #[default]
    Idle,
    Thinking,
    Planning,
    Building,
    Asking,
    WaitingPermission,
    Working,
    /// Running under `/loop` — one turn among several iterations.
    Looping,
}

/// Why a `/loop` run ended, reported on `AgentEvent::LoopFinished`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopStopReason {
    /// The model's reply contained the loop's completion sentinel.
    Done,
    /// The configured (or default) iteration cap was reached.
    MaxIterations,
    /// Cancelled via Esc / `Action::CancelGeneration`.
    Cancelled,
    /// A turn ended in a provider/stream error (not a cancellation).
    Failed,
}

/// Which cap stopped a turn — see `AgentEvent::TurnLimitReached`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnLimitKind {
    /// Tool-call rounds within one turn.
    Rounds,
    /// Individual tool calls within one turn, summed across rounds.
    ToolCalls,
    /// Elapsed time since the turn started.
    WallClock,
}

/// A snapshot of local machine resource usage, polled independently of the
/// LLM turn loop — only meaningful for local providers (e.g. Ollama), where
/// there's no per-token cost to show instead.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ResourceStats {
    pub cpu_percent: f32,
    pub ram_used_mb: u64,
    pub ram_total_mb: u64,
    pub gpu_percent: Option<f32>,
    pub vram_used_mb: Option<u64>,
    pub vram_total_mb: Option<u64>,
}

/// Actions flowing from input handling into the agent orchestrator.
#[derive(Debug, Clone)]
pub enum Action {
    SubmitMessage(String),
    CancelGeneration,
    PermissionResponse(PermissionDecision),
    /// `provider: None` keeps the current provider and just changes the
    /// model; `Some(p)` rebuilds the agent against a different provider
    /// (conversation history is preserved either way). `save` persists the
    /// resulting provider/model to `~/.smith/config.toml`.
    SwitchModel {
        provider: Option<String>,
        model: String,
        save: bool,
    },
    SetPermissionPolicy {
        policy: PermissionPolicy,
        save: bool,
    },
    /// `/plan <description>`: runs a planning-only turn and gates Mutating/
    /// Dangerous tools until `ApprovePlan` or `RejectPlan`.
    StartPlan(String),
    ApprovePlan,
    RejectPlan,
    /// `/goal <description>` (`Some`) or `/goal clear` (`None`). Persisted to
    /// `.smith/goal.md` and folded into the system prompt on every request.
    SetGoal(Option<String>),
    /// Answer to an `ask_user` prompt (chosen suggestion or free text).
    QuestionResponse(String),
    /// `/compact`: summarise the older part of the conversation now, instead
    /// of waiting for the context to cross the auto-compaction threshold.
    Compact,
    /// `/remember <note>`: append a standing instruction to the project's
    /// `SMITH.md`. Distinct from `SetGoal` — a goal is what this session is
    /// for and dies with it; a memory outlives the session and every later
    /// one in this project inherits it.
    Remember(String),
    /// `/loop [<N>] <task>`: runs `task` repeatedly — each iteration after the
    /// first is a "continue" turn — until the model's reply contains the
    /// completion sentinel, `max_iterations` is reached (defaults to a safety
    /// cap when `None`), or the turn is cancelled/fails.
    StartLoop {
        prompt: String,
        max_iterations: Option<u32>,
    },
    Quit,
}

/// Events flowing from the agent orchestrator out to the TUI (and persistence).
///
/// The tagged serialization is not incidental: it *is* the wire format for
/// `--output-format stream-json`, so variant and field names are a public
/// interface that scripts will parse. Renaming one is a breaking change even
/// though nothing in this crate reads it back.
///
/// Adjacent tagging (`{"type": ..., "data": ...}`) rather than internal:
/// serde cannot internally-tag a newtype variant wrapping a string or a
/// sequence, and several variants are exactly that (`AssistantTextDelta`,
/// `TasksUpdated`). It also makes every event the same shape, so a consumer
/// can branch on `.type` and read `.data` without special cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    AssistantTextDelta(String),
    /// `stop_reason` tells the frontend whether this is an intermediate round
    /// (`ToolUse` — more work is coming) or the turn's true final reply.
    AssistantTurnComplete {
        message: Message,
        stop_reason: StopReason,
    },
    ToolCallStarted {
        id: String,
        tool_name: String,
        input: serde_json::Value,
    },
    /// A line emitted by a tool that is still running, between its
    /// `ToolCallStarted` and `ToolCallResult`. `id` matches that pair, so a
    /// frontend can attach the line to the right call even while several
    /// tools are in flight. Advisory only — a tool's real answer is always
    /// the `ToolCallResult`, never the progress lines.
    ToolProgress {
        id: String,
        line: String,
    },
    ToolCallResult {
        id: String,
        output: String,
        is_error: bool,
    },
    PermissionPromptNeeded(PermissionRequest),
    /// The agent needs a clarifying answer before continuing (3 options + custom).
    UserQuestionNeeded(UserQuestion),
    /// Coarse status for the activity line / chrome.
    PhaseChanged(AgentPhase),
    TokenUsage(Usage),
    /// How full the model's context window is, recomputed after every provider
    /// round and after a compaction.
    ///
    /// Three flat fields rather than a ratio, because a ratio throws away the
    /// two numbers a frontend actually wants to print ("128k / 200k") and
    /// cannot be recovered from. `estimated` exists because the honest answer
    /// changes shape mid-turn: right after a response `used` is the provider's
    /// own prompt count and is exact, but once tool results have been appended
    /// part of it is a `chars/4` estimate — and a gauge that renders "162,000"
    /// when it means "roughly 162,000" is lying about which one it has.
    ContextUsage {
        used: u32,
        window: u32,
        estimated: bool,
    },
    /// Confirms a successful `Action::SwitchModel`, so the frontend can
    /// update its own display labels and let the user know it took effect.
    ModelChanged {
        provider: String,
        model: String,
        saved: bool,
    },
    /// Confirms a successful `Action::SetPermissionPolicy`.
    PermissionPolicyChanged {
        policy: PermissionPolicy,
        saved: bool,
    },
    /// Piggybacks on this same stream for plumbing simplicity, even though
    /// it isn't produced by the agent loop itself — see `ResourceStats`.
    ResourceUsage(ResourceStats),
    /// Reflects the agent's current `plan_gated` state after `StartPlan`,
    /// `ApprovePlan`, or `RejectPlan`.
    PlanGateChanged {
        gated: bool,
    },
    /// Confirms a successful `Action::SetGoal`.
    GoalChanged(Option<String>),
    /// A new `/loop` turn is starting; `max_iterations` is the resolved cap
    /// (after defaulting), for progress display like "iteration 3/25".
    LoopIterationStarted {
        iteration: u32,
        max_iterations: u32,
    },
    /// The loop driver stopped; `iterations` is how many turns actually ran.
    LoopFinished {
        reason: LoopStopReason,
        iterations: u32,
    },
    /// The agent replaced its task checklist via `write_tasks` — the full
    /// list, not a diff; the frontend just swaps its copy wholesale.
    TasksUpdated(Vec<Task>),
    /// A provider request failed with something worth re-sending, and the
    /// agent is about to wait `delay_ms` before attempt `attempt + 1`.
    ///
    /// Emitted *before* the sleep, always. A backoff nobody can see is
    /// indistinguishable from a hang, and a user who thinks the app has hung
    /// kills it — losing the turn the retry was about to save.
    ProviderRetry {
        /// 1-based index of the attempt that just failed.
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        /// The failure being retried, rendered for display.
        reason: String,
    },
    /// The turn stopped because it hit one of its own safety caps, not because
    /// anything failed.
    ///
    /// Deliberately *not* an `Error`. The turn did exactly what it was
    /// configured to do, everything it completed is intact, and the remedy is
    /// "say continue" — rendering that as an error teaches users to ignore
    /// errors and hides that the work survived.
    TurnLimitReached {
        kind: TurnLimitKind,
        /// One line naming the cap and its value, ready to show as-is.
        detail: String,
    },
    Error(String),
}

/// A handle onto the `AgentEvent` stream scoped to a single tool call. Handed
/// to a tool through `ToolContext` so it can report progress mid-execution
/// without knowing anything about the agent loop — or even its own call id.
///
/// It rides the *same* channel as `ToolCallStarted`/`ToolCallResult` rather
/// than one of its own: the ordering between the three is what makes the
/// stream renderable, and a separate channel could deliver a progress line
/// ahead of the call it belongs to.
#[derive(Debug, Clone)]
pub struct ProgressReporter {
    id: String,
    events: mpsc::UnboundedSender<AgentEvent>,
}

impl ProgressReporter {
    pub fn new(id: impl Into<String>, events: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self {
            id: id.into(),
            events,
        }
    }

    /// The id of the tool call this reporter belongs to.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Emits one progress line. A closed receiver means the frontend is gone,
    /// which is not a running tool's problem — so send failures are dropped
    /// rather than surfaced.
    pub fn send(&self, line: impl Into<String>) {
        let _ = self.events.send(AgentEvent::ToolProgress {
            id: self.id.clone(),
            line: line.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The serialized shape is the `--output-format stream-json` contract, so
    /// it is pinned deliberately: a rename here breaks every script parsing
    /// smith's output, and nothing inside this crate would notice.
    #[test]
    fn agent_event_serializes_as_a_tagged_snake_case_object() {
        let json = serde_json::to_value(AgentEvent::AssistantTextDelta("hi".into())).unwrap();
        assert_eq!(json["type"], "assistant_text_delta");

        assert_eq!(json["data"], "hi");

        let json = serde_json::to_value(AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "done".into(),
            is_error: false,
        })
        .unwrap();
        assert_eq!(json["type"], "tool_call_result");
        assert_eq!(json["data"]["id"], "call_1");
        assert_eq!(json["data"]["output"], "done");
        assert_eq!(json["data"]["is_error"], false);
    }

    #[test]
    fn agent_event_round_trips_through_json() {
        let events = vec![
            AgentEvent::PhaseChanged(AgentPhase::Thinking),
            AgentEvent::TokenUsage(Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read: 30,
                cache_write: 40,
            }),
            AgentEvent::TasksUpdated(vec![Task {
                content: "ship it".into(),
                status: TaskStatus::InProgress,
            }]),
            AgentEvent::Error("boom".into()),
        ];

        for event in events {
            let encoded = serde_json::to_string(&event).unwrap();
            let decoded: AgentEvent = serde_json::from_str(&encoded)
                .unwrap_or_else(|e| panic!("{encoded} failed to round-trip: {e}"));
            assert_eq!(format!("{event:?}"), format!("{decoded:?}"));
        }
    }

    #[test]
    fn tool_progress_serializes_alongside_the_rest_of_the_tool_call_events() {
        let json = serde_json::to_value(AgentEvent::ToolProgress {
            id: "call_1".into(),
            line: "compiling...".into(),
        })
        .unwrap();
        assert_eq!(json["type"], "tool_progress");
        assert_eq!(json["data"]["id"], "call_1");
        assert_eq!(json["data"]["line"], "compiling...");
    }

    #[test]
    fn progress_reporter_tags_every_line_with_its_call_id() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reporter = ProgressReporter::new("call_7", tx);
        assert_eq!(reporter.id(), "call_7");

        reporter.send("first");
        reporter.send("second".to_string());

        let lines: Vec<(String, String)> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| match e {
                AgentEvent::ToolProgress { id, line } => (id, line),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                ("call_7".to_string(), "first".to_string()),
                ("call_7".to_string(), "second".to_string()),
            ]
        );
    }

    /// A tool holding a reporter must not care that the frontend went away —
    /// it is fire-and-forget by design.
    #[test]
    fn progress_reporter_survives_a_dropped_receiver() {
        let (tx, rx) = mpsc::unbounded_channel();
        let reporter = ProgressReporter::new("call_1", tx);
        drop(rx);
        reporter.send("nobody is listening");
    }

    /// Enum payloads go over the wire too; `snake_case` keeps them idiomatic
    /// for `jq` rather than leaking Rust's PascalCase.
    #[test]
    fn enum_payloads_are_snake_case() {
        assert_eq!(
            serde_json::to_value(TaskStatus::InProgress).unwrap(),
            "in_progress"
        );
        assert_eq!(
            serde_json::to_value(AgentPhase::WaitingPermission).unwrap(),
            "waiting_permission"
        );
        assert_eq!(
            serde_json::to_value(PermissionDecision::AllowSession).unwrap(),
            "allow_session"
        );
    }
}
