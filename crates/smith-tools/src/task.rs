//! `task` — delegate a bounded piece of work to a subagent.
//!
//! Registered like any other tool so the model sees a real schema and the
//! validator can check calls against it, but `execute` never runs: the agent
//! intercepts this tool by name and runs a whole child `Agent` instead. A
//! `Tool` gets `&self` and lives in this crate, so it can reach neither the
//! provider nor the tool registry the child needs — see
//! `smith_core::subagent`.

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};

pub struct TaskTool;

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        smith_core::subagent::TASK_TOOL
    }

    fn description(&self) -> &str {
        "Delegate a self-contained piece of research to a subagent, which runs its own \
         read-only agent loop and returns a single report. Everything it reads is discarded, \
         so this is how you investigate something broad without filling your own context: \
         'find every call site of X and summarise what each passes', 'work out how the \
         permission gate fits together', 'search the web for how library Y handles Z'. \
         The subagent starts with no knowledge of this conversation and cannot ask you \
         anything, so `prompt` must state the entire task and say exactly what to report \
         back. It cannot write files, run commands, or delegate further — do those yourself \
         once you have its report. Prefer a direct read_file/grep when you already know where \
         to look; a subagent costs a second conversation. \
         Args: description (3-5 word label), prompt (the full task), \
         subagent_type (optional; omit for the general-purpose one)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "A 3-5 word label for what this subagent is doing, shown to the user."
                },
                "prompt": {
                    "type": "string",
                    "description": "The complete task. The subagent sees only this — state the goal, where to look, and exactly what to report back."
                },
                "subagent_type": {
                    "type": "string",
                    "description": "Which configured subagent to use. Omit for the general-purpose one."
                }
            },
            "required": ["description", "prompt"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        // Read-only, and not merely by convention: a subagent's tool set is
        // intersected with the read-only tools before it is built, so this
        // call cannot reach anything that mutates. The agent intercepts it
        // before the permission gate either way.
        PermissionClass::ReadOnly
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        // Should never run — Agent::run_one_tool intercepts `task`.
        ToolResult::error("task must be handled by the agent's subagent bridge")
    }
}
