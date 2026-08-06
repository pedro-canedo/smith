//! How JSON-RPC messages reach an MCP server and come back.
//!
//! Every transport is the same shape: a [`Transport`] you push single
//! JSON-RPC messages into, paired with an [`Incoming`] receiver the server's
//! messages arrive on. `McpClient` is written against that pair alone and
//! knows nothing about processes, sockets or SSE framing — which is what lets
//! stdio, Streamable HTTP and HTTP+SSE share one client, one request/response
//! correlator and one set of timeouts.
//!
//! The receiver is separate from the trait rather than a `recv()` method on
//! it because the correlator wants to own it exclusively while `send` is
//! called from every concurrent tool call at once.

use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};

use crate::client::McpError;

/// Messages arriving from the server. Closing this channel is how a transport
/// reports "the server is gone" — `McpClient` turns that into a failure for
/// every request still in flight rather than letting them sit until timeout.
pub type Incoming = mpsc::UnboundedReceiver<Value>;
pub type IncomingSender = mpsc::UnboundedSender<Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Stdio,
    /// MCP "Streamable HTTP": one endpoint, POST per message, replies come
    /// back either as a JSON body or as an SSE stream on that same response.
    Http,
    /// The older HTTP+SSE pair: a long-lived GET for server→client messages
    /// and a POST endpoint the server names in its first event.
    Sse,
}

impl TransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::Http => "http",
            TransportKind::Sse => "sse",
        }
    }

    /// Parses the config's `transport = "..."` field.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stdio" => Some(TransportKind::Stdio),
            "http" | "streamable-http" | "streamable_http" => Some(TransportKind::Http),
            "sse" | "http-sse" | "http+sse" => Some(TransportKind::Sse),
            _ => None,
        }
    }
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// Delivers one JSON-RPC message. Returning `Ok` means "handed off", not
    /// "answered" — a response, if there is one, arrives on [`Incoming`].
    async fn send(&self, message: &Value) -> Result<(), McpError>;

    fn kind(&self) -> TransportKind;
}

// ---------------------------------------------------------------------------
// stdio
// ---------------------------------------------------------------------------

/// A child process speaking newline-delimited JSON-RPC on its stdin/stdout.
pub struct StdioTransport {
    stdin: Mutex<ChildStdin>,
    // Kept alive for the transport's lifetime; `kill_on_drop` cleans it up.
    _child: Mutex<Child>,
}

impl StdioTransport {
    pub async fn spawn(command: &str, args: &[String]) -> Result<(Self, Incoming), McpError> {
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

        let (tx, rx) = mpsc::unbounded_channel();
        spawn_reader(stdout, tx);

        Ok((
            Self {
                stdin: Mutex::new(stdin),
                _child: Mutex::new(child),
            },
            rx,
        ))
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&self, message: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(message).expect("serde_json::Value always serializes");
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Stdio
    }
}

/// MCP's stdio transport frames each JSON-RPC message as a single line.
/// Spawns a background task that parses each line and forwards it.
///
/// A line that will not parse is dropped with a warning rather than killing
/// the reader: servers write diagnostics to stdout more often than they should
/// and one stray line must not take the session's other requests down with it.
/// The loop ends when the child closes stdout, and dropping `tx` there is the
/// signal `McpClient` reads as "this server is gone".
fn spawn_reader(stdout: ChildStdout, tx: IncomingSender) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(line) {
                        Ok(value) => {
                            if tx.send(value).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("mcp: failed to parse line as JSON-RPC: {e}");
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("mcp: error reading from server stdout: {e}");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_names_round_trip_through_the_config_spelling() {
        for kind in [
            TransportKind::Stdio,
            TransportKind::Http,
            TransportKind::Sse,
        ] {
            assert_eq!(TransportKind::parse(kind.as_str()), Some(kind));
        }
        // Tolerant of case and of the spellings the two HTTP transports go by
        // in the spec and in other clients' config files.
        assert_eq!(
            TransportKind::parse(" Streamable-HTTP "),
            Some(TransportKind::Http)
        );
        assert_eq!(TransportKind::parse("HTTP+SSE"), Some(TransportKind::Sse));
        assert_eq!(TransportKind::parse("carrier-pigeon"), None);
    }

    #[tokio::test]
    async fn a_child_that_exits_closes_the_incoming_channel() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: python3 not available");
            return;
        }
        let (_transport, mut rx) = StdioTransport::spawn(
            "python3",
            &["-c".to_string(), "print('not json'); ".to_string()],
        )
        .await
        .expect("spawns");
        // The unparseable line is dropped, and the channel closes when the
        // child exits — that close is the liveness signal the client needs.
        assert!(rx.recv().await.is_none());
    }
}
