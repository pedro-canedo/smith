//! What a frontend needs to say and show about MCP servers.
//!
//! These are plain reports, not the client: `smith-core` still knows nothing
//! about processes or HTTP. The types live here for the same reason
//! `RewindReport` does — they cross the `Action`/`AgentEvent` channels, and
//! both ends have to agree on their shape without either depending on the
//! other.

use serde::{Deserialize, Serialize};

/// `/mcp` and its subcommands. One `Action` variant covers the family so the
/// event enum does not grow a arm per subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum McpCommand {
    /// `/mcp` — report every configured server.
    Status,
    /// `/mcp prompt [<server>] <name> [key=value ...]` — fetch a server-supplied
    /// prompt template and run a turn with it.
    Prompt {
        /// `None` when only one server publishes a prompt by that name.
        server: Option<String>,
        name: String,
        arguments: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpHealth {
    /// Connected at startup and still answering.
    Connected,
    /// Connected at startup, but the transport has since closed.
    Disconnected,
    /// Never connected.
    Failed,
}

impl McpHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            McpHealth::Connected => "connected",
            McpHealth::Disconnected => "disconnected",
            McpHealth::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    /// `stdio`, `http` or `sse` — the transport actually in use, which for an
    /// auto-detected URL server is not always the one the user would guess.
    pub transport: String,
    pub health: McpHealth,
    pub tools: usize,
    pub resources: usize,
    pub prompts: usize,
    /// Why it failed, or what version it reported. One line, ready to print.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpStatus {
    pub servers: Vec<McpServerStatus>,
}

impl McpStatus {
    /// The report as lines, ready for a frontend to print one per row. Shared
    /// so the TUI and `stream-json` consumers cannot describe the same servers
    /// differently.
    pub fn lines(&self) -> Vec<String> {
        if self.servers.is_empty() {
            return vec![
                "no MCP servers configured — add an [[mcp_servers]] entry with a `command` \
                 (stdio) or a `url` (http/sse)"
                    .to_string(),
            ];
        }
        self.servers
            .iter()
            .map(|s| {
                let mut line = format!(
                    "{} [{}] {} — {} tool(s), {} resource(s), {} prompt(s)",
                    s.name,
                    s.transport,
                    s.health.as_str(),
                    s.tools,
                    s.resources,
                    s.prompts
                );
                if let Some(detail) = &s.detail {
                    line.push_str(&format!(" — {detail}"));
                }
                line
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(health: McpHealth, detail: Option<&str>) -> McpServerStatus {
        McpServerStatus {
            name: "docs".into(),
            transport: "http".into(),
            health,
            tools: 3,
            resources: 2,
            prompts: 1,
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn no_servers_says_how_to_add_one_rather_than_printing_nothing() {
        let lines = McpStatus::default().lines();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("[[mcp_servers]]"));
    }

    #[test]
    fn a_line_carries_the_transport_the_health_and_every_count() {
        let report = McpStatus {
            servers: vec![
                status(McpHealth::Connected, Some("v1.2.0")),
                status(McpHealth::Failed, Some("connection refused")),
            ],
        };
        let lines = report.lines();
        assert_eq!(
            lines[0],
            "docs [http] connected — 3 tool(s), 2 resource(s), 1 prompt(s) — v1.2.0"
        );
        assert!(lines[1].contains("failed"));
        assert!(lines[1].contains("connection refused"));
    }

    #[test]
    fn the_command_round_trips_through_the_stream_json_shape() {
        let cmd = McpCommand::Prompt {
            server: Some("docs".into()),
            name: "review".into(),
            arguments: vec![("path".into(), "src/lib.rs".into())],
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(serde_json::from_str::<McpCommand>(&json).unwrap(), cmd);
        assert_eq!(
            serde_json::to_string(&McpCommand::Status).unwrap(),
            r#"{"command":"status"}"#
        );
    }
}
