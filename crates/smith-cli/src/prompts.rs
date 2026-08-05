//! System prompt and the prompt-building/reply-parsing helpers for the
//! `/plan` and `/loop` flows. Pure string/history manipulation, no I/O — kept
//! apart from `orchestrator.rs` (which drives the actual turns) so this half
//! can be tuned and tested without touching any async/channel plumbing.

use chrono::{DateTime, FixedOffset, Local};
use smith_config::MemoryCache;
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

/// The whole volatile half of the system prompt: environment context followed
/// by `SMITH.md` project memory. This is what goes into
/// `Agent::with_context_provider`.
///
/// Order is the point of this function existing. `Agent::effective_system`
/// emits the static `SYSTEM_PROMPT`, then whatever this returns, then the
/// session goal — so the final prompt reads:
///
/// 1. `SYSTEM_PROMPT` — byte-identical forever, so a provider's prefix cache
///    keeps hitting. Nothing volatile may go in front of it.
/// 2. Environment — the date. Fixed for a session in all but the pathological
///    case, and short.
/// 3. Project memory — user-authored and editable *during* the session
///    (`/remember`, or just opening SMITH.md). It sits behind the environment
///    block precisely because it is the more mutable of the two: an edit then
///    invalidates less of the cached prefix than it would the other way round.
/// 4. The goal — last, and deliberately so. Memory is what is always true of
///    this project; the goal is what the user asked for in this session. The
///    more specific and more recent instruction belongs closest to the
///    conversation, and if a `SMITH.md` and the goal disagree the goal must
///    win — otherwise a file checked into the repo could countermand what the
///    user typed thirty seconds ago.
pub fn context_provider(memory: MemoryCache) -> impl Fn() -> String + Send + Sync + 'static {
    move || {
        let environment = environment_now();
        let memory = memory.render();
        if memory.trim().is_empty() {
            environment
        } else {
            format!("{environment}\n\n{memory}")
        }
    }
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

/// The one place the whole system prompt is asserted end to end: a real
/// `Agent` with a real context provider, driven through one turn, with the
/// request the provider actually received inspected afterwards.
///
/// `Agent::effective_system` is private and lives in a crate this feature
/// must not edit, so ordering can only be checked from the outside — which is
/// the right place anyway, since it is the sent bytes that matter.
#[cfg(test)]
mod system_prompt_composition_tests {
    use std::sync::Arc;

    use smith_config::{MemoryCache, MemoryScope};
    use smith_core::testkit::ScriptedProvider;
    use smith_core::{Agent, ToolContext};
    use smith_tools::ToolRegistry;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Runs one scripted turn and returns the system prompt that was sent.
    async fn system_prompt_for(memory_body: Option<&str>, goal: Option<&str>) -> String {
        let tmp = tempfile::tempdir().unwrap();
        if let Some(body) = memory_body {
            std::fs::write(tmp.path().join("SMITH.md"), body).unwrap();
        }
        // `global_dir: None` keeps the developer's own ~/.smith/SMITH.md out
        // of the assertion.
        let memory = MemoryCache::new(MemoryScope::new(None, tmp.path(), tmp.path()));

        let provider = Arc::new(ScriptedProvider::text("ok"));
        let mut agent = Agent::new(
            provider.clone(),
            Arc::new(ToolRegistry::with_builtin_tools()),
            "test-model".to_string(),
            ToolContext::new(tmp.path(), "test-session"),
        )
        .with_system(super::SYSTEM_PROMPT)
        .with_context_provider(super::context_provider(memory));
        agent.set_goal(goal.map(str::to_string));

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();
        agent
            .run_turn(
                "hello".to_string(),
                event_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        provider
            .last_request()
            .expect("a request was sent")
            .system
            .expect("a system prompt was sent")
    }

    #[tokio::test]
    async fn static_prompt_then_environment_then_memory_then_goal() {
        let system = system_prompt_for(
            Some("commit messages stay under 50 chars"),
            Some("ship login"),
        )
        .await;

        let base = system
            .find("You are smith, a terminal-based coding agent")
            .expect("static prompt");
        let environment = system.find("## Environment").expect("environment block");
        let memory = system.find("## Project memory").expect("memory block");
        let goal = system.find("Current session goal").expect("goal");

        // The static prompt first is load-bearing, not cosmetic: it is what a
        // provider's prefix cache keys on, and anything volatile in front of
        // it invalidates the cache on every request.
        assert_eq!(base, 0, "static prompt is not the prefix:\n{system}");
        assert!(
            base < environment && environment < memory && memory < goal,
            "wrong order in:\n{system}"
        );
        assert!(system.contains("commit messages stay under 50 chars"));
    }

    #[tokio::test]
    async fn a_goal_still_lands_last_when_there_is_no_memory() {
        let system = system_prompt_for(None, Some("ship login")).await;
        assert!(!system.contains("## Project memory"));
        let environment = system.find("## Environment").unwrap();
        let goal = system.find("Current session goal").unwrap();
        assert!(environment < goal, "wrong order in:\n{system}");
    }

    #[tokio::test]
    async fn memory_alone_composes_with_no_goal_set() {
        let system = system_prompt_for(Some("always run the tests"), None).await;
        assert!(system.contains("always run the tests"));
        assert!(!system.contains("Current session goal"));
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
    fn context_provider_without_memory_is_just_the_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = smith_config::MemoryScope::new(None, tmp.path(), tmp.path());
        let context = super::context_provider(smith_config::MemoryCache::new(scope))();
        assert!(context.contains("## Environment"));
        assert!(!context.contains("## Project memory"));
        // No trailing separator left behind by the absent memory block.
        assert_eq!(context.trim_end(), context);
    }

    #[test]
    fn context_provider_puts_memory_after_the_environment_block() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("SMITH.md"), "this project uses tabs").unwrap();
        let scope = smith_config::MemoryScope::new(None, tmp.path(), tmp.path());
        let context = super::context_provider(smith_config::MemoryCache::new(scope))();

        let environment = context.find("## Environment").expect("environment block");
        let memory = context.find("## Project memory").expect("memory block");
        assert!(environment < memory, "wrong order in:\n{context}");
        assert!(context.contains("this project uses tabs"));
    }

    #[test]
    fn context_provider_picks_up_an_edit_made_mid_session() {
        // The volatility decision, asserted: memory is re-read (behind a
        // fingerprint check), not frozen at construction.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("SMITH.md");
        std::fs::write(&path, "the first rule").unwrap();
        let scope = smith_config::MemoryScope::new(None, tmp.path(), tmp.path());
        let provider = super::context_provider(smith_config::MemoryCache::new(scope));

        assert!(provider().contains("the first rule"));
        std::fs::write(&path, "a second rule, of a different length").unwrap();
        assert!(provider().contains("a second rule"));
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
