//! Every configured MCP server, connected once and then queried for the rest
//! of the session.
//!
//! The one thing this type exists to get right is **startup**. Servers are
//! connected concurrently and each is given a hard budget: `N` servers cost
//! the slowest one, not the sum, and a server that hangs during its handshake
//! costs [`CONNECT_TIMEOUT`] and is then written off — it can never wedge the
//! session, which is what a serial connect with no deadline could do.

use std::sync::Arc;
use std::time::Duration;

use smith_config::McpServerConfig;
use smith_core::mcp::{McpHealth, McpServerStatus, McpStatus};
use smith_core::Tool;

use crate::bridge::{ListMcpResourcesTool, McpToolAdapter, ReadMcpResourceTool};
use crate::client::{McpClient, McpPromptDef, McpResourceDef, McpToolDef};
use crate::transport::TransportKind;

/// Total budget for one server's connect + handshake + inventory. Generous
/// enough for a cold `npx` download on a slow link, short enough that a wedged
/// server is a delay rather than a hang.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// One configured server, connected or not.
pub struct ConnectedServer {
    pub name: String,
    pub transport: Option<TransportKind>,
    /// `None` when the server never connected — `error` says why.
    pub client: Option<Arc<McpClient>>,
    pub error: Option<String>,
    pub tools: Vec<McpToolDef>,
    pub resources: Vec<McpResourceDef>,
    pub prompts: Vec<McpPromptDef>,
}

impl ConnectedServer {
    fn failed(name: &str, error: String) -> Self {
        Self {
            name: name.to_string(),
            transport: None,
            client: None,
            error: Some(error),
            tools: Vec::new(),
            resources: Vec::new(),
            prompts: Vec::new(),
        }
    }

    fn status(&self) -> McpServerStatus {
        let health = match &self.client {
            None => McpHealth::Failed,
            Some(c) if c.is_alive() => McpHealth::Connected,
            Some(_) => McpHealth::Disconnected,
        };
        let detail = match (&self.error, &self.client) {
            (Some(e), _) => Some(e.clone()),
            (None, Some(c)) if !c.is_alive() => {
                Some("the server exited after connecting".to_string())
            }
            (None, Some(c)) => c.server_version().map(|v| format!("v{v}")),
            (None, None) => None,
        };
        McpServerStatus {
            name: self.name.clone(),
            transport: self
                .transport
                .map(TransportKind::as_str)
                .unwrap_or("-")
                .to_string(),
            health,
            tools: self.tools.len(),
            resources: self.resources.len(),
            prompts: self.prompts.len(),
            detail,
        }
    }
}

#[derive(Default)]
pub struct McpRegistry {
    servers: Vec<ConnectedServer>,
}

impl McpRegistry {
    /// Connects every entry, concurrently, each under [`CONNECT_TIMEOUT`].
    ///
    /// Never fails: a server that cannot be reached becomes a `Failed` entry
    /// with a reason, because one bad server must not cost the user the
    /// session — nor the other servers.
    pub async fn connect_all(specs: &[McpServerConfig]) -> Self {
        let servers = futures::future::join_all(specs.iter().map(|spec| async move {
            match tokio::time::timeout(CONNECT_TIMEOUT, connect_one(spec)).await {
                Ok(server) => server,
                Err(_) => ConnectedServer::failed(
                    &spec.name,
                    format!(
                        "did not finish connecting within {}s",
                        CONNECT_TIMEOUT.as_secs()
                    ),
                ),
            }
        }))
        .await;
        Self { servers }
    }

    pub fn servers(&self) -> &[ConnectedServer] {
        &self.servers
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// One line per server that failed, for the frontend to surface. Success
    /// is silent; `/mcp` is where the full picture lives.
    pub fn problems(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter_map(|s| {
                s.error
                    .as_ref()
                    .map(|e| format!("mcp server '{}': {e}", s.name))
            })
            .collect()
    }

    pub fn status(&self) -> McpStatus {
        McpStatus {
            servers: self.servers.iter().map(ConnectedServer::status).collect(),
        }
    }

    /// Every tool to register: one adapter per remote tool, plus the two
    /// resource tools — and those only when some server actually publishes
    /// resources. A schema nobody can use still costs tokens on every request.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for server in &self.servers {
            let Some(client) = &server.client else {
                continue;
            };
            for def in &server.tools {
                tools.push(Arc::new(McpToolAdapter::new(client.clone(), def.clone())));
            }
        }

        let with_resources: Vec<Arc<McpClient>> = self
            .servers
            .iter()
            .filter(|s| !s.resources.is_empty())
            .filter_map(|s| s.client.clone())
            .collect();
        if !with_resources.is_empty() {
            tools.push(Arc::new(ListMcpResourcesTool::new(with_resources.clone())));
            tools.push(Arc::new(ReadMcpResourceTool::new(with_resources)));
        }
        tools
    }

    /// `(server, prompt)` for every prompt on offer.
    ///
    /// Public and deliberately shaped for a caller that wants to publish one
    /// slash command per prompt — see `render_prompt`.
    pub fn prompts(&self) -> Vec<(&str, &McpPromptDef)> {
        self.servers
            .iter()
            .flat_map(|s| s.prompts.iter().map(move |p| (s.name.as_str(), p)))
            .collect()
    }

    /// Fetches a prompt template and renders it as the text of a user message.
    ///
    /// **Not fenced as untrusted data, and that is the decision.** A prompt is
    /// an instruction by construction — fencing one would leave the model
    /// text it has been told to ignore. What makes it safe enough is that it
    /// is not model-reachable: no tool exposes it, so it can only enter the
    /// conversation because the user typed its name, which puts it at the same
    /// trust level as anything else the user types. It still carries
    /// provenance, so the model can see the words are the server's and not
    /// the user's own.
    pub async fn render_prompt(
        &self,
        server: Option<&str>,
        name: &str,
        arguments: &[(String, String)],
    ) -> Result<String, String> {
        let mut matches = self
            .servers
            .iter()
            .filter(|s| server.is_none_or(|want| want == s.name))
            .filter(|s| s.prompts.iter().any(|p| p.name == name));

        let Some(target) = matches.next() else {
            return Err(match server {
                Some(s) => format!("MCP server `{s}` publishes no prompt named `{name}`"),
                None => format!(
                    "no connected MCP server publishes a prompt named `{name}`. Available: {}",
                    self.prompt_list()
                ),
            });
        };
        if matches.next().is_some() {
            return Err(format!(
                "several MCP servers publish a prompt named `{name}` — say which, e.g. \
                 `/mcp prompt <server> {name}`"
            ));
        }
        let client = target
            .client
            .as_ref()
            .ok_or_else(|| format!("MCP server `{}` is not connected", target.name))?;

        let def = target
            .prompts
            .iter()
            .find(|p| p.name == name)
            .expect("filtered on this above");
        let missing: Vec<&str> = def
            .arguments
            .iter()
            .filter(|a| a.required && !arguments.iter().any(|(k, _)| k == &a.name))
            .map(|a| a.name.as_str())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "prompt `{name}` needs {} — pass them as key=value",
                missing.join(", ")
            ));
        }

        let rendered = client
            .get_prompt(name, arguments)
            .await
            .map_err(|e| format!("MCP server `{}`: {e}", target.name))?;

        let mut out = format!(
            "[The text below is the `{name}` prompt template from MCP server `{}`, inserted \
             because I asked for it by name.]\n",
            target.name
        );
        if !rendered.description.is_empty() {
            out.push_str(&format!("[{}]\n", rendered.description));
        }
        for message in &rendered.messages {
            // Roles are kept as labels rather than as real turns: a server
            // that supplies an "assistant" message is scripting words into
            // the model's mouth, and a label the model can see is honest
            // where a forged turn would not be.
            if message.role != "user" {
                out.push_str(&format!("[{} said] ", message.role));
            }
            out.push_str(&message.text);
            out.push('\n');
        }
        Ok(out.trim_end().to_string())
    }

    fn prompt_list(&self) -> String {
        let names: Vec<String> = self
            .prompts()
            .iter()
            .map(|(server, p)| format!("{server}:{}", p.name))
            .collect();
        if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        }
    }
}

/// Connects one server and pulls its inventory. Every optional half is
/// best-effort: a server whose `resources/list` fails is still a server whose
/// tools work.
async fn connect_one(spec: &McpServerConfig) -> ConnectedServer {
    let client = match McpClient::connect(&spec.name, spec).await {
        Ok(client) => Arc::new(client),
        Err(e) => return ConnectedServer::failed(&spec.name, e.to_string()),
    };

    let transport = client.transport_kind();
    let caps = client.capabilities();

    let tools = if caps.tools {
        match client.list_tools().await {
            Ok(tools) => tools,
            Err(e) => {
                return ConnectedServer {
                    error: Some(format!("connected, but tools/list failed: {e}")),
                    transport: Some(transport),
                    client: Some(client),
                    name: spec.name.clone(),
                    tools: Vec::new(),
                    resources: Vec::new(),
                    prompts: Vec::new(),
                }
            }
        }
    } else {
        Vec::new()
    };

    let resources = if caps.resources {
        client.list_resources().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let prompts = if caps.prompts {
        client.list_prompts().await.unwrap_or_default()
    } else {
        Vec::new()
    };

    ConnectedServer {
        name: spec.name.clone(),
        transport: Some(transport),
        client: Some(client),
        error: None,
        tools,
        resources,
        prompts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::{python3_available, stdio_spec, FAKE_SERVER};

    #[tokio::test]
    async fn a_connected_server_publishes_tools_resource_tools_and_prompts() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let registry = McpRegistry::connect_all(&[stdio_spec("docs", FAKE_SERVER)]).await;
        assert!(registry.problems().is_empty());

        let status = registry.status();
        assert_eq!(status.servers.len(), 1);
        assert_eq!(status.servers[0].transport, "stdio");
        assert_eq!(status.servers[0].health, McpHealth::Connected);
        assert_eq!(status.servers[0].tools, 1);
        assert_eq!(status.servers[0].resources, 2);
        assert_eq!(status.servers[0].prompts, 1);

        let names: Vec<String> = registry
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert!(names.contains(&"mcp__docs__echo".to_string()));
        // The resource tools appear because this server publishes resources.
        assert!(names.contains(&"list_mcp_resources".to_string()));
        assert!(names.contains(&"read_mcp_resource".to_string()));
    }

    /// The resource tools are not registered at all when nothing offers
    /// resources — an unusable schema still costs tokens on every request.
    #[tokio::test]
    async fn no_resources_means_no_resource_tools() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let tools_only = FAKE_SERVER.replace(
            r#""capabilities": {"tools": {}, "resources": {}, "prompts": {}}"#,
            r#""capabilities": {"tools": {}}"#,
        );
        let registry = McpRegistry::connect_all(&[stdio_spec("docs", &tools_only)]).await;
        let names: Vec<String> = registry
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(names, vec!["mcp__docs__echo".to_string()]);
        assert_eq!(registry.status().servers[0].resources, 0);
    }

    #[tokio::test]
    async fn a_prompt_is_rendered_with_provenance_and_missing_arguments_are_named() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let registry = McpRegistry::connect_all(&[stdio_spec("docs", FAKE_SERVER)]).await;
        assert_eq!(registry.prompts().len(), 1);

        let text = registry
            .render_prompt(None, "review", &[("path".into(), "src/lib.rs".into())])
            .await
            .unwrap();
        assert!(text.contains("Please review src/lib.rs"));
        assert!(text.contains("MCP server `docs`"));
        assert!(text.contains("because I asked for it by name"));

        let missing = registry
            .render_prompt(None, "review", &[])
            .await
            .unwrap_err();
        assert!(missing.contains("path"), "{missing}");

        let unknown = registry.render_prompt(None, "nope", &[]).await.unwrap_err();
        assert!(unknown.contains("docs:review"), "{unknown}");
    }

    /// Two servers publishing the same prompt name is ambiguous, and guessing
    /// would run the wrong server's instructions.
    #[tokio::test]
    async fn an_ambiguous_prompt_name_is_refused_until_qualified() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let registry =
            McpRegistry::connect_all(&[stdio_spec("a", FAKE_SERVER), stdio_spec("b", FAKE_SERVER)])
                .await;
        let err = registry
            .render_prompt(None, "review", &[])
            .await
            .unwrap_err();
        assert!(err.contains("several MCP servers"), "{err}");

        let ok = registry
            .render_prompt(Some("b"), "review", &[("path".into(), "x".into())])
            .await
            .unwrap();
        assert!(ok.contains("MCP server `b`"));
    }

    /// One unreachable server costs itself and nothing else — the others
    /// still connect, and the failure is a line the user can read.
    #[tokio::test]
    async fn a_broken_server_does_not_take_the_others_down() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        let registry = McpRegistry::connect_all(&[
            McpServerConfig {
                name: "ghost".into(),
                command: "definitely-not-a-real-binary-xyz".into(),
                ..Default::default()
            },
            stdio_spec("docs", FAKE_SERVER),
        ])
        .await;

        let problems = registry.problems();
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("ghost"), "{}", problems[0]);

        let status = registry.status();
        assert_eq!(status.servers[0].health, McpHealth::Failed);
        assert_eq!(status.servers[1].health, McpHealth::Connected);
        assert!(registry
            .tools()
            .iter()
            .any(|t| t.name() == "mcp__docs__echo"));
    }

    /// A server that hangs before the handshake completes is written off
    /// after its budget rather than wedging startup forever.
    #[tokio::test(start_paused = true)]
    async fn a_hanging_server_is_bounded_by_the_connect_budget() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        // Reads stdin and never answers anything.
        let hangs = stdio_spec("hangs", "import sys\nfor line in sys.stdin:\n    pass\n");
        let started = tokio::time::Instant::now();
        let registry = McpRegistry::connect_all(&[hangs]).await;
        // Auto-advanced virtual time: the assertion is that the wait is
        // bounded by our budget, not by the 30s per-request timeout.
        assert!(started.elapsed() <= CONNECT_TIMEOUT);
        assert_eq!(registry.status().servers[0].health, McpHealth::Failed);
        assert!(registry.problems()[0].contains("within"));
    }

    /// A server that answers its inventory and then exits is `Disconnected`,
    /// not `Failed` and not `Connected`: its tools are still registered but
    /// every call will now fail, and `/mcp` has to say which of the two it is.
    #[tokio::test]
    async fn a_server_that_dies_after_connecting_reads_as_disconnected() {
        if !python3_available() {
            eprintln!("skipping: python3 not available");
            return;
        }
        const BRIEF: &str = r#"
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
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"capabilities": {"tools": {}}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}})
        sys.exit(0)
"#;
        let registry = McpRegistry::connect_all(&[stdio_spec("brief", BRIEF)]).await;
        assert_eq!(registry.status().servers[0].tools, 1);

        let client = registry.servers()[0].client.clone().expect("connected");
        for _ in 0..100 {
            if !client.is_alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!client.is_alive(), "the child should have exited by now");

        let status = registry.status();
        assert_eq!(status.servers[0].health, McpHealth::Disconnected);
        assert!(status.servers[0]
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("exited after connecting")));
    }
}
