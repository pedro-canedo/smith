use crate::message::{Message, StopReason, Usage};
use crate::tool::PermissionPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct UserQuestion {
    pub id: String,
    pub prompt: String,
    pub options: [String; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// One step of a checklist the agent keeps visible via the `write_tasks`
/// tool — mirrors how coding-agent CLIs surface a persistent todo list
/// instead of just a transient "thinking…" status.
#[derive(Debug, Clone)]
pub struct Task {
    pub content: String,
    pub status: TaskStatus,
}

/// High-level agent activity for status chrome (avoids a silent "stuck" UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, Default)]
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
#[derive(Debug, Clone)]
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
