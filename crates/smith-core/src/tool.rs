use async_trait::async_trait;
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    /// Stable id for this chat session — used for on-disk staging under
    /// `.smith/staging/<session_id>/`.
    pub session_id: String,
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
