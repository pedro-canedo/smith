use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::transport;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("failed to spawn MCP server: {0}")]
    Spawn(std::io::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("server returned an error: {0}")]
    Server(String),
    #[error("unexpected response shape: {0}")]
    Protocol(String),
    #[error("request timed out")]
    Timeout,
    #[error("client is shutting down")]
    Closed,
}

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct ToolCallOutcome {
    pub text: String,
    pub is_error: bool,
}

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// A JSON-RPC 2.0 client over an MCP server's stdio transport. Deliberately
/// hand-rolled rather than pulling in the `rmcp` SDK: MCP's stdio wire format
/// is just newline-delimited JSON-RPC with three methods we need
/// (`initialize`, `tools/list`, `tools/call`), which keeps this a small,
/// fully-understood surface instead of an external API-compatibility risk.
pub struct McpClient {
    name: String,
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: PendingMap,
    next_id: AtomicU64,
    // Kept alive for the client's lifetime; `kill_on_drop` cleans it up.
    _child: Mutex<Child>,
}

impl McpClient {
    pub async fn connect(name: &str, command: &str, args: &[String]) -> Result<Self, McpError> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(McpError::Spawn)?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("child has no stdout".into()))?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        transport::spawn_reader(stdout, tx);

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else {
                    continue; // notification; nothing we need to act on yet
                };
                let mut pending = pending_reader.lock().await;
                if let Some(tx) = pending.remove(&id) {
                    if let Some(err) = msg.get("error") {
                        let message = err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("mcp error")
                            .to_string();
                        let _ = tx.send(Err(message));
                    } else {
                        let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
                    }
                }
            }
        });

        let client = Self {
            name: name.to_string(),
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        };

        client
            .call(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "smith", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;

        Ok(client)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = transport::write_message(&mut stdin, &request).await {
                self.pending.lock().await.remove(&id);
                return Err(McpError::Io(e));
            }
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(err))) => Err(McpError::Server(err)),
            Ok(Err(_)) => Err(McpError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout)
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let notification = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let mut stdin = self.stdin.lock().await;
        transport::write_message(&mut stdin, &notification)
            .await
            .map_err(McpError::Io)
    }

    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let result = self.call("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .ok_or_else(|| McpError::Protocol("tools/list: missing tools array".into()))?;

        Ok(tools
            .iter()
            .filter_map(|t| {
                Some(McpToolDef {
                    name: t.get("name")?.as_str()?.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                })
            })
            .collect())
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<ToolCallOutcome, McpError> {
        let result = self
            .call(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        Ok(ToolCallOutcome { text, is_error })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny Python MCP server speaking exactly the subset of the protocol
    /// McpClient needs, used to test the client end-to-end without depending
    /// on a real MCP server being installed.
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
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "0.0.1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{"name": "echo", "description": "echoes text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}})
    elif method == "tools/call":
        args = req["params"]["arguments"]
        if req["params"]["name"] == "echo":
            send({"jsonrpc": "2.0", "id": req["id"], "result": {"content": [{"type": "text", "text": args.get("text", "")}], "isError": False}})
        else:
            send({"jsonrpc": "2.0", "id": req["id"], "error": {"message": "unknown tool"}})
"#;

    async fn connect_fake() -> Option<McpClient> {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return None; // skip gracefully where python3 isn't available
        }
        McpClient::connect(
            "fake",
            "python3",
            &["-c".to_string(), FAKE_SERVER.to_string()],
        )
        .await
        .ok()
    }

    #[tokio::test]
    async fn lists_and_calls_tools_against_a_real_child_process() {
        let Some(client) = connect_fake().await else {
            eprintln!("skipping: python3 not available");
            return;
        };

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let outcome = client
            .call_tool("echo", json!({"text": "hello mcp"}))
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert_eq!(outcome.text, "hello mcp");

        let error_outcome = client.call_tool("nonexistent", json!({})).await;
        assert!(error_outcome.is_err());
    }
}
