//! Domain types, provider/tool traits, and the agent orchestration loop.

pub mod agent;
pub mod event;
pub mod message;
pub mod permission_detail;
pub mod provider;
pub mod redact;
pub mod tool;

pub use agent::{parse_tasks, Agent, NoTools, PermissionAsk, QuestionAsk, ToolExecutor};
pub use event::{
    Action, AgentEvent, AgentPhase, LoopStopReason, PermissionDecision, PermissionRequest,
    ResourceStats, Task, TaskStatus, UserQuestion,
};
pub use message::{
    CompletionRequest, ContentBlock, Message, Role, StopReason, StreamEvent, ToolDefinition, Usage,
};
pub use permission_detail::format_permission_detail;
pub use provider::{LlmProvider, ProviderError};
pub use redact::Redactor;
pub use tool::{PermissionClass, PermissionPolicy, Tool, ToolContext, ToolResult};
