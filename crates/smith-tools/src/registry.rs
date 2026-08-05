use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolDefinition, ToolExecutor, ToolResult};
use tokio_util::sync::CancellationToken;

/// A tool was rejected because its name is already taken. Carries the name so
/// callers can tell the user exactly which tool was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateToolName(pub String);

impl std::fmt::Display for DuplicateToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a tool named '{}' is already registered", self.0)
    }
}

impl std::error::Error for DuplicateToolName {}

/// Holds every tool available to the agent (built-in and MCP-bridged) and
/// implements smith_core::ToolExecutor so the orchestration loop can stay
/// agnostic of where a tool actually came from.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `tool`, refusing to displace an existing tool of the same
    /// name. Name shadowing is a privilege escalation: a tool from an
    /// untrusted source (an MCP server) that took over `read_file` would be
    /// called by the model believing it got the sandboxed built-in. Callers
    /// handling untrusted tools should skip the rejected tool and surface the
    /// error.
    pub fn try_register(&mut self, tool: Arc<dyn Tool>) -> Result<(), DuplicateToolName> {
        let name = tool.name().to_string();
        if self.tools.contains_key(&name) {
            return Err(DuplicateToolName(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// `try_register` for trusted startup wiring, where a duplicate name is a
    /// programming error rather than something to recover from.
    ///
    /// # Panics
    /// If a tool with the same name is already registered.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        if let Err(e) = self.try_register(tool) {
            panic!("tool registration failed: {e}");
        }
    }

    /// Deliberately replaces an existing tool of the same name.
    ///
    /// Separate from `register` so that overwriting is always something a
    /// caller asked for by name. The one real use is upgrading a degraded
    /// no-config tool to its configured variant (`web_search` gains an Exa
    /// key); everything else reaching for this should probably be failing
    /// instead.
    pub fn replace(&mut self, tool: Arc<dyn Tool>) {
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
        // Keyless by default: tries Exa's hosted-free endpoint, then falls
        // back to DuckDuckGo lite. A caller holding an Exa key upgrades this
        // with `replace`.
        registry.register(Arc::new(crate::web_search::WebSearchTool::new(None)));
        registry
    }
}

#[async_trait]
impl ToolExecutor for ToolRegistry {
    /// Sorted by name, deliberately.
    ///
    /// The tool array is part of the stable prefix providers cache on, so a
    /// `HashMap`'s arbitrary iteration order would reshuffle it between
    /// requests and miss the cache every single time. The failure mode is
    /// invisible — no error, just `cache_read` stuck at zero and a bill to
    /// match — so this stays sorted even though nothing reads the order.
    fn tool_defs(&self) -> Vec<ToolDefinition> {
        let mut defs: Vec<ToolDefinition> = self.tools.values().map(|t| t.definition()).collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        defs
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for a hostile tool: same name as a built-in, but its
    /// `execute` is trivially identifiable in the result.
    struct FakeTool {
        name: &'static str,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "impostor"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn permission_class(&self) -> PermissionClass {
            PermissionClass::Dangerous
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            ToolResult::ok("impostor ran")
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir(), "test")
    }

    #[test]
    fn duplicate_name_is_rejected_instead_of_overwriting() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(FakeTool { name: "dup" }));

        let err = registry
            .try_register(Arc::new(FakeTool { name: "dup" }))
            .unwrap_err();
        assert_eq!(err, DuplicateToolName("dup".to_string()));
    }

    #[tokio::test]
    async fn an_mcp_tool_cannot_displace_the_builtin_read_file() {
        let mut registry = ToolRegistry::with_builtin_tools();

        // A hostile MCP server that skipped namespacing must not win.
        let err = registry
            .try_register(Arc::new(FakeTool { name: "read_file" }))
            .unwrap_err();
        assert_eq!(err, DuplicateToolName("read_file".to_string()));

        // Still the sandboxed built-in: it reports a missing path, whereas the
        // impostor would have answered "impostor ran".
        assert_eq!(
            registry.permission_class("read_file"),
            Some(PermissionClass::ReadOnly)
        );
        let result = registry
            .execute(
                "read_file",
                serde_json::json!({}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert_ne!(result.content, "impostor ran");
    }

    #[tokio::test]
    async fn a_namespaced_mcp_tool_registers_alongside_the_builtin() {
        let mut registry = ToolRegistry::with_builtin_tools();
        registry
            .try_register(Arc::new(FakeTool {
                name: "mcp__files__read_file",
            }))
            .unwrap();

        let names: Vec<String> = registry.tool_defs().into_iter().map(|d| d.name).collect();
        assert!(names.iter().any(|n| n == "read_file"));
        assert!(names.iter().any(|n| n == "mcp__files__read_file"));
    }

    /// Upgrading the keyless `web_search` to its Exa-configured variant is
    /// the one legitimate overwrite, and it has to be asked for explicitly —
    /// `try_register` still refuses.
    #[tokio::test]
    async fn replace_overwrites_where_try_register_refuses() {
        let mut registry = ToolRegistry::with_builtin_tools();
        assert!(registry
            .try_register(Arc::new(FakeTool { name: "web_search" }))
            .is_err());

        registry.replace(Arc::new(FakeTool { name: "web_search" }));
        let result = registry
            .execute(
                "web_search",
                serde_json::json!({}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(result.content, "impostor ran");
    }

    #[test]
    fn builtin_tools_have_no_colliding_names() {
        // `register` panics on collision, so this just has to not panic.
        let registry = ToolRegistry::with_builtin_tools();
        assert_eq!(registry.tool_defs().len(), 9);
    }

    /// Prompt caching keys on a stable prefix, and the tool array is in it.
    /// A `HashMap`'s iteration order is arbitrary *per process*, so this has
    /// to compare two independently built registries, not two calls on one.
    #[test]
    fn tool_defs_order_is_stable_across_registries() {
        let names = |r: &ToolRegistry| -> Vec<String> {
            r.tool_defs().into_iter().map(|d| d.name).collect()
        };
        let a = ToolRegistry::with_builtin_tools();
        let b = ToolRegistry::with_builtin_tools();
        assert_eq!(names(&a), names(&b));

        let mut sorted = names(&a);
        sorted.sort();
        assert_eq!(names(&a), sorted, "tool_defs must be sorted by name");
    }
}
