use std::sync::Arc;

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

use crate::client::{McpClient, McpToolDef};

/// Builds the name an MCP tool is exposed under: `mcp__{server}__{tool}`.
/// The double underscore keeps the split unambiguous when a server or tool
/// name itself contains single underscores.
pub fn namespaced_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// Adapts one remote MCP tool to smith_core::Tool so the agent's orchestration
/// loop can call it exactly like a built-in. Always `Dangerous`: an arbitrary
/// MCP server's tool semantics can't be statically trusted, so it always
/// prompts unless the user grants it for the session.
pub struct McpToolAdapter {
    client: Arc<McpClient>,
    def: McpToolDef,
    /// Namespaced name the model and the registry see. A server must not be
    /// able to publish a bare `read_file` and have the model call it believing
    /// it got the sandboxed built-in — nor to shadow another server's tool.
    /// The `tools/call` wire request still carries `def.name`.
    exposed_name: String,
}

impl McpToolAdapter {
    pub fn new(client: Arc<McpClient>, def: McpToolDef) -> Self {
        let exposed_name = namespaced_tool_name(client.name(), &def.name);
        Self {
            client,
            def,
            exposed_name,
        }
    }

    /// The tool's original name on the remote server — what goes on the wire.
    pub fn remote_name(&self) -> &str {
        &self.def.name
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        &self.exposed_name
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake MCP server publishing a tool that collides with a built-in
    /// (`read_file`); `tools/call` answers with the name it was asked for, so
    /// a test can see exactly what went over the wire.
    const FAKE_SERVER: &str = r#"
import sys, json

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{"name": "read_file", "description": "reads a file on the remote host", "inputSchema": {"type": "object"}}]}})
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"content": [{"type": "text", "text": "called:" + req["params"]["name"]}], "isError": False}})
"#;

    fn ctx() -> ToolContext {
        ToolContext {
            cwd: std::env::temp_dir(),
            session_id: "test".to_string(),
        }
    }

    #[test]
    fn namespacing_survives_underscores_in_both_halves() {
        assert_eq!(
            namespaced_tool_name("my_server", "read_file"),
            "mcp__my_server__read_file"
        );
    }

    #[tokio::test]
    async fn exposes_a_namespaced_name_but_calls_the_remote_one() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 not available");
            return;
        }
        let client = McpClient::connect(
            "files",
            "python3",
            &["-c".to_string(), FAKE_SERVER.to_string()],
        )
        .await
        .expect("fake server connects");
        let def = client.list_tools().await.unwrap().remove(0);
        let adapter = McpToolAdapter::new(Arc::new(client), def);

        assert_eq!(adapter.name(), "mcp__files__read_file");
        assert_eq!(adapter.remote_name(), "read_file");
        assert_eq!(adapter.description(), "reads a file on the remote host");
        assert_eq!(adapter.definition().name, "mcp__files__read_file");

        // The prefix is for the model only — the server is asked for the tool
        // by the name it published.
        let result = adapter
            .execute(serde_json::json!({}), &ctx(), CancellationToken::new())
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content, "called:read_file");
    }
}
