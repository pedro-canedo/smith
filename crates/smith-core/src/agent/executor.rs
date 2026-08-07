//! The tool-execution port, and the channels a frontend answers on.

use async_trait::async_trait;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::event::{PermissionDecision, PermissionRequest, UserQuestion};
use crate::tool::{PermissionClass, ToolContext, ToolResult};

/// Implemented by smith-tools::ToolRegistry. Kept as a trait here so smith-core
/// never depends on the concrete tool crate.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition>;
    fn permission_class(&self, name: &str) -> Option<PermissionClass>;
    /// Forwards `Tool::snapshot_paths` for the named tool. Defaulted so an
    /// executor that has no filesystem tools at all (and every existing
    /// implementation) compiles unchanged.
    fn snapshot_paths(
        &self,
        _name: &str,
        _input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
    /// Forwards `Tool::scratch_scoped` for the named tool. Defaulted to
    /// `false` — the safe answer — so executors without filesystem tools
    /// compile unchanged and never accidentally waive a prompt.
    fn scratch_scoped(&self, _name: &str, _input: &serde_json::Value, _ctx: &ToolContext) -> bool {
        false
    }
    /// Checks a call against the schema the model was shown, without running
    /// it.
    ///
    /// `execute` already does this at dispatch and still does; this exists so
    /// the *same* check can be applied to arguments a `PreToolUse` hook
    /// rewrote, at the point the rewrite happens. Without it the only place a
    /// hook's mistake would surface is a dispatch-time error that reads as if
    /// the model had written the bad arguments — blaming the one participant
    /// that did not.
    ///
    /// Defaulted to `Ok(())` so executors that publish no schemas compile
    /// unchanged. That is safe rather than lax: a default `Ok` only loses the
    /// attribution, never the check, because dispatch validates again.
    fn validate_input(&self, _name: &str, _input: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult;
}

/// A no-op executor for when the agent is run without any tools wired in yet.
pub struct NoTools;

#[async_trait]
impl ToolExecutor for NoTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
    }

    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        None
    }

    async fn execute(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        ToolResult::error("no tools are registered")
    }
}

/// Asks the TUI (or any frontend) to resolve a permission prompt. The oneshot
/// sender is how the caller's answer makes it back into the orchestration loop.
pub struct PermissionAsk {
    pub request: PermissionRequest,
    pub respond_to: oneshot::Sender<PermissionDecision>,
}

/// Asks a frontend to resolve an `ask_user` question.
///
/// The oneshot carries `Ok(answer)` — one of the three suggestions or custom
/// input — or `Err(reason)` when the frontend cannot ask at all. That second
/// case is not hypothetical: headless runs have no user, and being able only
/// to *answer* forced them to put words in the user's mouth. A refusal comes
/// back to the model as a failed tool call, which is the honest shape.
pub struct QuestionAsk {
    pub question: UserQuestion,
    pub respond_to: oneshot::Sender<Result<String, String>>,
}
