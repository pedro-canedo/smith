//! Context accounting and the pure half of compaction.
//!
//! Everything here is a free function over `&[Message]` with no provider, no
//! I/O and no `self`. That is deliberate: the acceptance criterion for
//! compaction is "a 200-message session auto-compacts and preserves pending
//! todos", and the only way to *prove* that rather than demo it is for the
//! carry-over to be a pure function you can call with 200 messages and assert
//! on the result. The half that spends a provider request (summarising the
//! prose) lives in `agent.rs` and is allowed to fail; this half never is.

use std::collections::BTreeSet;

use crate::event::{Task, TaskStatus};
use crate::message::{ContentBlock, Message, Role};

/// Average characters per token for English prose under a BPE tokenizer.
///
/// We deliberately do **not** link a tokenizer crate. A real vocab is ~10MB of
/// data per model family, needs to be kept in step with every model we add,
/// and buys precision we throw away one turn later: the provider reports the
/// exact prompt size with every response, so an estimate is only ever used for
/// the *unsent delta* since that response.
const CHARS_PER_TOKEN: usize = 4;

/// Safety margin applied on top of `chars / 4`, as a fraction (numerator over
/// denominator) — 4/3, i.e. the estimate is inflated by a third, which nets
/// out to roughly one token per three characters.
///
/// The two directions of error are not symmetric. Over-estimating compacts a
/// little earlier than strictly necessary: wasted tokens, fully recoverable.
/// Under-estimating lets history grow past the window and the *next* request
/// hard-fails with the context already spent. So the margin only ever points
/// one way.
///
/// A third is the right size because of *what* the unsent delta is made of.
/// It is almost never prose — it is tool results: JSON, file contents, diffs,
/// shell output, paths. BPE splits punctuation, indentation and identifiers
/// far more finely than English text, and measured ratios for that kind of
/// content sit around 3.0–3.5 characters per token rather than 4. Inflating by
/// a third lands the estimate at the pessimistic end of that range without
/// making it absurd for the prose case.
const MARGIN_NUM: usize = 4;
const MARGIN_DEN: usize = 3;

/// Per-message structural overhead: role markers, block delimiters and the
/// JSON scaffolding every provider wraps a message in, none of which appears
/// in the text we are counting.
const MESSAGE_OVERHEAD_TOKENS: u32 = 4;

/// Fraction of the context window at which compaction fires.
///
/// Not higher, because the trigger is checked *before* a request whose reply
/// (up to `max_output` tokens) also has to fit, and because the estimate of
/// the unsent delta is only approximate. Not lower, because every compaction
/// throws away detail and costs a request.
pub const COMPACT_THRESHOLD: f32 = 0.80;

/// Estimated tokens for a string. Counts `char`s, not bytes: UTF-8 would
/// triple-count CJK, which is already the case this estimate is worst at.
pub fn estimate_tokens(text: &str) -> u32 {
    let chars = text.chars().count();
    let tokens = chars.div_ceil(CHARS_PER_TOKEN) * MARGIN_NUM / MARGIN_DEN;
    tokens.min(u32::MAX as usize) as u32
}

/// Estimated tokens one message contributes to a request, including the
/// serialized form of any tool call's arguments — a `write_file` call carries
/// the whole file body in its input, and ignoring that would under-count the
/// single largest thing in most histories.
pub fn estimate_message_tokens(message: &Message) -> u32 {
    let mut total = MESSAGE_OVERHEAD_TOKENS;
    for block in &message.content {
        total = total.saturating_add(match block {
            ContentBlock::Text { text } => estimate_tokens(text),
            ContentBlock::ToolUse { name, input, .. } => {
                estimate_tokens(name).saturating_add(estimate_tokens(&input.to_string()))
            }
            ContentBlock::ToolResult { content, .. } => estimate_tokens(content),
        });
    }
    total
}

pub fn estimate_messages_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .map(estimate_message_tokens)
        .fold(0u32, u32::saturating_add)
}

/// How full the context is, as reported out to a frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    /// Tokens the next request would carry.
    pub used: u32,
    /// The model's total context window, from `LlmProvider::capabilities`.
    pub window: u32,
    /// True when any part of `used` is estimated rather than provider-reported.
    pub estimated: bool,
}

impl ContextUsage {
    pub fn ratio(&self) -> f32 {
        if self.window == 0 {
            return 0.0;
        }
        self.used as f32 / self.window as f32
    }
}

/// Facts that must survive compaction **structurally**, carried over by
/// re-injection rather than by hoping the summary happens to mention them.
///
/// This is the difference between a compaction that demos well and one that
/// works: a summarising model asked to compress twenty rounds of work will
/// reliably drop the three todos it has not started yet, because they are the
/// least interesting thing in the transcript. Re-injecting them verbatim makes
/// that impossible.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CarryOver {
    /// The session goal, repeated so it survives even though it also rides the
    /// system prompt (a `/model` switch or a resume can rebuild that).
    pub goal: Option<String>,
    /// Every task not yet `Completed`, verbatim.
    pub pending_tasks: Vec<Task>,
    /// Paths the dropped history touched, in first-seen order.
    pub files_touched: Vec<String>,
}

/// Cap on how many paths get re-injected. Past this the list stops being
/// context and starts being another thing eating the window.
const MAX_FILES_CARRIED: usize = 40;

/// Argument names tools use for the path they act on.
const PATH_KEYS: &[&str] = &["path", "file_path", "dir", "directory"];

/// Builds the structural carry-over for a compaction that is about to drop
/// `dropped`.
///
/// `tasks` is the agent's live checklist and wins over anything reconstructed
/// from history: it already reflects every `write_tasks` call, including ones
/// in the part of history that is being *kept*. Only when it is empty do we
/// fall back to the last `write_tasks` call inside `dropped`, which is the
/// case where the list would otherwise vanish with the messages.
pub fn carry_over(dropped: &[Message], goal: Option<&str>, tasks: &[Task]) -> CarryOver {
    let mut pending: Vec<Task> = tasks
        .iter()
        .filter(|t| t.status != TaskStatus::Completed)
        .cloned()
        .collect();

    if tasks.is_empty() {
        if let Some(recovered) = last_write_tasks(dropped) {
            pending = recovered
                .into_iter()
                .filter(|t| t.status != TaskStatus::Completed)
                .collect();
        }
    }

    CarryOver {
        goal: goal.map(str::to_string),
        pending_tasks: pending,
        files_touched: files_touched(dropped),
    }
}

fn last_write_tasks(messages: &[Message]) -> Option<Vec<Task>> {
    messages
        .iter()
        .rev()
        .flat_map(|m| m.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } if name == "write_tasks" => {
                crate::agent::parse_tasks(input).ok()
            }
            _ => None,
        })
}

/// Paths named by any tool call in `messages`, deduplicated, in the order they
/// were first touched. Keyed off the argument name rather than a list of tool
/// names so an MCP-bridged or future tool that takes a `path` is covered too.
fn files_touched(messages: &[Message]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    for block in messages.iter().flat_map(|m| m.content.iter()) {
        let ContentBlock::ToolUse { input, .. } = block else {
            continue;
        };
        for key in PATH_KEYS {
            let Some(path) = input.get(key).and_then(|v| v.as_str()) else {
                continue;
            };
            let path = path.trim();
            if path.is_empty() || !seen.insert(path.to_string()) {
                continue;
            }
            ordered.push(path.to_string());
        }
    }
    ordered
}

impl CarryOver {
    pub fn is_empty(&self) -> bool {
        self.goal.is_none() && self.pending_tasks.is_empty() && self.files_touched.is_empty()
    }

    /// Renders the replacement message that stands in for the dropped history.
    ///
    /// `summary` is the model-written prose, or `None` if there is none. The
    /// structural sections are appended *after* it and are never summarised —
    /// that is the whole point.
    pub fn render(&self, summary: Option<&str>) -> String {
        let mut out = String::from(
            "[smith] The earlier part of this conversation was compacted to stay \
             inside the model's context window. Treat everything below as \
             established fact about work already done.\n",
        );

        if let Some(summary) = summary.map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str("\n## What happened before this point\n\n");
            out.push_str(summary);
            out.push('\n');
        }

        if let Some(goal) = &self.goal {
            out.push_str("\n## Session goal (unchanged)\n\n");
            out.push_str(goal);
            out.push('\n');
        }

        if !self.pending_tasks.is_empty() {
            out.push_str(
                "\n## Outstanding todos (carried over verbatim — these are still open)\n\n",
            );
            for task in &self.pending_tasks {
                let marker = match task.status {
                    TaskStatus::InProgress => "[~]",
                    _ => "[ ]",
                };
                out.push_str(marker);
                out.push(' ');
                out.push_str(&task.content);
                out.push('\n');
            }
        }

        if !self.files_touched.is_empty() {
            out.push_str("\n## Files already touched\n\n");
            for path in self.files_touched.iter().take(MAX_FILES_CARRIED) {
                out.push_str("- ");
                out.push_str(path);
                out.push('\n');
            }
            if self.files_touched.len() > MAX_FILES_CARRIED {
                out.push_str(&format!(
                    "- …and {} more\n",
                    self.files_touched.len() - MAX_FILES_CARRIED
                ));
            }
        }

        out.push_str("\nThe most recent messages follow this one unmodified.");
        out
    }
}

/// Where the kept tail of history should start, or `None` if there is no safe
/// place to cut.
///
/// The constraint that makes this non-trivial: a `tool_use` block and its
/// matching `tool_result` must stay together or the *next* request is rejected
/// outright by the provider. So the cut can only land on a **clean boundary** —
/// a `User` message that carries no `ToolResult` blocks, i.e. something the
/// human actually typed (or the loop driver synthesised), never the
/// pseudo-user message the agent uses to return tool results.
///
/// Given the ideal cut point (`len - keep_recent`) it prefers the latest clean
/// boundary *at or before* it, because keeping more than asked is the harmless
/// direction. Only if there is none — a long uninterrupted tool-heavy stretch,
/// which is exactly when compaction matters most — does it look forward and
/// keep less.
pub fn compaction_split(messages: &[Message], keep_recent: usize) -> Option<usize> {
    if messages.len() <= keep_recent {
        return None;
    }
    let candidate = messages.len() - keep_recent;
    let is_boundary = |i: usize| {
        messages[i].role == Role::User
            && !messages[i]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    };

    // Never 0: cutting there would drop nothing and burn a summarisation
    // request for no reduction at all.
    (1..=candidate)
        .rev()
        .find(|&i| is_boundary(i))
        .or_else(|| (candidate + 1..messages.len()).find(|&i| is_boundary(i)))
}

/// Per-tool-result cap when rendering history for the summariser. Tool output
/// dominates a coding session's history by an order of magnitude and is the
/// least worth summarising verbatim — the model needs to know a build was run
/// and failed, not the 4000 lines it printed.
const RESULT_EXCERPT_CHARS: usize = 400;
/// Same idea for prose, which is worth more but still not worth all of it.
const TEXT_EXCERPT_CHARS: usize = 2000;

/// Renders history as a flat transcript for the summarising request.
///
/// Sent as the *content of a single user message* rather than as real
/// conversation history, for two reasons. It sidesteps the `tool_use` /
/// `tool_result` pairing rules entirely — a transcript is just text, so no
/// arrangement of it can produce a 400 from the provider. And it lets each
/// tool result be excerpted, which cuts the cost of the summarisation request
/// by roughly the ratio that tool output dominates history.
///
/// `budget_chars` caps the whole thing; when it overflows, the *end* is kept,
/// since the messages nearest the present are the ones the continuation
/// depends on.
pub fn render_transcript(messages: &[Message], budget_chars: usize) -> String {
    let mut out = String::new();
    for message in messages {
        let who = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for block in &message.content {
            match block {
                ContentBlock::Text { text } if !text.trim().is_empty() => {
                    out.push_str(&format!("{who}: {}\n", excerpt(text, TEXT_EXCERPT_CHARS)));
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    out.push_str(&format!(
                        "{who} called {name}({})\n",
                        excerpt(&input.to_string(), RESULT_EXCERPT_CHARS)
                    ));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let status = if *is_error { "error" } else { "ok" };
                    out.push_str(&format!(
                        "  -> {status}: {}\n",
                        excerpt(content, RESULT_EXCERPT_CHARS)
                    ));
                }
                ContentBlock::Text { .. } => {}
            }
        }
    }

    if out.chars().count() > budget_chars {
        let skip = out.chars().count() - budget_chars;
        let tail: String = out.chars().skip(skip).collect();
        return format!("[earlier transcript elided]\n{tail}");
    }
    out
}

fn excerpt(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.replace('\n', " ");
    }
    let head: String = trimmed.chars().take(max_chars).collect();
    format!("{}… [{} chars truncated]", head.replace('\n', " "), {
        trimmed.chars().count() - max_chars
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(content: &str, status: TaskStatus) -> Task {
        Task::new(content, status)
    }

    fn tool_call(id: &str, name: &str, input: serde_json::Value) -> Message {
        Message::assistant(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }])
    }

    fn tool_result(id: &str, content: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: content.to_string(),
                is_error: false,
            }],
        }
    }

    #[test]
    fn the_estimate_never_undershoots_chars_over_four() {
        // 40 chars: chars/4 is 10, and the margin has to push it above that.
        let text = "a".repeat(40);
        assert!(estimate_tokens(&text) >= 10, "{}", estimate_tokens(&text));
        assert_eq!(estimate_tokens(&text), 13);
        assert_eq!(estimate_tokens(""), 0);
    }

    /// A `write_file` call carries the file body in its arguments; counting
    /// only the visible text would miss the largest thing in the message.
    #[test]
    fn tool_call_arguments_count_toward_the_estimate() {
        let body = "x".repeat(4000);
        let with_body = tool_call("1", "write_file", serde_json::json!({"content": body}));
        let without = tool_call("1", "write_file", serde_json::json!({}));
        assert!(estimate_message_tokens(&with_body) > estimate_message_tokens(&without) + 900);
    }

    #[test]
    fn ratio_is_safe_against_a_zero_window() {
        let usage = ContextUsage {
            used: 100,
            window: 0,
            estimated: true,
        };
        assert_eq!(usage.ratio(), 0.0);
    }

    /// The invariant that makes compaction safe at all: never cut between a
    /// tool call and its result.
    #[test]
    fn the_split_never_lands_on_a_tool_result_message() {
        let messages = vec![
            Message::user_text("first request"),
            tool_call("a", "read_file", serde_json::json!({"path": "a.rs"})),
            tool_result("a", "contents"),
            Message::assistant(vec![ContentBlock::Text {
                text: "read it".into(),
            }]),
            Message::user_text("second request"),
            tool_call("b", "read_file", serde_json::json!({"path": "b.rs"})),
            tool_result("b", "contents"),
        ];

        // The ideal cut (len 7 - keep 3 = 4) is already a clean boundary.
        assert_eq!(compaction_split(&messages, 3), Some(4));

        // An ideal cut that lands on a tool-result message walks *back* to the
        // previous real user message rather than splitting the pair.
        assert_eq!(compaction_split(&messages, 1), Some(4));
    }

    /// A tool-heavy stretch with no clean boundary before the ideal cut has to
    /// look forward instead of refusing to compact — that stretch is precisely
    /// what fills a context window.
    #[test]
    fn the_split_looks_forward_when_nothing_clean_precedes_the_ideal_cut() {
        let mut messages = vec![Message::user_text("go")];
        for i in 0..10 {
            messages.push(tool_call(
                &i.to_string(),
                "read_file",
                serde_json::json!({}),
            ));
            messages.push(tool_result(&i.to_string(), "contents"));
        }
        messages.push(Message::user_text("now do the next thing"));
        messages.push(Message::assistant(vec![ContentBlock::Text {
            text: "ok".into(),
        }]));

        // Ideal cut is at index 21, deep inside the tool stretch; the only
        // clean boundary at or before it is 0, which is not allowed.
        let split = compaction_split(&messages, 2).unwrap();
        assert_eq!(split, 21);
        assert_eq!(messages[split].role, Role::User);
        assert!(!messages[split]
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. })));
    }

    #[test]
    fn there_is_no_split_when_there_is_nothing_to_drop() {
        let messages = vec![Message::user_text("hi")];
        assert_eq!(compaction_split(&messages, 8), None);
        // A history that is all one tool stretch has no clean boundary at all.
        let stuck = vec![
            tool_call("a", "x", serde_json::json!({})),
            tool_result("a", "y"),
        ];
        assert_eq!(compaction_split(&stuck, 1), None);
    }

    /// The acceptance criterion in miniature: the pure function is what makes
    /// "the todos are all still there" assertable rather than hopeful.
    #[test]
    fn carry_over_keeps_every_open_todo_and_drops_completed_ones() {
        let tasks = vec![
            task("done already", TaskStatus::Completed),
            task("halfway through", TaskStatus::InProgress),
            task("not started", TaskStatus::Pending),
        ];
        let carried = carry_over(&[], Some("ship the thing"), &tasks);

        assert_eq!(carried.pending_tasks.len(), 2);
        let rendered = carried.render(Some("we did some work"));
        assert!(rendered.contains("halfway through"), "{rendered}");
        assert!(rendered.contains("not started"), "{rendered}");
        assert!(rendered.contains("ship the thing"), "{rendered}");
        assert!(!rendered.contains("done already"), "{rendered}");
        assert!(rendered.contains("we did some work"), "{rendered}");
    }

    /// When the live checklist is empty (a resumed session whose agent state
    /// was never seeded, say) the todos still have to come back — from the
    /// history about to be thrown away.
    #[test]
    fn carry_over_recovers_todos_from_history_when_the_live_list_is_empty() {
        let dropped = vec![tool_call(
            "1",
            "write_tasks",
            serde_json::json!({"tasks": [
                {"content": "old and done", "status": "completed"},
                {"content": "still open", "status": "pending"},
            ]}),
        )];
        let carried = carry_over(&dropped, None, &[]);
        assert_eq!(carried.pending_tasks.len(), 1);
        assert_eq!(carried.pending_tasks[0].content, "still open");
    }

    #[test]
    fn files_touched_are_deduplicated_in_first_seen_order() {
        let dropped = vec![
            tool_call("1", "read_file", serde_json::json!({"path": "b.rs"})),
            tool_call("2", "write_file", serde_json::json!({"path": "a.rs"})),
            tool_call("3", "edit_file", serde_json::json!({"path": "b.rs"})),
            tool_call("4", "list_dir", serde_json::json!({"dir": "src"})),
            tool_call("5", "run_bash", serde_json::json!({"command": "ls"})),
        ];
        let carried = carry_over(&dropped, None, &[]);
        assert_eq!(carried.files_touched, vec!["b.rs", "a.rs", "src"]);
    }

    #[test]
    fn an_empty_carry_over_reports_itself_as_empty() {
        assert!(carry_over(&[], None, &[]).is_empty());
        assert!(!carry_over(&[], Some("g"), &[]).is_empty());
    }

    #[test]
    fn the_transcript_excerpts_tool_output_and_keeps_the_recent_end() {
        let messages = vec![
            Message::user_text("build it"),
            tool_call(
                "a",
                "run_bash",
                serde_json::json!({"command": "cargo build"}),
            ),
            tool_result("a", &"compiler noise\n".repeat(500)),
            Message::assistant(vec![ContentBlock::Text {
                text: "it built".into(),
            }]),
        ];

        let full = render_transcript(&messages, 100_000);
        assert!(full.contains("chars truncated"), "{full}");
        assert!(full.contains("it built"));

        let clipped = render_transcript(&messages, 50);
        assert!(
            clipped.starts_with("[earlier transcript elided]"),
            "{clipped}"
        );
        // The end is what survives, because that is what the continuation
        // depends on.
        assert!(clipped.contains("it built"), "{clipped}");
    }
}
