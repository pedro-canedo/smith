use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolDefinition, ToolExecutor, ToolResult};
use tokio_util::sync::CancellationToken;

/// Holds every tool available to the agent (built-in and, later, MCP-bridged)
/// and implements smith_core::ToolExecutor so the orchestration loop can stay
/// agnostic of where a tool actually came from.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Registers every built-in tool: the read-only/mutating file tools plus
    /// the shell tool. Use `register` directly if you want a narrower set
    /// (e.g. read-only tools only).
    pub fn with_builtin_tools() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(crate::fs_tools::ReadFileTool));
        registry.register(Arc::new(crate::fs_tools::ListDirTool));
        registry.register(Arc::new(crate::fs_tools::GlobTool));
        registry.register(Arc::new(crate::fs_tools::WriteFileTool));
        registry.register(Arc::new(crate::fs_tools::EditFileTool));
        registry.register(Arc::new(crate::shell_tool::RunBashTool));
        registry.register(Arc::new(crate::ask_user::AskUserTool));
        registry.register(Arc::new(crate::write_tasks::WriteTasksTool));
        // Keyless by default (tries Exa's hosted-free endpoint, then falls
        // back to DuckDuckGo lite) — callers with an Exa API key should
        // `register` a fresh `WebSearchTool::new(Some(key))` afterward to
        // replace this one, since `register` overwrites by name.
        registry.register(Arc::new(crate::web_search::WebSearchTool::new(None)));
        registry
    }
}

#[async_trait]
impl ToolExecutor for ToolRegistry {
    fn tool_defs(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    fn permission_class(&self, name: &str) -> Option<PermissionClass> {
        self.tools.get(name).map(|t| t.permission_class())
    }

    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        match self.tools.get(name) {
            Some(tool) => tool.execute(input, ctx, cancel).await,
            None => ToolResult::error(format!("unknown tool: {name}")),
        }
    }
}
