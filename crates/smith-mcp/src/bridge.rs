use std::sync::Arc;

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

use crate::client::{McpClient, McpToolDef};

/// Adapts one remote MCP tool to smith_core::Tool so the agent's orchestration
/// loop can call it exactly like a built-in. Always `Dangerous`: an arbitrary
/// MCP server's tool semantics can't be statically trusted, so it always
/// prompts unless the user grants it for the session.
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    def: McpToolDef,
}

impl McpToolAdapter {
    pub fn new(client: Arc<McpClient>, def: McpToolDef) -> Self {
        Self { client, def }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.def.name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.def.input_schema.clone()
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Dangerous
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => ToolResult::error("cancelled by user"),
            result = self.client.call_tool(&self.def.name, input) => match result {
                Ok(outcome) => ToolResult { content: outcome.text, is_error: outcome.is_error },
                Err(e) => ToolResult::error(e.to_string()),
            },
        }
    }
}
