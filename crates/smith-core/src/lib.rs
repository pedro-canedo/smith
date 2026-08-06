//! Domain types, provider/tool traits, and the agent orchestration loop.

pub mod agent;
pub mod checkpoint;
pub mod context;
pub mod event;
pub mod hooks;
pub mod message;
pub mod permission_detail;
pub mod pricing;
pub mod provider;
pub mod redact;
pub mod retry;
pub mod subagent;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod tool;

pub use agent::{
    parse_tasks, Agent, CompactionConfig, CompactionOutcome, NoTools, PermissionAsk, QuestionAsk,
    ToolExecutor, TurnAccounting, TurnLimits,
};
pub use checkpoint::{Checkpointer, ConflictKind, RewindConflict, RewindReport, RewindStatus};
pub use context::{carry_over, CarryOver, ContextUsage, COMPACT_THRESHOLD};
pub use event::{
    Action, AgentEvent, AgentPhase, LoopStopReason, PermissionDecision, PermissionRequest,
    ProgressReporter, ResourceStats, Task, TaskStatus, TurnLimitKind, UserQuestion,
};
pub use hooks::{HookDefinition, HookEvent, HookSet};
pub use message::{
    CompletionRequest, ContentBlock, Message, Role, StopReason, StreamEvent, ToolDefinition, Usage,
};
pub use permission_detail::format_permission_detail;
pub use provider::{LlmProvider, ProviderCapabilities, ProviderError};
pub use redact::Redactor;
pub use retry::RetryPolicy;
pub use subagent::SubagentDefinition;
pub use tool::{PermissionClass, PermissionPolicy, Tool, ToolContext, ToolResult};
