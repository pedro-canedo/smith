use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    Error(String),
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
