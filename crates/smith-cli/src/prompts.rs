//! System prompt and the prompt-building/reply-parsing helpers for the
//! `/plan` and `/loop` flows. Pure string/history manipulation, no I/O — kept
//! apart from `orchestrator.rs` (which drives the actual turns) so this half
//! can be tuned and tested without touching any async/channel plumbing.

use chrono::{DateTime, FixedOffset, Local};
use smith_core::Message;

/// Environment context appended to the system prompt on every request, via
/// `Agent::with_context_provider`.
///
/// Without this the model has no anchor for "now" and silently falls back on
/// the last year it saw in training — which is how it ends up issuing a search
/// for a year that's already history and then reporting that nothing current
/// turned up. Split from `environment_now` so the formatting is testable
/// against a fixed instant instead of the wall clock.
pub fn environment_block(now: DateTime<FixedOffset>) -> String {
    format!(
        "## Environment\n\
         Current date: {weekday}, {date} (local time, UTC{offset}).\n\
         \n\
         This comes from the system clock, so it is the real current date, and it is LATER than \
         your training data. Whenever you reason about \"current\", \"latest\", \"today\", \"this \
         year\", or how recent something is, use THIS date — not the most recent year you happen \
         to remember. Never put a remembered year into a web_search query: use the year above, or \
         leave the year out entirely and let the results speak.",
        weekday = now.format("%A"),
        date = now.format("%Y-%m-%d"),
        offset = now.format("%:z"),
    )
}

/// `environment_block` for right now — the function handed to the `Agent`.
pub fn environment_now() -> String {
    environment_block(Local::now().fixed_offset())
}

pub const SYSTEM_PROMPT: &str = "\
You are smith, a terminal-based coding agent. Be concise.

Workflow:
- Break work into small steps. Briefly state the next step before calling tools.
- For existing files: read_file, then edit_file. Use write_file only to create or fully overwrite.
- After each tool result, briefly verify success or failure before the next mutation.
- Do not produce large plans unless the user ran /plan.

Decisions & questions:
- Prefer deciding yourself for low-risk, reversible choices (names of helpers, minor wording, obvious defaults).
- When a choice is ambiguous or high-impact (architecture, deleting data, public API, irreversible ops), call ask_user with exactly three concrete options (option_a/b/c). The UI also offers free-text.
- Never call ask_user just because the user hasn't given you a task yet — a greeting like \"hi\" or a vague \"what can you do\" gets a normal conversational reply, not a question. Only call ask_user once there's an actual task in progress and a genuine fork in how to proceed.
- Never ask the user to \"approve the plan\" in chat — plan approval is a separate UI. Once told the plan is approved, start implementing with tools immediately.

Task tracking:
- For any multi-step task (3+ steps), call write_tasks once at the start with the full step list (status: pending), then again whenever a step starts (in_progress) or finishes (completed) — always resend the full list, not a diff. Skip it for single-step or trivial requests.

Research:
- When you're not confident about something you're about to rely on (current events, a library's API/version/behavior, a fact you might be wrong about), call web_search to check before proceeding — don't guess, and don't tell the user to go search it themselves.
- Prefer web_search over improvising shell pipelines against undocumented endpoints (e.g. scraping an API by hand with curl/jq) when you just need information, not a specific file on disk.
- Once you've called web_search, answer from what the results actually say, not from training knowledge. This matters most for anything time-sensitive (news, current events, prices, who currently holds some position): your training data is stale and will be wrong there even when it sounds confident. Each result may carry a `published` date — use it to judge how current a source is, and prefer the most recent when sources disagree.
- Build queries against the current date given in the Environment section, never against a year you remember. If a query came back empty or off-target, refine it once — correct or drop the year, reword it, go at the primary source — before concluding the information isn't out there.
- If the results cover only part of the question, report what you DID find and then name the gap explicitly. Never withhold the entire answer because one part wasn't covered, and never pad the gap from memory.
";

const BUILD_PLAN_PROMPT: &str = "\
The plan below was approved in the UI. Do NOT ask for approval. \
Do NOT call ask_user about proceeding. Call tools NOW to implement step 1. \
Prefer small steps: read before edit, verify each result. \
Only use ask_user if a concrete implementation choice is ambiguous (then give three options).

## Approved plan
";

const BUILD_PLAN_FOOTER: &str = "\n\nStart with the first concrete tool call.";

/// Line the model is told to emit, on its own, once a `/loop` task is truly
/// finished — the loop driver checks for it verbatim in the assistant's text.
pub const LOOP_DONE_SENTINEL: &str = "LOOP_DONE";

pub const LOOP_CONTINUE_PROMPT: &str = "\
Continue working on the loop task from where you left off. If it is now fully \
complete, end your reply with the exact line `LOOP_DONE` on its own line.";

/// Builds the user message for a post-approval implementation turn.
pub fn build_approved_plan_prompt(plan_text: &str) -> String {
    let plan = plan_text.trim();
    if plan.is_empty() {
        format!(
            "{BUILD_PLAN_PROMPT}(plan text unavailable — follow the plan from earlier in this conversation.){BUILD_PLAN_FOOTER}"
        )
    } else {
        format!("{BUILD_PLAN_PROMPT}{plan}{BUILD_PLAN_FOOTER}")
    }
}

/// Last non-empty assistant text in history (the approved plan body).
pub fn last_assistant_plan_text(history: &[Message]) -> String {
    history
        .iter()
        .rev()
        .find(|m| m.role == smith_core::Role::Assistant && !m.text().trim().is_empty())
        .map(|m| m.text())
        .unwrap_or_default()
}

/// Builds the first-iteration prompt for a `/loop` task, instructing the
/// model to keep working autonomously and self-report completion.
pub fn build_loop_task_prompt(task: &str) -> String {
    format!(
        "{task}\n\n\
Work on this using the available tools, across as many turns as it takes. \
Once the task is fully complete, end your reply with the exact line `{LOOP_DONE_SENTINEL}` \
on its own line — only once it is truly done, not before. Don't ask the user whether to \
continue; keep working autonomously."
    )
}

/// Whether the most recent assistant reply declared the loop task done.
pub fn loop_turn_is_done(history: &[Message]) -> bool {
    history
        .iter()
        .rev()
        .find(|m| m.role == smith_core::Role::Assistant)
        .map(|m| m.text().contains(LOOP_DONE_SENTINEL))
        .unwrap_or(false)
}

#[cfg(test)]
mod build_prompt_tests {
    use super::{build_approved_plan_prompt, last_assistant_plan_text};
    use smith_core::{ContentBlock, Message, Role};

    #[test]
    fn approved_plan_prompt_embeds_plan_excerpt() {
        let prompt = build_approved_plan_prompt("1. Read foo\n2. Edit bar");
        assert!(prompt.contains("## Approved plan"));
        assert!(prompt.contains("1. Read foo"));
        assert!(prompt.contains("Do NOT ask for approval"));
        assert!(prompt.contains("Start with the first concrete tool call"));
    }

    #[test]
    fn last_assistant_plan_text_picks_latest_nonempty() {
        let history = vec![
            Message::user_text("plan this"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "old plan".into(),
                }],
            },
            Message::user_text("ok"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "fresh plan body".into(),
                }],
            },
        ];
        assert_eq!(last_assistant_plan_text(&history), "fresh plan body");
    }
}

#[cfg(test)]
mod loop_prompt_tests {
    use super::{build_loop_task_prompt, loop_turn_is_done, LOOP_DONE_SENTINEL};
    use smith_core::{ContentBlock, Message, Role};

    #[test]
    fn loop_task_prompt_embeds_task_and_sentinel() {
        let prompt = build_loop_task_prompt("fix the flaky test");
        assert!(prompt.contains("fix the flaky test"));
        assert!(prompt.contains(LOOP_DONE_SENTINEL));
        assert!(prompt.contains("Don't ask the user whether to continue"));
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[test]
    fn loop_turn_is_done_detects_sentinel_in_latest_assistant_reply() {
        let history = vec![
            Message::user_text("do the task"),
            assistant("still working, no sentinel here"),
            Message::user_text("continue"),
            assistant("all done.\nLOOP_DONE"),
        ];
        assert!(loop_turn_is_done(&history));
    }

    #[test]
    fn loop_turn_is_done_false_without_sentinel() {
        let history = vec![
            Message::user_text("do the task"),
            assistant("working on it, not done yet"),
        ];
        assert!(!loop_turn_is_done(&history));
    }

    #[test]
    fn loop_turn_is_done_false_on_empty_history() {
        assert!(!loop_turn_is_done(&[]));
    }
}

#[cfg(test)]
mod environment_tests {
    use super::{environment_block, environment_now};
    use chrono::{FixedOffset, TimeZone};

    #[test]
    fn environment_block_renders_weekday_iso_date_and_offset() {
        let brt = FixedOffset::west_opt(3 * 3600).unwrap();
        let block = environment_block(brt.with_ymd_and_hms(2026, 8, 5, 14, 30, 0).unwrap());

        assert!(block.contains("2026-08-05"), "missing ISO date: {block}");
        assert!(block.contains("Wednesday"), "missing weekday: {block}");
        assert!(block.contains("UTC-03:00"), "missing offset: {block}");
    }

    #[test]
    fn environment_block_tells_the_model_to_override_its_training_year() {
        let block = environment_block(
            FixedOffset::east_opt(0)
                .unwrap()
                .with_ymd_and_hms(2026, 8, 5, 0, 0, 0)
                .unwrap(),
        );
        assert!(block.contains("LATER than your training data"));
        assert!(block.contains("web_search"));
    }

    #[test]
    fn environment_now_reports_a_plausible_current_date() {
        // Guards the wiring: `environment_now` must read the real clock, not
        // some constant baked in at build time.
        let now = environment_now();
        let year = chrono::Local::now().format("%Y").to_string();
        assert!(now.contains(&year), "expected {year} in: {now}");
    }
}
