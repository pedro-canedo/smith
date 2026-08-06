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
        // One read-set shared by the file tools: `write_file` refuses to
        // replace an existing file that `read_file` has not shown the model
        // (see `fs_tools::ReadSet`), so they have to be looking at the same
        // record of what was read.
        let reads = Arc::new(crate::fs_tools::ReadSet::new());
        registry.register(Arc::new(crate::fs_tools::ReadFileTool::new(reads.clone())));
        registry.register(Arc::new(crate::fs_tools::ListDirTool));
        registry.register(Arc::new(crate::fs_tools::GlobTool));
        registry.register(Arc::new(crate::grep::GrepTool));
        registry.register(Arc::new(crate::fs_tools::WriteFileTool::new(reads.clone())));
        registry.register(Arc::new(crate::fs_tools::EditFileTool::new(reads.clone())));
        registry.register(Arc::new(crate::fs_tools::MultiEditTool::new(reads)));
        registry.register(Arc::new(crate::shell_tool::RunBashTool));
        registry.register(Arc::new(crate::ask_user::AskUserTool));
        registry.register(Arc::new(crate::write_tasks::WriteTasksTool));
        // Unconfigured by default: Bing (free, no key) with DuckDuckGo behind
        // it. A caller holding an Exa key or a SearXNG URL upgrades this with
        // `replace`.
        registry.register(Arc::new(crate::web_search::WebSearchTool::new(None)));
        registry.register(Arc::new(crate::web_fetch::WebFetchTool::new()));
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

    /// An unknown tool answers "nothing", same as a tool that declines to
    /// predict its writes — and the agent treats both as uncovered, so a
    /// missing registration can never be mistaken for a covered call.
    fn snapshot_paths(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Vec<std::path::PathBuf> {
        self.tools
            .get(name)
            .map(|t| t.snapshot_paths(input, ctx))
            .unwrap_or_default()
    }

    /// The one place a tool call is checked against the schema the model was
    /// shown.
    ///
    /// It goes here rather than in each tool for the same reason the plan gate
    /// and the permission prompt live in `Agent::run_one_tool`: it is the
    /// choke point every call passes through, so a tool added tomorrow is
    /// covered by the schema it already has to publish, and no tool can forget.
    /// Tools keep their own checks behind this — validation proves the
    /// *shape*, never that a path exists or that `old_str` is unique.
    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        let Some(tool) = self.tools.get(name) else {
            return ToolResult::error(format!("unknown tool: {name}"));
        };
        // A rejection is an ordinary `ToolResult::error`, deliberately: the
        // model already sees these, and a wrong argument is the most
        // correctable thing it can be told. Anything harsher (aborting the
        // turn) would spend a whole turn on a fixable typo.
        if let Err(message) =
            crate::schema_validate::validate_input(name, &tool.input_schema(), &input)
        {
            return ToolResult::error(message);
        }
        tool.execute(input, ctx, cancel).await
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

    /// Answers with the arguments it was handed, verbatim — the only way to
    /// prove the validator is a *gate* and not a filter.
    struct EchoTool {
        name: &'static str,
        schema: serde_json::Value,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn input_schema(&self) -> serde_json::Value {
            self.schema.clone()
        }
        fn permission_class(&self) -> PermissionClass {
            PermissionClass::ReadOnly
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            ToolResult::ok(input.to_string())
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
        // `register` panics on collision, so building it is most of the test.
        let registry = ToolRegistry::with_builtin_tools();
        let names: Vec<String> = registry.tool_defs().into_iter().map(|d| d.name).collect();

        // Asserted by name rather than by count: a count says "10 tools" when
        // one is missing and another was added, and it makes every new tool a
        // merge conflict for no benefit.
        for expected in [
            "read_file",
            "list_dir",
            "glob",
            "grep",
            "write_file",
            "edit_file",
            "multi_edit",
            "run_bash",
            "ask_user",
            "write_tasks",
            "web_search",
            "web_fetch",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} is not registered: {names:?}"
            );
        }

        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "duplicate tool name in {names:?}"
        );
    }

    /// The real wiring behind checkpointing, checked against the real
    /// built-ins rather than a stand-in: the two mutating file tools declare
    /// the path they will write (resolved, so the agent snapshots the same
    /// absolute path the write lands on), and `run_bash` declares nothing —
    /// which is what makes the agent record it as an uncovered call and
    /// `/rewind` admit it did not undo whatever the shell did.
    #[test]
    fn the_mutating_file_tools_declare_their_path_and_run_bash_declares_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ToolRegistry::with_builtin_tools();
        let ctx = ToolContext::new(dir.path(), "test");

        for tool in ["write_file", "edit_file", "multi_edit"] {
            let paths = registry.snapshot_paths(
                tool,
                &serde_json::json!({"path": "src/main.rs", "content": "x"}),
                &ctx,
            );
            assert_eq!(
                paths,
                vec![dir.path().canonicalize().unwrap().join("src/main.rs")],
                "{tool} did not declare the file it is about to write"
            );
        }

        assert!(registry
            .snapshot_paths(
                "run_bash",
                &serde_json::json!({"command": "rm -rf src"}),
                &ctx
            )
            .is_empty());
        // Read-only tools declare nothing either, but the agent only treats
        // that as a gap above `ReadOnly` — see its `checkpoint_before`.
        assert!(registry
            .snapshot_paths("read_file", &serde_json::json!({"path": "a.txt"}), &ctx)
            .is_empty());
    }

    /// A path the jail refuses yields nothing to snapshot, because that call
    /// is about to fail without touching anything.
    #[test]
    fn a_write_that_escapes_the_project_declares_nothing_to_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let registry = ToolRegistry::with_builtin_tools();
        let ctx = ToolContext::new(dir.path(), "test");
        assert!(registry
            .snapshot_paths(
                "write_file",
                &serde_json::json!({"path": "../escaped.txt", "content": "x"}),
                &ctx
            )
            .is_empty());
    }

    /// The dispatch point rejects a call the schema forbids *before* the tool
    /// runs, so `read_file` never gets to quietly default a bad `offset` to 1.
    #[tokio::test]
    async fn a_call_that_contradicts_the_schema_never_reaches_the_tool() {
        let registry = ToolRegistry::with_builtin_tools();
        let result = registry
            .execute(
                "read_file",
                serde_json::json!({"path": "a.rs", "offset": "abc"}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "read_file: argument \"offset\" must be an integer, but got the string \"abc\"."
        );
    }

    /// A valid call is a pass-through: the tool must receive exactly the
    /// arguments the model sent, unknown keys and all.
    #[tokio::test]
    async fn a_valid_call_reaches_the_tool_byte_identical() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool {
            name: "echo",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "num_results": {"type": "integer", "minimum": 1}
                },
                "required": ["query"]
            }),
        }));

        let input = serde_json::json!({"query": "rust", "num_results": 3});
        let result = registry
            .execute("echo", input.clone(), &ctx(), CancellationToken::new())
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, input.to_string());
    }

    /// `smith_core`'s `align_arguments` passes through argument names it
    /// cannot map onto the schema, on purpose, so the tool can decide. This
    /// layer has to agree — a weak model that sent `region` to `web_search`
    /// must still get its search, not a validation refusal.
    #[tokio::test]
    async fn an_unknown_argument_is_forwarded_rather_than_refused() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool {
            name: "echo",
            schema: serde_json::json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"]
            }),
        }));

        let input = serde_json::json!({"query": "rust", "region": "us-east-1"});
        let result = registry
            .execute("echo", input.clone(), &ctx(), CancellationToken::new())
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, input.to_string());
    }

    /// A remote server's broken schema costs it its own validation and nothing
    /// else. Two things it must not be able to do: make its tool permanently
    /// uncallable (a server that accepts the call anyway would be bricked by
    /// smith), and reach past its own entry to weaken a built-in.
    #[tokio::test]
    async fn a_malformed_remote_schema_cannot_disable_validation_for_a_builtin() {
        let mut registry = ToolRegistry::with_builtin_tools();
        registry
            .try_register(Arc::new(EchoTool {
                name: "mcp__hostile__read_file",
                schema: serde_json::json!("not a schema at all"),
            }))
            .unwrap();

        // The nonsense schema validates nothing, so the call goes through.
        let result = registry
            .execute(
                "mcp__hostile__read_file",
                serde_json::json!({"whatever": 1}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        // The built-in is untouched — schemas are read per tool, from the tool.
        let result = registry
            .execute(
                "read_file",
                serde_json::json!({"offset": 0}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("missing required argument \"path\""),
            "{}",
            result.content
        );
    }

    /// Guards against a schema that declares something no valid call can
    /// satisfy — a typo in `required`, a `minimum` above every sensible value.
    /// The representative call is written by hand rather than generated,
    /// because the point is that the *documented* way to use each tool passes.
    #[test]
    fn every_builtin_schema_accepts_a_representative_good_call() {
        let good: &[(&str, serde_json::Value)] = &[
            (
                "read_file",
                serde_json::json!({"path": "src/main.rs", "offset": 1, "limit": 50, "line_numbers": true}),
            ),
            ("list_dir", serde_json::json!({"path": "src"})),
            (
                "glob",
                serde_json::json!({"pattern": "**/*.rs", "include_hidden": false}),
            ),
            (
                "grep",
                serde_json::json!({
                    "pattern": "fn main", "path": "src", "glob": "*.rs", "type": "rust",
                    "mode": "content", "literal": false, "case_insensitive": true,
                    "include_hidden": false, "before_context": 0, "after_context": 2, "context": 0
                }),
            ),
            (
                "write_file",
                serde_json::json!({"path": "a.txt", "content": "hi"}),
            ),
            (
                "edit_file",
                serde_json::json!({"path": "a.rs", "old_str": "a", "new_str": "b", "replace_all": false}),
            ),
            (
                "multi_edit",
                serde_json::json!({
                    "path": "a.rs",
                    "edits": [{"old_str": "a", "new_str": "b"}, {"old_str": "c", "new_str": "d", "replace_all": true}]
                }),
            ),
            (
                "run_bash",
                serde_json::json!({"command": "ls -la", "timeout_secs": 30}),
            ),
            (
                "ask_user",
                serde_json::json!({
                    "question": "Which?", "option_a": "a", "option_b": "b", "option_c": "c"
                }),
            ),
            (
                "write_tasks",
                serde_json::json!({
                    "tasks": [{"content": "do it", "status": "in_progress"}]
                }),
            ),
            (
                "web_search",
                serde_json::json!({"query": "ratatui", "num_results": 5}),
            ),
            (
                "web_fetch",
                serde_json::json!({"url": "https://example.com", "max_chars": 30000}),
            ),
        ];

        let defs = ToolRegistry::with_builtin_tools().tool_defs();
        for def in &defs {
            let (_, input) = good
                .iter()
                .find(|(name, _)| *name == def.name)
                .unwrap_or_else(|| panic!("{} has no representative call in this test", def.name));
            crate::schema_validate::validate_input(&def.name, &def.input_schema, input)
                .unwrap_or_else(|e| panic!("{}'s own schema rejects a good call: {e}", def.name));
        }
        assert_eq!(
            defs.len(),
            good.len(),
            "a tool was removed but not this list"
        );
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
