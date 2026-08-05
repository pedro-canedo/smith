use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines};
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

/// Minimum gap between two `ToolProgress` events, across *both* pipes.
///
/// The alternative — one event per line — makes a noisy build (`cargo build`
/// emits tens of thousands of lines) push tens of thousands of events through
/// a channel whose only consumers are a one-line activity display and a
/// `stream-json` writer. Sampling rather than batching is deliberate: the
/// event type is documented as one *line*, advisory, and the complete output
/// is still returned as the tool result, so nothing is actually lost by
/// showing every tenth-of-a-second instead of every line. What *is* skipped is
/// announced (`... (N lines omitted)`), so the progress stream never
/// silently pretends to be complete.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Ceiling on what one pipe keeps in memory while the command runs, and how
/// much of the tail survives when that ceiling is hit.
///
/// Streaming means output is accumulated line by line for the whole run, so an
/// unbounded buffer turns `yes > /dev/stdout` into an OOM. Both numbers are
/// far above `MAX_OUTPUT_CHARS` even at 4 bytes per character, so dropping
/// here can never be the *only* truncation: `truncate_tail` always fires
/// afterwards and always leaves its marker.
const CAPTURE_LIMIT_BYTES: usize = 512 * 1024;
const CAPTURE_KEEP_BYTES: usize = 256 * 1024;

/// How long to keep reading the pipes after a cancelled/timed-out command has
/// been killed. Bounded because a surviving process holding the write end
/// would otherwise keep us here forever; short because everything still in the
/// pipe at this point is already-written bytes, not new work.
const DRAIN_BUDGET: Duration = Duration::from_millis(200);

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

        let (outcome, stdout, stderr) = stream_output(&mut child, ctx, &cancel, timeout_secs).await;
        format_result(outcome, stdout, stderr)
    }
}

/// How the command stopped. Every variant carries whatever output had already
/// been produced, which is the point of the whole streaming rewrite: reading
/// both pipes to EOF *before* looking at cancellation meant an interrupted
/// build returned nothing at all, not even the half that had already run.
enum Outcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    Cancelled,
    TimedOut(u64),
}

/// Drives the child to completion (or to cancellation/timeout), reporting
/// lines as they arrive and accumulating them for the final result.
///
/// One `select!` loop rather than a reader future raced against a canceller:
/// racing loses the partial output with the dropped future, whereas keeping
/// cancellation *inside* the loop leaves the buffers in scope on every exit
/// path.
async fn stream_output(
    child: &mut Child,
    ctx: &ToolContext,
    cancel: &CancellationToken,
    timeout_secs: u64,
) -> (Outcome, String, String) {
    let mut out_lines = child.stdout.take().map(|pipe| BufReader::new(pipe).lines());
    let mut err_lines = child.stderr.take().map(|pipe| BufReader::new(pipe).lines());
    let mut stdout_buf = TailBuffer::default();
    let mut stderr_buf = TailBuffer::default();
    let mut throttle = Throttle::default();
    let mut status: Option<std::io::Result<std::process::ExitStatus>> = None;

    let deadline = tokio::time::sleep(Duration::from_secs(timeout_secs));
    tokio::pin!(deadline);

    let outcome = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break Outcome::Cancelled,
            _ = &mut deadline => break Outcome::TimedOut(timeout_secs),
            line = next_line(&mut out_lines), if out_lines.is_some() => {
                if let Some(line) = line {
                    record(&line, false, &mut stdout_buf, &mut throttle, ctx);
                }
            }
            line = next_line(&mut err_lines), if err_lines.is_some() => {
                if let Some(line) = line {
                    record(&line, true, &mut stderr_buf, &mut throttle, ctx);
                }
            }
            exited = child.wait(), if status.is_none() => status = Some(exited),
        }
        // Checked after *every* wake-up rather than as a `select!` pattern: a
        // branch disabled by a non-matching pattern stops the loop from ever
        // re-testing the exit condition, which hangs a finished command whose
        // last pipe reached EOF after the process was reaped.
        if out_lines.is_none() && err_lines.is_none() && status.is_some() {
            break Outcome::Exited(status.take().unwrap_or_else(|| {
                Err(std::io::Error::other("child status unexpectedly missing"))
            }));
        }
    };

    if !matches!(outcome, Outcome::Exited(_)) {
        kill_process_tree(child).await;
        // The pipes hold whatever the command wrote but we hadn't read yet —
        // in particular a final, unterminated line, which `next_line` only
        // yields at EOF. Killing the group closed the write ends, so this
        // terminates on its own; the budget is only there for a write end that
        // somehow outlived the kill.
        let _ = tokio::time::timeout(DRAIN_BUDGET, async {
            while out_lines.is_some() || err_lines.is_some() {
                tokio::select! {
                    line = next_line(&mut out_lines), if out_lines.is_some() => {
                        if let Some(line) = line {
                            record(&line, false, &mut stdout_buf, &mut throttle, ctx);
                        }
                    }
                    line = next_line(&mut err_lines), if err_lines.is_some() => {
                        if let Some(line) = line {
                            record(&line, true, &mut stderr_buf, &mut throttle, ctx);
                        }
                    }
                }
            }
        })
        .await;
    }

    // Whatever the last interval suppressed would otherwise never be
    // accounted for: the count is only reported by the *next* emission, and a
    // command that finishes inside one interval never has one.
    if let Some(skipped) = throttle.finish() {
        ctx.report_progress(format!("... ({skipped} lines omitted)"));
    }

    (outcome, stdout_buf.into_string(), stderr_buf.into_string())
}

/// One line, or `None` when the stream has finished — in which case the reader
/// is dropped so its `select!` branch turns itself off.
///
/// Callers must guard the branch with `is_some()`; the `?` below is only a
/// safety net, and a branch that fired on an already-finished stream would
/// spin the loop.
async fn next_line<R>(lines: &mut Option<Lines<R>>) -> Option<String>
where
    R: AsyncBufRead + Unpin,
{
    let reader = lines.as_mut()?;
    match reader.next_line().await {
        Ok(Some(line)) => Some(line),
        // EOF and read errors are the same thing here: nothing more is coming.
        _ => {
            *lines = None;
            None
        }
    }
}

fn record(
    line: &str,
    is_stderr: bool,
    buf: &mut TailBuffer,
    throttle: &mut Throttle,
    ctx: &ToolContext,
) {
    buf.push_line(line);
    let Some(skipped) = throttle.offer() else {
        return;
    };
    if skipped > 0 {
        ctx.report_progress(format!("... ({skipped} lines omitted)"));
    }
    // Tagged in the live stream for the same reason the final result separates
    // them: "error: ..." on stdout and on stderr mean different things.
    if is_stderr {
        ctx.report_progress(format!("[stderr] {line}"));
    } else {
        ctx.report_progress(line.to_string());
    }
}

/// Rate limiter for progress events, counting what it suppresses so the gap
/// can be reported rather than hidden.
#[derive(Default)]
struct Throttle {
    /// Defaulting to `None` rather than "now minus the interval" is what makes
    /// the first line go out immediately — which is the whole difference
    /// between a 90-second build looking like work and looking like a hang.
    /// (It also avoids `Instant` subtraction, which can panic on underflow.)
    last: Option<Instant>,
    skipped: u64,
}

impl Throttle {
    /// `Some(skipped)` when this line should be emitted, carrying how many
    /// were dropped since the last emission.
    fn offer(&mut self) -> Option<u64> {
        let now = Instant::now();
        let due = match self.last {
            None => true,
            Some(last) => now.duration_since(last) >= PROGRESS_INTERVAL,
        };
        if due {
            self.last = Some(now);
            Some(std::mem::take(&mut self.skipped))
        } else {
            self.skipped += 1;
            None
        }
    }

    /// Lines suppressed since the last emission and never reported, if any.
    fn finish(&mut self) -> Option<u64> {
        let skipped = std::mem::take(&mut self.skipped);
        (skipped > 0).then_some(skipped)
    }
}

/// A growing capture of one pipe that discards its *head* once it gets too
/// big — matching `truncate_tail`, which keeps the end for the same reason:
/// the tail is what diagnoses a failure.
#[derive(Default)]
struct TailBuffer {
    text: String,
}

impl TailBuffer {
    fn push_line(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
        if self.text.len() > CAPTURE_LIMIT_BYTES {
            self.drop_head();
        }
    }

    fn drop_head(&mut self) {
        let mut cut = self.text.len().saturating_sub(CAPTURE_KEEP_BYTES);
        // Byte-index arithmetic again, so again the char boundary has to be
        // respected explicitly or accented output panics the drain.
        while cut < self.text.len() && !self.text.is_char_boundary(cut) {
            cut += 1;
        }
        if let Some(newline) = self.text[cut..].find('\n') {
            cut += newline + 1;
        }
        self.text.drain(..cut);
    }

    fn into_string(self) -> String {
        self.text
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

fn format_result(outcome: Outcome, stdout: String, stderr: String) -> ToolResult {
    let output = combine(stdout, stderr);

    let status = match outcome {
        Outcome::Exited(Ok(status)) => status,
        Outcome::Exited(Err(e)) => {
            return ToolResult::error(format!("failed to wait on command: {e}\n{output}"))
        }
        // The partial output is labelled as partial on purpose: unlabelled, a
        // half-finished build reads to the model as a finished one.
        Outcome::Cancelled => {
            return ToolResult::error(format!(
                "command cancelled by user; output produced before it was killed:\n{output}"
            ))
        }
        Outcome::TimedOut(secs) => {
            return ToolResult::error(format!(
                "command timed out after {secs}s; output produced before it was killed:\n{output}"
            ))
        }
    };

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

/// Joins the two captured streams into the single blob the model sees, keeping
/// them labelled so `error:` on stdout stays distinguishable from `error:` on
/// stderr.
fn combine(stdout: String, stderr: String) -> String {
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
    output
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

/// `truncate_tail`'s mirror image: keeps the *first* `max` characters.
///
/// Same char-boundary discipline, opposite end, because the two callers want
/// opposite halves. Shell output is diagnosed from its tail; a fetched web
/// page is read from its top — the article is at the beginning and the
/// navigation cruft at the bottom, so keeping a page's tail would hand the
/// model the footer and throw away the answer. Reusing `truncate_tail` for
/// `web_fetch` would have been the wrong cut, so it reuses the technique (and
/// the marker) instead, from the same module rather than a second hand-rolled
/// byte-index version.
pub fn truncate_head(text: &str, max: usize) -> String {
    if max == 0 {
        return "... (truncated)".to_string();
    }
    // Byte offset of the character *after* the last one we keep; `None` means
    // the text already fits.
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((end, _)) => format!("{}\n... (truncated)", &text[..end]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use smith_core::{AgentEvent, ProgressReporter};
    #[cfg(unix)]
    use std::path::PathBuf;

    // Only the `sh`-driven tests need a context, and those are all `cfg(unix)`.
    #[cfg(unix)]
    fn ctx() -> ToolContext {
        ToolContext::new(PathBuf::from("."), "test-session")
    }

    /// A context wired to a real progress channel, plus the receiving end.
    #[cfg(unix)]
    fn ctx_with_progress() -> (
        ToolContext,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolContext::new(PathBuf::from("."), "test-session")
            .with_progress(ProgressReporter::new("call_1", tx));
        (ctx, rx)
    }

    #[cfg(unix)]
    fn drain_progress(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<String> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .filter_map(|e| match e {
                AgentEvent::ToolProgress { line, .. } => Some(line),
                _ => None,
            })
            .collect()
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
    // The `truncate_*` tests further down are pure and stay cross-platform.

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
    async fn stdout_and_stderr_stay_distinguishable() {
        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "echo out; echo err 1>&2"}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("out"));
        assert!(
            result.content.contains("[stderr]\nerr"),
            "stderr lost its label: {}",
            result.content
        );
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

    // --- streaming ---------------------------------------------------------

    #[cfg(unix)]
    #[tokio::test]
    async fn progress_lines_arrive_while_the_command_is_still_running() {
        let (ctx, mut rx) = ctx_with_progress();
        let handle = tokio::spawn(async move {
            RunBashTool
                .execute(
                    serde_json::json!({"command": "echo first; sleep 3; echo last"}),
                    &ctx,
                    CancellationToken::new(),
                )
                .await
        });

        // Well before the command can finish. The old implementation read both
        // pipes to EOF first, so nothing at all reached the channel until the
        // process exited.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let lines = drain_progress(&mut rx);
        assert!(
            lines.iter().any(|l| l == "first"),
            "no progress before exit: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l == "last"),
            "the command cannot have finished yet: {lines:?}"
        );

        handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_progress_lines_are_labelled() {
        let (ctx, mut rx) = ctx_with_progress();
        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "echo oops 1>&2"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(drain_progress(&mut rx), vec!["[stderr] oops".to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_flood_of_output_does_not_become_a_flood_of_events() {
        let (ctx, mut rx) = ctx_with_progress();
        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "seq 1 20000"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);

        let lines = drain_progress(&mut rx);
        assert!(
            lines.len() < 200,
            "20k lines produced {} progress events",
            lines.len()
        );
        // Suppression is announced rather than silent.
        assert!(
            lines.iter().any(|l| l.contains("lines omitted")),
            "skipped lines were never accounted for: {lines:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_still_returns_what_had_already_been_produced() {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            cancel_clone.cancel();
        });

        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "echo built-so-far; echo warned 1>&2; sleep 30"}),
                &ctx(),
                cancel,
            )
            .await;

        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));
        // The bug this rewrite exists for: Esc on a long build used to return
        // nothing, not even the part that had already run.
        assert!(
            result.content.contains("built-so-far"),
            "partial stdout was discarded: {}",
            result.content
        );
        assert!(
            result.content.contains("warned"),
            "partial stderr was discarded: {}",
            result.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_still_returns_what_had_already_been_produced() {
        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "echo partial; sleep 30", "timeout_secs": 1}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("timed out"));
        assert!(
            result.content.contains("partial"),
            "partial output was discarded: {}",
            result.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_is_still_truncated_to_the_character_cap() {
        let result = RunBashTool
            .execute(
                serde_json::json!({"command": "seq 1 30000"}),
                &ctx(),
                CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.starts_with("... (truncated)"),
            "no truncation marker on oversized output"
        );
        assert!(result.content.chars().count() < MAX_OUTPUT_CHARS + 100);
        // The tail is what survived, so the last line is still there.
        assert!(result.content.contains("30000"));
    }

    // --- pure helpers ------------------------------------------------------

    #[test]
    fn tail_buffer_drops_the_head_and_keeps_the_tail() {
        let mut buf = TailBuffer::default();
        // Multibyte on purpose: the drain is byte-indexed underneath.
        for i in 0..40_000 {
            buf.push_line(&format!("café line {i}"));
        }
        let text = buf.into_string();
        assert!(text.len() <= CAPTURE_LIMIT_BYTES);
        assert!(text.contains("café line 39999"));
        assert!(!text.contains("café line 0\n"));
        // The cut lands on a line boundary, not mid-line.
        assert!(
            text.starts_with("café line "),
            "cut mid-line: {:?}",
            &text[..40]
        );
    }

    #[test]
    fn throttle_emits_the_first_line_immediately_and_counts_what_it_drops() {
        let mut throttle = Throttle::default();
        assert_eq!(throttle.offer(), Some(0), "first line must go out at once");
        assert_eq!(throttle.offer(), None);
        assert_eq!(throttle.offer(), None);

        std::thread::sleep(PROGRESS_INTERVAL);
        assert_eq!(throttle.offer(), Some(2), "dropped lines went unreported");
        assert_eq!(throttle.offer(), None);

        // A command that ends inside an interval still accounts for its tail.
        assert_eq!(throttle.finish(), Some(1));
        assert_eq!(throttle.finish(), None);
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

    #[test]
    fn truncate_head_keeps_the_head_not_the_tail() {
        assert_eq!(truncate_head("abcdef", 3), "abc\n... (truncated)");
        assert_eq!(truncate_head("áéíóú", 2), "áé\n... (truncated)");
    }

    #[test]
    fn truncate_head_leaves_short_input_untouched() {
        assert_eq!(truncate_head("hello", 10), "hello");
        assert_eq!(truncate_head("hello", 5), "hello");
        assert_eq!(truncate_head("caí três vezes", 64), "caí três vezes");
    }

    #[test]
    fn truncate_head_survives_multibyte_cut_points() {
        let text = "€".repeat(1_000);
        let out = truncate_head(&text, 500);
        assert!(out.ends_with("\n... (truncated)"));
        assert_eq!(out.matches('€').count(), 500);
    }
}
