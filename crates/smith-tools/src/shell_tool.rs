use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Keeps a runaway command's output from blowing up the conversation context.
/// Counted in `char`s, not bytes — the limit exists to bound how much the
/// model has to read, and that maps to characters far better than to bytes.
const MAX_OUTPUT_CHARS: usize = 20_000;
/// How long a cancelled command gets to shut down on SIGTERM before SIGKILL.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(200);

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

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Give the shell its own process group so cancel/timeout can signal
        // everything it spawned. Signalling `sh` alone leaves grandchildren
        // (`npm run dev` -> node, `cargo build` -> rustc) running forever.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return ToolResult::error(format!("failed to spawn `sh -c`: {e}")),
        };

        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                kill_process_tree(&mut child).await;
                ToolResult::error("command cancelled by user")
            }
            _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
                kill_process_tree(&mut child).await;
                ToolResult::error(format!("command timed out after {timeout_secs}s"))
            }
            (status, stdout, stderr) = wait_with_output(&mut child) => {
                format_result(status, stdout, stderr)
            }
        }
    }
}

/// Kills the shell *and* everything it spawned, by signalling the process
/// group created at spawn time.
#[cfg(unix)]
async fn kill_process_tree(child: &mut Child) {
    // The child is its own group leader (`process_group(0)`), so its pid is
    // also the group id. It hasn't been waited on yet, so the pid can't have
    // been recycled onto some unrelated process.
    if let Some(pid) = child.id() {
        let pgid = pid as libc::pid_t;
        // SAFETY: plain signal delivery to a group we created; the worst a
        // stale pgid can do is return ESRCH, which we ignore.
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
        tokio::time::sleep(KILL_GRACE).await;
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    let _ = child.wait().await;
}

/// Non-Unix fallback: kills only the direct child. Reaching grandchildren on
/// Windows needs a Job Object, which is out of scope here.
#[cfg(not(unix))]
async fn kill_process_tree(child: &mut Child) {
    let _ = child.kill().await;
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
        output.push_str(&truncate_tail(&stdout, MAX_OUTPUT_CHARS));
    }
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("[stderr]\n");
        output.push_str(&truncate_tail(&stderr, MAX_OUTPUT_CHARS));
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

/// Keeps the last `max` *characters* of `text`, dropping the head.
///
/// The tail is what matters when diagnosing a failure, so the end is what
/// survives. Indexing is by `char` boundary rather than byte offset: a byte
/// cut lands mid-character on accented Latin, CJK or emoji and panics.
pub fn truncate_tail(text: &str, max: usize) -> String {
    if max == 0 {
        return "... (truncated)".to_string();
    }
    // Byte offset of the `max`-th character from the end; `None` means the
    // text is shorter than the limit, `Some(0)` that it fits exactly.
    match text.char_indices().nth_back(max - 1) {
        None => text.to_string(),
        Some((0, _)) => text.to_string(),
        Some((start, _)) => format!("... (truncated)\n{}", &text[start..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::PathBuf;

    // Only the `sh`-driven tests need a context, and those are all `cfg(unix)`.
    #[cfg(unix)]
    fn ctx() -> ToolContext {
        ToolContext {
            cwd: PathBuf::from("."),
            session_id: "test-session".into(),
        }
    }

    // --- POSIX shell behaviour --------------------------------------------
    //
    // `RunBashTool` spawns `sh -c` by design, and these tests drive it with
    // `sh` builtins (`echo`, `exit`) and coreutils (`sleep`). Neither `sh` nor
    // `sleep` exists on a stock Windows runner, so on Windows the spawn fails
    // and every assertion below is about a POSIX shell rather than about the
    // tool's own logic. They are gated rather than rewritten: what they cover
    // — signalling a process group, killing a real child on timeout — is
    // genuinely Unix-shaped. Windows would need a Job Object and its own
    // tests (see the `cfg(not(unix))` `kill_process_tree` fallback, which is
    // currently untested).
    //
    // The `truncate_tail` tests further down are pure and stay cross-platform.

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_grandchildren_too() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("grandchild-ran");
        // The backgrounded subshell outlives `sh` unless the whole process
        // group is signalled; it only writes the sentinel after its sleep.
        let command = format!(
            "sleep 2 && touch {} & wait",
            sentinel.to_str().unwrap_or_default()
        );

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel_clone.cancel();
        });
        let result = RunBashTool
            .execute(serde_json::json!({ "command": command }), &ctx(), cancel)
            .await;
        assert!(result.is_error);

        // Well past the grandchild's sleep: if it survived, the file is there.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !sentinel.exists(),
            "grandchild survived cancellation and wrote {}",
            sentinel.display()
        );
    }

    #[cfg(unix)]
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

    #[test]
    fn truncate_tail_survives_multibyte_cut_points() {
        // `€` is 3 bytes wide, so cutting at a byte offset lands inside a
        // character: the old byte-indexed slice panicked here.
        let text = "€".repeat(MAX_OUTPUT_CHARS + 1);
        let out = truncate_tail(&text, MAX_OUTPUT_CHARS);
        assert!(out.starts_with("... (truncated)\n"));
        assert_eq!(out.matches('€').count(), MAX_OUTPUT_CHARS);
    }

    #[test]
    fn truncate_tail_leaves_short_input_untouched() {
        assert_eq!(truncate_tail("hello", 10), "hello");
        assert_eq!(truncate_tail("hello", 5), "hello");
        assert_eq!(truncate_tail("caí três vezes", 64), "caí três vezes");
    }

    #[test]
    fn truncate_tail_keeps_the_tail_not_the_head() {
        assert_eq!(truncate_tail("abcdef", 3), "... (truncated)\ndef");
        assert_eq!(truncate_tail("áéíóú", 2), "... (truncated)\nóú");
    }
}
