//! `write_tasks` — a visible checklist for the current work, shown live in
//! the TUI. Purely a UI signal: `Agent::run_one_tool` intercepts it (see
//! `ask_user`) before it ever reaches `execute` below.

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};

pub struct WriteTasksTool;

#[async_trait]
impl Tool for WriteTasksTool {
    fn name(&self) -> &str {
        "write_tasks"
    }

    fn description(&self) -> &str {
        "Maintain a visible checklist of the steps for the current work, shown live to the \
         user. Call it once at the start of any multi-step task (3+ steps) with the full list \
         (status: pending), then again whenever a step's status changes — mark it in_progress \
         right before starting it, completed right after finishing it. Always resend the FULL \
         list, not just the changed item. Keep task descriptions short and concrete (3-7 \
         words). Skip this entirely for single-step or trivial requests."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"]
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        // Bookkeeping only; the agent intercepts this tool and updates TUI
        // state directly instead of routing it through here.
        PermissionClass::ReadOnly
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        ToolResult::error("write_tasks must be handled by the agent UI bridge")
    }
}
