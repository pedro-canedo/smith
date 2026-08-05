use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::mpsc;

/// MCP's stdio transport frames each JSON-RPC message as a single line.
/// Spawns a background task that parses each line and forwards it.
pub fn spawn_reader(stdout: ChildStdout, tx: mpsc::UnboundedSender<serde_json::Value>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(line) {
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

pub async fn write_message(
    stdin: &mut ChildStdin,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).expect("serde_json::Value always serializes");
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}
