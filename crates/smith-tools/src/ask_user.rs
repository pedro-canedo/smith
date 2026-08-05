//! `ask_user` — mediated clarifying question (3 options + free-text in the TUI).

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a clarifying question when a decision is ambiguous or high-impact. \
         Always provide exactly three concrete suggestions (option_a/b/c). The UI also \
         offers a free-text fourth option. Prefer deciding yourself for low-risk choices; \
         use this sparingly so the user can mostly watch you work. Do NOT call this just to \
         open a conversation or in response to a greeting/small talk with no task yet — reply \
         normally instead; only use it once there's an actual task in progress and a genuine \
         fork in how to proceed. \
         Args: question, option_a, option_b, option_c (all required strings)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {"type": "string"},
                "option_a": {"type": "string"},
                "option_b": {"type": "string"},
                "option_c": {"type": "string"}
            },
            "required": ["question", "option_a", "option_b", "option_c"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        // Treated as read-only for the plan gate / permission modal; the agent
        // intercepts this tool and routes it through the question UI instead.
        PermissionClass::ReadOnly
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        // Should never run — Agent::run_one_tool mediates ask_user.
        ToolResult::error("ask_user must be handled by the agent UI bridge")
    }
}
