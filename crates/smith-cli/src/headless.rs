//! The non-interactive frontend: a second consumer of the orchestrator's
//! `(event_rx, permission_rx, question_rx)` triple, alongside `smith_tui::run`.
//!
//! It deliberately duplicates none of `orchestrator.rs`. The orchestrator
//! already owns "an `Action` goes in, `AgentEvent`s come out"; everything here
//! is the other side of those channels — send one `SubmitMessage`, render the
//! stream, answer the two kinds of question a TUI would have popped a modal
//! for, and decide what the process's exit status should be.
//!
//! # Exit codes
//!
//! | code | meaning |
//! |------|---------|
//! | 0 | the turn completed normally |
//! | 1 | the turn failed — provider/stream error, or cancelled |
//! | 2 | usage or configuration error — bad flags, no API key, unusable `--cwd` |
//! | 3 | the turn was stopped by a safety cap (`--max-turns`, tool-call or wall-clock budget) |
//!
//! 2 matches what `clap` itself exits with on a bad flag, so "you invoked me
//! wrong" is one code whether or not argument parsing caught it. 3 is separate
//! from 1 because the two want opposite reactions in CI: a failed turn is a
//! bug to investigate, a capped turn is a budget to raise (and everything it
//! did before the cap is intact).
//!
//! # Streams
//!
//! In `text` mode stdout carries the assistant's prose and nothing else, while
//! tool activity, retries, denials and errors go to stderr. That split is what
//! makes `smith -p 'write a haiku' > haiku.txt` produce a haiku instead of a
//! haiku wrapped in progress chrome, without hiding the chrome from someone
//! watching the terminal. `json` and `stream-json` put their entire payload on
//! stdout — a machine reading them wants one stream, not two.
//!
//! # The `stream-json` contract
//!
//! One JSON value per line, each `{"type": ..., "data": ...}`. Most lines are
//! `AgentEvent`s verbatim, in the shape `smith-core` already serializes them
//! into — that enum *is* the wire format and this module does not re-describe
//! it. Two line types are the frontend's own and will not deserialize as an
//! `AgentEvent`:
//!
//! - `permission_decision` — what `--allowed-tools` decided about a tool. It
//!   has to be synthesized: on a denial the agent emits no tool events at all,
//!   so a consumer would otherwise see a `permission_prompt_needed` followed
//!   by silence and have to guess.
//! - `result` — always the final line, carrying the same object `json` mode
//!   prints. Without it a consumer would have to reimplement "was that a
//!   success?" from the event stream, and get it subtly wrong.

use std::collections::BTreeSet;
use std::io::Write;

use clap::ValueEnum;
use smith_core::{
    Action, AgentEvent, PermissionAsk, PermissionDecision, QuestionAsk, StopReason, Usage,
};
use tokio::sync::mpsc;

pub const EXIT_OK: u8 = 0;
pub const EXIT_TURN_FAILED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_LIMIT: u8 = 3;

/// What a headless run writes to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum OutputFormat {
    /// The assistant's prose, streamed as it arrives. Chrome goes to stderr.
    #[default]
    Text,
    /// One JSON object, printed once the turn is over.
    Json,
    /// One JSON value per line (JSONL), printed as the turn runs.
    StreamJson,
}

/// The instruction the model is given, assembled from `-p` and/or stdin.
///
/// Both present: `-p` is the instruction and stdin is data it refers to, so
/// the flag goes first and the piped bytes follow inside a delimiter. That
/// ordering is the whole point of `cat error.log | smith -p 'diagnose this'`
/// — "diagnose this" is meaningless before the thing it points at, and
/// splitting them into two messages would spend a round-trip establishing
/// what one message already says. The delimiter is not decoration: it is the
/// only thing separating an instruction from arbitrary bytes that may contain
/// text shaped like one.
///
/// Only stdin: the pipe *is* the prompt (`echo 'explain this repo' | smith`).
/// Only `-p`: the flag is the prompt. Neither: there is nothing to run.
pub fn compose_prompt(flag: Option<&str>, stdin: Option<&str>) -> Result<String, String> {
    let flag = flag.map(str::trim).filter(|s| !s.is_empty());
    let stdin = stdin.map(str::trim).filter(|s| !s.is_empty());
    match (flag, stdin) {
        (Some(flag), Some(stdin)) => Ok(format!(
            "{flag}\n\nThe following was piped to smith's standard input:\n\n<stdin>\n{stdin}\n</stdin>"
        )),
        (Some(flag), None) => Ok(flag.to_string()),
        (None, Some(stdin)) => Ok(stdin.to_string()),
        (None, None) => Err(
            "no prompt: pass -p/--print <PROMPT>, or pipe one in on stdin.".to_string(),
        ),
    }
}

pub struct HeadlessOptions {
    pub prompt: String,
    pub format: OutputFormat,
    /// Tool names `--allowed-tools` permitted. Only ever consulted for tools
    /// that reach the permission channel at all, i.e. `Mutating` and
    /// `Dangerous` ones — `ReadOnly` tools never prompt, in headless exactly
    /// as in the TUI, so they are always available.
    pub allowed_tools: BTreeSet<String>,
    pub color: bool,
    pub provider: String,
    pub model: String,
}

/// A tool call as it appeared to the frontend, for the `json` summary.
#[derive(Debug, Clone, serde::Serialize)]
struct ToolCallRecord {
    id: String,
    name: String,
    input: serde_json::Value,
    output: Option<String>,
    is_error: bool,
}

/// The final object of a `json` run, and the last line of a `stream-json` one.
#[derive(Debug, Clone, serde::Serialize)]
struct RunResult {
    ok: bool,
    exit_code: u8,
    provider: String,
    model: String,
    /// The assistant's last reply — what a caller doing `| jq -r .result`
    /// wants. Empty if the turn never produced one.
    result: String,
    stop_reason: Option<StopReason>,
    /// Why the run failed, when it did. `null` on success.
    error: Option<String>,
    /// Set when a safety cap stopped the turn.
    limit: Option<String>,
    /// How many provider replies the turn went through.
    num_turns: u32,
    usage: Usage,
    tool_calls: Vec<ToolCallRecord>,
    /// Tools the model asked for that `--allowed-tools` did not cover. Its
    /// own key rather than a line in the log, because "the agent tried to do
    /// the thing and was stopped" is the single most likely explanation for a
    /// disappointing headless run, and it should not need grepping for.
    denied_tools: Vec<String>,
}

/// Why the event loop stopped. The stop reason itself rides on the
/// accumulator, which is where the JSON summary reads it from.
enum Ending {
    Completed,
    Failed(String),
    Limit(String),
}

impl Ending {
    fn exit_code(&self) -> u8 {
        match self {
            Ending::Completed => EXIT_OK,
            Ending::Failed(_) => EXIT_TURN_FAILED,
            Ending::Limit(_) => EXIT_LIMIT,
        }
    }
}

#[derive(Default)]
struct Accumulator {
    result: String,
    stop_reason: Option<StopReason>,
    num_turns: u32,
    usage: Usage,
    tool_calls: Vec<ToolCallRecord>,
    denied_tools: Vec<String>,
}

const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

struct Paint(bool);

impl Paint {
    fn dim(&self, s: &str) -> String {
        if self.0 {
            format!("{DIM}{s}{RESET}")
        } else {
            s.to_string()
        }
    }

    fn red(&self, s: &str) -> String {
        if self.0 {
            format!("{RED}{s}{RESET}")
        } else {
            s.to_string()
        }
    }
}

/// Drives one headless turn to completion and returns the process exit code.
///
/// Generic over its two sinks so the tests can read back exactly what a run
/// would have printed; production passes stdout and stderr.
pub async fn run<O: Write, E: Write>(
    opts: &HeadlessOptions,
    action_tx: mpsc::UnboundedSender<Action>,
    mut event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut permission_rx: mpsc::UnboundedReceiver<PermissionAsk>,
    mut question_rx: mpsc::UnboundedReceiver<QuestionAsk>,
    out: &mut O,
    err: &mut E,
) -> u8 {
    let paint = Paint(opts.color);
    let mut acc = Accumulator::default();
    // Startup diagnostics (a broken MCP server, say) arrive on the same
    // channel as turn errors and are *not* fatal, so `Error` only ends the
    // run once the turn itself is underway. The agent emits `Thinking` as its
    // very first act, which is the marker.
    let mut turn_started = false;
    // Whether stdout still owes a newline. Deltas are written raw so a reader
    // watching `less` sees them land, and a model's last token is rarely a
    // newline — a shell prompt glued to the final word is the tell of a
    // program that forgot. Adding one unconditionally is the opposite bug.
    let mut newline_owed = false;

    if action_tx
        .send(Action::SubmitMessage(opts.prompt.clone()))
        .is_err()
    {
        return finish(
            opts,
            &paint,
            acc,
            Ending::Failed("the agent orchestrator is not running".into()),
            out,
            err,
            newline_owed,
        );
    }

    let ending = loop {
        // `biased` so every event already queued is rendered before a
        // permission or question ask is answered: without it `select!` picks
        // at random and the tool-call line for a call can print after the
        // decision that allowed it. There is no starvation risk — the agent
        // is blocked on the oneshot while an ask is outstanding, so no
        // further events can be produced until it is answered.
        tokio::select! {
            biased;

            event = event_rx.recv() => {
                let Some(event) = event else {
                    break Ending::Failed("the agent stopped without finishing the turn".into());
                };

                if let OutputFormat::StreamJson = opts.format {
                    write_line(out, &event);
                }

                match &event {
                    AgentEvent::PhaseChanged(phase) => {
                        if !matches!(phase, smith_core::AgentPhase::Idle) {
                            turn_started = true;
                        }
                    }
                    AgentEvent::AssistantTextDelta(delta) => {
                        if opts.format == OutputFormat::Text {
                            let _ = write!(out, "{delta}");
                            let _ = out.flush();
                            if let Some(last) = delta.chars().last() {
                                newline_owed = last != '\n';
                            }
                        }
                    }
                    AgentEvent::TokenUsage(usage) => {
                        // `add` rather than two field additions, so the cache
                        // counts reach the JSON summary too — a run that is
                        // 90% cache reads should not report as 10% of its
                        // real prompt size.
                        acc.usage.add(usage);
                    }
                    AgentEvent::ToolCallStarted { id, tool_name, input } => {
                        if opts.format == OutputFormat::Text {
                            let _ = writeln!(
                                err,
                                "{}",
                                paint.dim(&format!("· {tool_name} {}", summarize(input)))
                            );
                        }
                        acc.tool_calls.push(ToolCallRecord {
                            id: id.clone(),
                            name: tool_name.clone(),
                            input: input.clone(),
                            output: None,
                            is_error: false,
                        });
                    }
                    AgentEvent::ToolCallResult { id, output, is_error } => {
                        if let Some(record) = acc.tool_calls.iter_mut().rev().find(|r| &r.id == id) {
                            record.output = Some(output.clone());
                            record.is_error = *is_error;
                        }
                        if *is_error && opts.format == OutputFormat::Text {
                            let _ = writeln!(
                                err,
                                "{}",
                                paint.red(&format!("  ! {}", first_line(output)))
                            );
                        }
                    }
                    AgentEvent::ProviderRetry { attempt, max_attempts, delay_ms, reason } => {
                        if opts.format == OutputFormat::Text {
                            let _ = writeln!(
                                err,
                                "{}",
                                paint.dim(&format!(
                                    "· retrying after {delay_ms}ms (attempt {attempt}/{max_attempts}): {reason}"
                                ))
                            );
                        }
                    }
                    AgentEvent::AssistantTurnComplete { message, stop_reason } => {
                        acc.num_turns += 1;
                        let text = message.text();
                        if !text.is_empty() {
                            acc.result = text;
                        }
                        if *stop_reason != StopReason::ToolUse {
                            acc.stop_reason = Some(*stop_reason);
                            break match stop_reason {
                                // The agent reports a cancelled turn this way
                                // rather than as an error, but for an exit
                                // code it is a failure like any other.
                                StopReason::Cancelled => Ending::Failed("cancelled".into()),
                                _ => Ending::Completed,
                            };
                        }
                    }
                    AgentEvent::TurnLimitReached { detail, .. } => {
                        break Ending::Limit(detail.clone());
                    }
                    AgentEvent::Error(message) => {
                        if !turn_started {
                            // Startup noise: report it and keep going.
                            let _ = writeln!(err, "{}", paint.red(&format!("smith: {message}")));
                            continue;
                        }
                        break Ending::Failed(message.clone());
                    }
                    _ => {}
                }
            }

            Some(ask) = permission_rx.recv() => {
                let name = ask.request.tool_name.clone();
                let allowed = opts.allowed_tools.contains(&name);
                if !allowed {
                    acc.denied_tools.push(name.clone());
                }
                report_decision(opts, &paint, &name, allowed, out, err);
                // `AllowSession` rather than `AllowOnce`: the answer comes
                // from a flag that cannot change mid-run, so re-asking would
                // only burn round-trips on a question with a fixed answer.
                let decision = if allowed {
                    PermissionDecision::AllowSession
                } else {
                    PermissionDecision::Deny
                };
                let _ = ask.respond_to.send(decision);
            }

            Some(ask) = question_rx.recv() => {
                let _ = ask.respond_to.send(Err(NO_INTERACTIVE_USER.to_string()));
                if opts.format == OutputFormat::Text {
                    let _ = writeln!(
                        err,
                        "{}",
                        paint.dim(&format!("· ask_user (unanswerable, headless): {}", ask.question.prompt))
                    );
                }
            }
        }
    };

    finish(opts, &paint, acc, ending, out, err, newline_owed)
}

/// What `ask_user` gets told when there is nobody to ask.
///
/// Three options were on the table. Blocking until an answer arrives hangs a
/// CI job on a question no one will ever see. Auto-picking a suggestion puts
/// words in the user's mouth — and `ask_user` is asked precisely when the
/// model has no basis to choose, so the pick would be arbitrary and then
/// treated as settled. Answering honestly is the only one that leaves the
/// model correctly informed: it learns the channel is closed, and that
/// proceeding on its own judgement is the expected behaviour rather than an
/// overstep.
///
/// It goes back through the normal answer channel, so the model sees it as
/// Sent as `Err`, so the model sees a *failed* tool call rather than a fake
/// answer attributed to a user who isn't there. Auto-picking a suggestion
/// would put words in the user's mouth at exactly the moment the model
/// admitted it had no basis to choose.
const NO_INTERACTIVE_USER: &str = "[smith] This run is headless — there is no interactive user, and \
    no further questions can be answered. Do not ask again. Choose the option you judge best, state \
    which one you chose and why, and carry on.";

#[allow(clippy::too_many_arguments)]
fn finish<O: Write, E: Write>(
    opts: &HeadlessOptions,
    paint: &Paint,
    acc: Accumulator,
    ending: Ending,
    out: &mut O,
    err: &mut E,
    newline_owed: bool,
) -> u8 {
    let exit_code = ending.exit_code();
    let (error, limit) = match &ending {
        Ending::Completed => (None, None),
        Ending::Failed(message) => (Some(message.clone()), None),
        Ending::Limit(detail) => (None, Some(detail.clone())),
    };

    let result = RunResult {
        ok: exit_code == EXIT_OK,
        exit_code,
        provider: opts.provider.clone(),
        model: opts.model.clone(),
        result: acc.result,
        stop_reason: acc.stop_reason,
        error: error.clone(),
        limit: limit.clone(),
        num_turns: acc.num_turns,
        usage: acc.usage,
        tool_calls: acc.tool_calls,
        denied_tools: acc.denied_tools,
    };

    match opts.format {
        OutputFormat::Text => {
            if newline_owed {
                let _ = writeln!(out);
            }
            if let Some(detail) = &limit {
                let _ = writeln!(err, "{}", paint.dim(&format!("smith: stopped: {detail}")));
            }
            if let Some(message) = &error {
                let _ = writeln!(err, "{}", paint.red(&format!("smith: error: {message}")));
            }
        }
        OutputFormat::Json => {
            // Pretty-printed: it is one object at the end of the run, so no
            // consumer is splitting it on newlines, and a human reading it
            // straight out of a CI log is the likelier audience.
            if let Ok(text) = serde_json::to_string_pretty(&result) {
                let _ = writeln!(out, "{text}");
            }
        }
        OutputFormat::StreamJson => {
            write_line(
                out,
                &serde_json::json!({ "type": "result", "data": result }),
            );
        }
    }
    let _ = out.flush();
    let _ = err.flush();
    exit_code
}

/// Emits one JSON value on one line.
///
/// Compact, never pretty: `stream-json` is a JSONL stream, and a
/// pretty-printed object spanning lines breaks every consumer that reads it
/// line by line — which is all of them.
fn write_line<W: Write, T: serde::Serialize>(out: &mut W, value: &T) {
    if let Ok(text) = serde_json::to_string(value) {
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }
}

fn report_decision<O: Write, E: Write>(
    opts: &HeadlessOptions,
    paint: &Paint,
    tool: &str,
    allowed: bool,
    out: &mut O,
    err: &mut E,
) {
    // On stderr in every format, including the machine-readable ones: stdout
    // is the only stream a parser reads, so this costs nothing there, and a
    // refusal is the one thing a human debugging a disappointing run needs to
    // see immediately.
    if !allowed {
        let _ = writeln!(
            err,
            "{}",
            paint.red(&format!(
                "smith: denied {tool} — add it to --allowed-tools to let this run use it"
            ))
        );
    }

    // A decision is invisible in the raw event stream: on a denial the agent
    // skips `ToolCallStarted`/`ToolCallResult` entirely, so without this line
    // a consumer sees a request and then silence.
    if opts.format == OutputFormat::StreamJson {
        write_line(
            out,
            &serde_json::json!({
                "type": "permission_decision",
                "data": { "tool_name": tool, "allowed": allowed, "source": "--allowed-tools" }
            }),
        );
    }
}

const SUMMARY_LIMIT: usize = 120;

/// A tool's input on one short line — enough to tell two calls apart in the
/// text log without dumping a file body into it.
fn summarize(input: &serde_json::Value) -> String {
    let text = match input {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                other => format!("{k}={other}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    };
    let text = text.replace('\n', " ");
    truncate(&text, SUMMARY_LIMIT)
}

fn first_line(text: &str) -> String {
    truncate(text.lines().next().unwrap_or("").trim(), SUMMARY_LIMIT)
}

/// Truncation by chars, not bytes: tool inputs are arbitrary user text and
/// slicing one mid-codepoint panics.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests;
