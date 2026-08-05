use async_trait::async_trait;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

use crate::event::ProgressReporter;
use crate::message::ToolDefinition;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionClass {
    /// Never prompts (e.g. read_file, list_dir, grep).
    ReadOnly,
    /// Prompts unless the session has already granted access (e.g. write_file, edit_file).
    Mutating,
    /// Always prompts unless the session has already granted access (e.g. run_bash,
    /// any MCP-sourced tool by default).
    Dangerous,
}

/// Session-wide override for how the permission gate behaves, set via
/// `/permission` (or persisted in config). Independent of the per-tool
/// "allow for this session" grants a user can still give from the modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPolicy {
    /// Always prompt for Mutating/Dangerous tools (the original behavior).
    #[default]
    Ask,
    /// Auto-allow Mutating tools (file writes/edits); still prompt for
    /// Dangerous ones (shell commands, MCP tools).
    Session,
    /// Auto-allow everything, including Dangerous tools. No confirmation of
    /// any kind — only ever set this deliberately, it removes the one
    /// safety net between the model and your shell.
    Skip,
}

impl PermissionPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionPolicy::Ask => "ask",
            PermissionPolicy::Session => "session",
            PermissionPolicy::Skip => "skip",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ask" => Some(PermissionPolicy::Ask),
            "session" | "allow-session" => Some(PermissionPolicy::Session),
            "skip" | "yolo" => Some(PermissionPolicy::Skip),
            _ => None,
        }
    }

    /// Whether a tool of this class should skip the confirmation prompt
    /// entirely under this policy (ReadOnly always does, regardless of
    /// policy — that's handled by the caller, not here).
    pub fn auto_allows(self, class: PermissionClass) -> bool {
        match self {
            PermissionPolicy::Ask => false,
            PermissionPolicy::Session => class != PermissionClass::Dangerous,
            PermissionPolicy::Skip => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Context handed to a tool at execution time.
///
/// Build one with `ToolContext::new`: the struct grows over time and every
/// literal is a site that has to change when it does.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    /// Stable id for this chat session — used for on-disk staging under
    /// `.smith/staging/<session_id>/`.
    pub session_id: String,
    /// Progress channel for the call currently executing, stamped on by
    /// `Agent::run_one_tool`. `None` on the agent's own session-long context
    /// and anywhere a context is built outside a tool call, so a tool must
    /// treat progress as best-effort — report through `report_progress`,
    /// which no-ops when there's nothing attached.
    pub progress: Option<ProgressReporter>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>, session_id: impl Into<String>) -> Self {
        Self {
            cwd: cwd.into(),
            session_id: session_id.into(),
            progress: None,
        }
    }

    /// A copy of this context scoped to one tool call. The agent keeps a
    /// single call-agnostic context for the session and stamps the current
    /// call onto a clone at dispatch time.
    pub fn with_progress(&self, progress: ProgressReporter) -> Self {
        Self {
            progress: Some(progress),
            ..self.clone()
        }
    }

    /// The id of the tool call being executed, if this context belongs to one.
    pub fn tool_call_id(&self) -> Option<&str> {
        self.progress.as_ref().map(ProgressReporter::id)
    }

    /// Reports one line of progress while the tool is still running. A no-op
    /// when no channel is attached, so tools never have to branch on it.
    pub fn report_progress(&self, line: impl Into<String>) {
        if let Some(progress) = &self.progress {
            progress.send(line);
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    fn permission_class(&self) -> PermissionClass;

    /// `cancel` is fired if the user interrupts the turn mid-execution
    /// (e.g. Esc while a shell command is running) — long-running tools
    /// should race it and abort their work (killing any child process).
    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_policy_never_auto_allows() {
        let policy = PermissionPolicy::Ask;
        assert!(!policy.auto_allows(PermissionClass::Mutating));
        assert!(!policy.auto_allows(PermissionClass::Dangerous));
    }

    #[test]
    fn session_policy_allows_mutating_but_not_dangerous() {
        let policy = PermissionPolicy::Session;
        assert!(policy.auto_allows(PermissionClass::Mutating));
        assert!(!policy.auto_allows(PermissionClass::Dangerous));
    }

    #[test]
    fn skip_policy_allows_everything() {
        let policy = PermissionPolicy::Skip;
        assert!(policy.auto_allows(PermissionClass::Mutating));
        assert!(policy.auto_allows(PermissionClass::Dangerous));
    }

    #[test]
    fn parses_known_aliases() {
        assert_eq!(PermissionPolicy::parse("ask"), Some(PermissionPolicy::Ask));
        assert_eq!(
            PermissionPolicy::parse("session"),
            Some(PermissionPolicy::Session)
        );
        assert_eq!(
            PermissionPolicy::parse("allow-session"),
            Some(PermissionPolicy::Session)
        );
        assert_eq!(
            PermissionPolicy::parse("skip"),
            Some(PermissionPolicy::Skip)
        );
        assert_eq!(
            PermissionPolicy::parse("yolo"),
            Some(PermissionPolicy::Skip)
        );
        assert_eq!(PermissionPolicy::parse("bogus"), None);
    }

    #[test]
    fn a_context_built_outside_a_tool_call_has_no_progress_channel() {
        let ctx = ToolContext::new(".", "test-session");
        assert!(ctx.progress.is_none());
        assert!(ctx.tool_call_id().is_none());
        // Must not panic — tools call this unconditionally.
        ctx.report_progress("into the void");
    }

    #[test]
    fn with_progress_scopes_a_copy_to_one_call_and_leaves_the_original_alone() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let base = ToolContext::new("/tmp", "test-session");
        let scoped = base.with_progress(ProgressReporter::new("call_1", tx));

        assert_eq!(scoped.tool_call_id(), Some("call_1"));
        assert_eq!(scoped.cwd, base.cwd);
        assert_eq!(scoped.session_id, base.session_id);
        assert!(base.tool_call_id().is_none());

        scoped.report_progress("halfway");
        match rx.try_recv().unwrap() {
            crate::event::AgentEvent::ToolProgress { id, line } => {
                assert_eq!(id, "call_1");
                assert_eq!(line, "halfway");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn round_trips_through_as_str() {
        for policy in [
            PermissionPolicy::Ask,
            PermissionPolicy::Session,
            PermissionPolicy::Skip,
        ] {
            assert_eq!(PermissionPolicy::parse(policy.as_str()), Some(policy));
        }
    }
}
