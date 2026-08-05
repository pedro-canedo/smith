use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Keeps a runaway command's output from blowing up the conversation context.
const MAX_OUTPUT_CHARS: usize = 20_000;

pub struct RunBashTool;

#[async_trait]
impl Tool for RunBashTool {
    fn name(&self) -> &str {
        "run_bash"
    }

    fn description(&self) -> &str {
        "Run a shell command via `sh -c` in the project directory. Args: command (required), timeout_secs (optional, default 120)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_secs": {"type": "integer", "minimum": 1}
            },
            "required": ["command"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Dangerous
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
            return ToolResult::error("missing required field: command");
        };
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let mut child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return ToolResult::error(format!("failed to spawn `sh -c`: {e}")),
        };

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                ToolResult::error("command cancelled by user")
            }
            _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
                let _ = child.kill().await;
                ToolResult::error(format!("command timed out after {timeout_secs}s"))
            }
            (status, stdout, stderr) = wait_with_output(&mut child) => {
                format_result(status, stdout, stderr)
            }
        }
    }
}

async fn wait_with_output(
    child: &mut Child,
) -> (std::io::Result<std::process::ExitStatus>, String, String) {
    let stdout_fut = async {
        let mut buf = String::new();
        if let Some(mut pipe) = child.stdout.take() {
            let _ = pipe.read_to_string(&mut buf).await;
        }
        buf
    };
    let stderr_fut = async {
        let mut buf = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut buf).await;
        }
        buf
    };
    let (stdout, stderr) = tokio::join!(stdout_fut, stderr_fut);
    let status = child.wait().await;
    (status, stdout, stderr)
}

fn format_result(
    status: std::io::Result<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
) -> ToolResult {
    let status = match status {
        Ok(s) => s,
        Err(e) => return ToolResult::error(format!("failed to wait on command: {e}")),
    };

    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(&truncate(&stdout));
    }
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("[stderr]\n");
        output.push_str(&truncate(&stderr));
    }
    if output.is_empty() {
        output.push_str("(no output)");
    }

    if status.success() {
        ToolResult::ok(output)
    } else {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        ToolResult::error(format!("exit code {code}\n{output}"))
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_CHARS {
        s.to_string()
    } else {
        let tail = &s[s.len() - MAX_OUTPUT_CHARS..];
        format!("... (truncated)\n{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext {
            cwd: PathBuf::from("."),
            session_id: "test-session".into(),
        }
    }

    #[tokio::test]
    async fn runs_command_and_captures_stdout() {
        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "echo hello"}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn non_zero_exit_is_an_error() {
        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "exit 3"}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("exit code 3"));
    }

    #[tokio::test]
    async fn cancellation_kills_a_long_running_command() {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        let start = std::time::Instant::now();
        let result = RunBashTool
            .execute(serde_json::json!({"command": "sleep 30"}), &ctx(), cancel)
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "command wasn't killed promptly"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_command() {
        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "sleep 30", "timeout_secs": 1}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("timed out"));
    }
}
