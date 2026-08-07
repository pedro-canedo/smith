use std::collections::VecDeque;
use std::time::{Duration, Instant};

use smith_core::{Action, AgentPhase, PermissionPolicy, ResourceStats, Task, Usage};

use crate::components::input::TextInput;
use ratatui::layout::Rect;

use crate::complete::{self, CompletionKind};
use crate::keymap::KeyMap;
use crate::logbuf::LogBuffer;
use crate::slash::SlashRegistry;
use crate::theme::Theme;
use crate::transcript::TranscriptCache;

mod chatline;
mod chrome;
mod commands;
mod events;
mod keys;
mod labels;
mod modal;

pub(crate) use chatline::LineStamp;
pub use chatline::{ChatLine, ChatRole, GroupSummary, GroupedCall};
pub use chrome::{
    IdleHint, Overlay, OverlayBody, SidebarTab, TuiConfig, HISTORY_LIMIT, LOG_PANEL_TITLE,
};
pub(crate) use labels::{group_labels, tool_labels};
pub use modal::{
    format_thought, ActivityStatus, Modal, ModelPicker, PermissionModal, PlanModal, QuestionModal,
};

/// How often the throbber advances. Shared with the event loop's tick
/// interval (`crate::SPINNER_INTERVAL`) so a card's own phase — derived from
/// its `started_at` — advances at the same rate as the global counter.
pub const SPINNER_INTERVAL_MS: u128 = 120;

const MAX_LABEL_CHARS: usize = 64;

/// Minimum gap (seconds) between events before we emit a `Thought` row —
/// anything shorter is just provider latency, not a real thinking pause.
const THOUGHT_THRESHOLD_SECS: f32 = 0.5;

/// How long a first `Ctrl+C` stays armed. Long enough to read the hint and
/// press again on purpose, short enough that a stray `Ctrl+C` from minutes
/// ago can't combine with a fresh one to throw the session away.
const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// The prompt history and where the user is in it.
///
/// One struct because the three fields are meaningless apart and the invariant
/// that ties them — `draft` holds the user's own half-typed text exactly while
/// `pos` is `Some` — was written in a doc comment and enforced by nothing.
#[derive(Default)]
pub(crate) struct PromptHistory {
    /// Prompts submitted in this project, oldest last — `entries[0]` is the
    /// most recent, so "one step back" is a plain index.
    ///
    /// Seeded on startup from the resumed session's own user messages rather
    /// than from a separate history file: the messages are already persisted,
    /// already scoped to this project, and cannot drift out of sync with the
    /// conversation they came from.
    pub(crate) entries: Vec<String>,
    /// How far back the user has walked, or `None` while editing their own text.
    pos: Option<usize>,
    draft: String,
}

impl PromptHistory {
    fn new(entries: Vec<String>) -> Self {
        Self {
            entries,
            pos: None,
            draft: String::new(),
        }
    }

    /// One entry further back, given whatever is in the prompt right now.
    /// `None` means there was nothing to step to.
    pub(crate) fn back(&mut self, current: &str) -> Option<String> {
        let next = match self.pos {
            None => 0,
            Some(i) => i + 1,
        };
        if next >= self.entries.len() {
            return None;
        }
        if self.pos.is_none() {
            // Set aside whatever was half-typed, so walking forward again
            // brings it back instead of losing it.
            self.draft = current.to_string();
        }
        self.pos = Some(next);
        Some(self.entries[next].clone())
    }

    /// One entry forward; past the newest, the saved draft.
    pub(crate) fn forward(&mut self) -> Option<String> {
        let i = self.pos?;
        if i == 0 {
            self.pos = None;
            Some(std::mem::take(&mut self.draft))
        } else {
            self.pos = Some(i - 1);
            Some(self.entries[i - 1].clone())
        }
    }

    /// Record a submitted prompt and leave history-walking mode.
    ///
    /// Consecutive duplicates collapse — holding Enter on the same message, or
    /// resubmitting a recalled one, should not make Up press twice to get past
    /// it.
    pub(crate) fn remember(&mut self, text: &str) {
        self.pos = None;
        self.draft.clear();
        if text.trim().is_empty() {
            return;
        }
        if self.entries.first().is_some_and(|h| h == text) {
            return;
        }
        self.entries.insert(0, text.to_string());
        self.entries.truncate(HISTORY_LIMIT);
    }
}

/// The clocks and counters behind the status bar's elapsed time and tok/s.
///
/// One struct because all five are reset together at a turn boundary — and
/// before this they were reset at eight scattered sites, only three of which
/// did the whole job. See `begin_turn`.
#[derive(Default)]
pub(crate) struct TurnMetrics {
    pub(crate) started_at: Option<Instant>,
    /// First assistant text delta of the current provider stream (per round).
    pub(crate) stream_started_at: Option<Instant>,
    /// Characters received in the current stream — for the live tok/s estimate.
    stream_output_chars: u32,
    /// Live estimate while streaming (`chars/4 / elapsed`).
    pub(crate) live_tokens_per_sec: Option<f32>,
    /// Last measured rate from provider `output_tokens / elapsed`.
    pub(crate) tokens_per_sec: Option<f32>,
    /// Recent tok/s readings, oldest first — what the sidebar sparkline
    /// draws. Sampled on a clock rather than per delta: deltas arrive in
    /// bursts of wildly different sizes, so a per-delta series plots the
    /// provider's chunking, not the throughput.
    throughput: VecDeque<u64>,
    last_sample_at: Option<Instant>,
}

/// Samples kept for the throughput sparkline. A little over two sidebar
/// widths, so the graph still has history to lose when the pane is wide.
const MAX_THROUGHPUT_SAMPLES: usize = 64;

/// How often a sample is taken. At the 120 ms spinner tick a per-tick series
/// would cover eight seconds and jitter with every chunk boundary; half a
/// second gives the 64 samples about half a minute of turn to describe.
const THROUGHPUT_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

impl TurnMetrics {
    /// A turn is starting: the clock runs, and nothing about the last stream
    /// carries over.
    ///
    /// The five sites that used to set only the clock — `/compact`, `/plan`,
    /// plan approval, `/loop`, and the plan-mode submit in `on_key` — left the
    /// previous turn's live rate on screen until the new turn's first delta
    /// arrived. They call this now, so they no longer do.
    pub(crate) fn begin_turn(&mut self) {
        self.started_at = Some(Instant::now());
        self.end_stream();
    }

    /// The stream ended, or never began. `tokens_per_sec` deliberately
    /// survives: it is the last *measured* rate, and the status bar goes on
    /// showing it between rounds rather than blanking.
    pub(crate) fn end_stream(&mut self) {
        self.stream_started_at = None;
        self.stream_output_chars = 0;
        self.live_tokens_per_sec = None;
    }

    /// The turn is over.
    pub(crate) fn clear(&mut self) {
        self.started_at = None;
        self.end_stream();
    }

    /// One assistant text delta arrived.
    pub(crate) fn note_delta(&mut self, chars: u32) {
        if self.stream_started_at.is_none() {
            self.stream_started_at = Some(Instant::now());
            self.stream_output_chars = 0;
        }
        self.stream_output_chars = self.stream_output_chars.saturating_add(chars);
        if let Some(started) = self.stream_started_at {
            let elapsed = started.elapsed().as_secs_f32().max(0.05);
            // Providers rarely stream mid-turn usage; ~4 chars/token is a
            // rough live estimate until TokenUsage arrives.
            let est_tokens = self.stream_output_chars as f32 / 4.0;
            self.live_tokens_per_sec = Some(est_tokens / elapsed);
        }
    }

    /// Roughly how many output tokens the current stream has produced.
    pub(crate) fn live_output_tokens_estimate(&self) -> Option<u32> {
        self.stream_started_at.map(|_| self.stream_output_chars / 4)
    }

    /// Records one throughput reading if the sample clock has come round.
    ///
    /// Only while a stream is actually running: sampling the gaps would draw
    /// a trough for every pause between rounds, which reads as the model
    /// slowing down rather than as it not being the model's turn.
    pub(crate) fn sample_throughput(&mut self) {
        if self.stream_started_at.is_none() {
            return;
        }
        let due = self
            .last_sample_at
            .is_none_or(|at| at.elapsed() >= THROUGHPUT_SAMPLE_INTERVAL);
        if !due {
            return;
        }
        self.last_sample_at = Some(Instant::now());
        let rate = self
            .live_tokens_per_sec
            .or(self.tokens_per_sec)
            .unwrap_or(0.0);
        if self.throughput.len() == MAX_THROUGHPUT_SAMPLES {
            self.throughput.pop_front();
        }
        self.throughput.push_back(rate.max(0.0).round() as u64);
    }

    /// Appends one reading, bypassing the sample clock — tests need a series
    /// without waiting half a second per point.
    #[cfg(test)]
    pub(crate) fn push_throughput_sample_for_test(&mut self, rate: u64) {
        if self.throughput.len() == MAX_THROUGHPUT_SAMPLES {
            self.throughput.pop_front();
        }
        self.throughput.push_back(rate);
    }

    /// The series, oldest first. Empty until a stream has run long enough to
    /// be sampled twice — one point is not a graph, and drawing it as one
    /// makes a flat bar look like a measurement.
    pub(crate) fn throughput(&self) -> &VecDeque<u64> {
        &self.throughput
    }
}

pub struct App {
    pub input: TextInput,
    pub lines: Vec<ChatLine>,
    pub should_quit: bool,
    pub banner: String,
    pub waiting_on_assistant: bool,
    /// The one overlay on screen right now, if any — permission prompt,
    /// plan review, or clarifying question (see `Modal`).
    pub modal: Modal,
    /// Coarse agent status for chrome (thinking / building / asking / …).
    pub phase: AgentPhase,
    /// Current render offset into the message pane, kept in sync by
    /// `ui::draw_messages` (which knows the actual content/viewport height).
    pub scroll: u16,
    /// True unless the user has manually scrolled up; when true the pane
    /// pins to the latest content on every redraw.
    pub follow_bottom: bool,
    /// Streaming text for the assistant reply currently in flight, if any.
    pub in_flight_text: Option<String>,
    /// Advanced on a timer while something is animating (thinking/running).
    pub spinner_frame: usize,
    /// Ember design tokens — the single source of truth for every color.
    pub theme: Theme,
    /// When true, *every* tool card expands. Now only a programmatic default
    /// — `Ctrl+O` drives per-card expansion instead (see `toggle_card_focus`)
    /// — but it stays in `LayoutKey`, because it is still global.
    pub verbose_tools: bool,
    /// Set when the selection cursor moves; consumed by `ui::draw_messages`,
    /// the only place that knows which rows the selected card occupies.
    pub(crate) scroll_to_selected: bool,
    /// Start of the current "thinking" gap — when the next activity arrives
    /// we emit a `Thought` row if the gap exceeds `THOUGHT_THRESHOLD_SECS`.
    thinking_since: Option<Instant>,
    pub provider_label: String,
    pub model_label: String,
    pub cwd_display: String,
    pub git_branch: Option<String>,
    pub idle_hint: IdleHint,
    pub usage: Usage,
    /// Latest context-window occupancy reported by the agent: tokens used,
    /// the model's window, and whether the figure includes an estimate.
    /// `None` until the first `ContextUsage` event of the session.
    pub context: Option<(u32, u32, bool)>,
    /// Latest local-machine resource snapshot (Ollama only; `None` for
    /// token-billed providers, which show a cost estimate instead).
    pub resources: Option<ResourceStats>,
    /// Which key runs which discretionary command. Defaults to what used to
    /// be hardcoded; `[keys]` in the config moves any of them.
    pub keys: KeyMap,
    /// Where the transcript was drawn last frame, so a click can be turned
    /// into a row in it. Recorded by `ui::draw_messages`, which is the only
    /// place that knows the rect.
    pub(crate) message_area: Rect,
    /// Paths under the project root, for `@` completion. Built on the first
    /// `@` rather than at startup: walking a large repository is not something
    /// to spend on a session that never types one.
    file_index: Option<Vec<String>>,
    /// What the suggestion list is currently offering.
    pub completion_kind: CompletionKind,
    pub(crate) history: PromptHistory,
    /// Prompts typed while a turn was already running, oldest first.
    ///
    /// Being unable to type the next thing until the agent stops is the wrong
    /// trade for an agent that runs for minutes: the most common reason to
    /// speak mid-turn is to *add* to what you just asked. They are held here,
    /// shown above the prompt, and sent one at a time as the agent frees up —
    /// never merged into the running turn, which would change a request the
    /// user can already see being worked on.
    pub queued: std::collections::VecDeque<String>,
    /// The read-only panel on screen, if any — see `Overlay`. Yields to a
    /// real `Modal`, which is why both can be set without ambiguity.
    pub overlay: Option<Overlay>,
    /// Diagnostics from the whole workspace, shown by `Ctrl+L`. Shared with
    /// the `tracing` subscriber `smith-cli` installs.
    pub logs: LogBuffer,
    /// The web console's URL, shown on the idle splash and in the sidebar
    /// when the session was started with `--web`.
    pub console_url: Option<String>,
    /// Money spent this session, as the agent reports it: `(usd, unpriced
    /// turns)`. `None` until the first `SessionCost` event.
    ///
    /// **Never computed here.** The TUI used to carry its own price table and
    /// multiply it by the running token count, which meant a resumed session
    /// displayed today's prices applied to last month's tokens. The agent
    /// sums the cost recorded at the time of each turn, seeded on `--resume`
    /// from the `turns` table; this field just holds what it says.
    pub session_cost: Option<(f64, u32)>,
    /// Whether the sidebar is on screen. `Ctrl+B` toggles it; hiding it hands
    /// its 28 columns back to the transcript, which is what you want while
    /// reading a wide diff.
    pub sidebar_visible: bool,
    /// Section of the sidebar currently shown — cycled with `Shift+Tab`.
    pub sidebar_tab: SidebarTab,
    pub permission_policy: PermissionPolicy,
    /// User messages submitted this session — for `/usage`.
    pub request_count: u32,
    /// Tool calls started this session — for `/usage`.
    pub tool_call_count: u32,
    /// True while a `/plan` proposal awaits approval (modal or `/plan approve`).
    pub plan_gated: bool,
    /// True between `/plan <task>` submit and the planning turn's final reply.
    pub plan_turn_active: bool,
    /// Highlighted row in the slash-command suggestion list.
    pub slash_selected: usize,
    /// Built-in and custom slash commands. Custom ones are prompts on disk,
    /// so they can never displace a built-in — see `crate::slash`.
    pub commands: SlashRegistry,
    /// Session objective set via `/goal`, persisted to `.smith/goal.md`.
    pub goal: Option<String>,
    /// Live checklist maintained by the agent via `write_tasks` — the full
    /// list is replaced wholesale on every update, no client-side diffing.
    pub tasks: Vec<Task>,
    /// True for the whole span of a `/loop` run (all iterations), so a turn
    /// finishing mid-loop doesn't drop the UI back to idle between rounds.
    pub loop_active: bool,
    /// `(iteration, max_iterations)` of the loop round in flight, if any.
    pub loop_progress: Option<(u32, u32)>,
    pub(crate) metrics: TurnMetrics,
    /// When the first of the two `Ctrl+C` presses landed, if the quit is
    /// currently armed — see `quit_pending`.
    quit_armed_at: Option<Instant>,
    /// Memoised rows for `lines` — see `crate::transcript`. Rebuilding the
    /// transcript from scratch each frame is O(whole session) per keystroke,
    /// which is what this replaces.
    pub(crate) transcript: TranscriptCache,
}

impl App {
    pub fn new(config: TuiConfig) -> Self {
        let theme = config.theme;
        Self {
            input: TextInput::new(&theme),
            lines: config.initial_lines,
            should_quit: false,
            banner: config.banner,
            waiting_on_assistant: false,
            modal: Modal::None,
            phase: AgentPhase::Idle,
            scroll: 0,
            follow_bottom: true,
            in_flight_text: None,
            spinner_frame: 0,
            theme,
            verbose_tools: false,
            scroll_to_selected: false,
            thinking_since: None,
            provider_label: config.provider_label,
            model_label: config.model_label,
            cwd_display: config.cwd_display,
            git_branch: config.git_branch,
            idle_hint: config.idle_hint,
            usage: Usage::default(),
            context: None,
            resources: None,
            keys: config.keys,
            message_area: Rect::default(),
            file_index: None,
            completion_kind: CompletionKind::default(),
            history: PromptHistory::new(config.history),
            queued: std::collections::VecDeque::new(),
            overlay: None,
            logs: config.logs,
            console_url: config.console_url,
            session_cost: None,
            sidebar_visible: true,
            sidebar_tab: SidebarTab::default(),
            permission_policy: config.permission_policy,
            request_count: 0,
            tool_call_count: 0,
            plan_gated: false,
            plan_turn_active: false,
            slash_selected: 0,
            commands: config.commands,
            goal: config.goal,
            tasks: config.tasks,
            loop_active: false,
            loop_progress: None,
            metrics: TurnMetrics::default(),
            quit_armed_at: None,
            transcript: TranscriptCache::default(),
        }
    }

    /// Completions for what the caret is on: slash commands for `/cmd`, file
    /// paths for `@path`, nothing otherwise.
    ///
    /// `&mut` because the file index is built here, on the first `@` of the
    /// session. Walking the repository at startup would charge every session
    /// for a feature most never use, and doing it per keystroke would charge
    /// the ones that do, on every keystroke.
    pub fn suggestions(&mut self) -> Vec<crate::slash::SlashSuggestion> {
        let text = self.input.text();
        if let Some(token) = complete::file_token(&text) {
            self.completion_kind = CompletionKind::File;
            let files = self
                .file_index
                .get_or_insert_with(|| complete::index_files(std::path::Path::new(".")));
            return complete::file_suggestions(files, token);
        }
        self.completion_kind = CompletionKind::Slash;
        self.commands.suggestions_for(&text)
    }

    /// Bracketed paste: text arrives as one event, so embedded newlines land
    /// in the buffer instead of submitting the prompt at the first one.
    pub fn on_paste(&mut self, text: &str) {
        if self.waiting_on_assistant {
            return;
        }
        if let Some(modal) = self.modal.question_mut() {
            modal.custom.push_str(text);
            return;
        }
        self.input.insert_str(text);
        self.slash_selected = 0;
    }

    /// Whether the chrome should use plan-mode styling (gated or planning in flight).
    pub fn in_plan_mode(&self) -> bool {
        self.plan_gated || self.plan_turn_active || self.modal.is_plan()
    }

    /// Rate shown in the sidebar: live estimate while streaming, else last measured.
    pub fn display_tokens_per_sec(&self) -> Option<f32> {
        if self.waiting_on_assistant {
            self.metrics
                .live_tokens_per_sec
                .or(self.metrics.tokens_per_sec)
        } else {
            self.metrics.tokens_per_sec
        }
    }

    /// Seconds since the current turn started, for the "thinking… 12s" style
    /// status line — `None` when idle.
    pub fn turn_elapsed_secs(&self) -> Option<f32> {
        self.metrics.started_at.map(|t| t.elapsed().as_secs_f32())
    }

    /// Rough output-token estimate for the round currently streaming in
    /// (~4 chars/token), for the same status line — `None` before any text
    /// has arrived this round.
    pub fn live_output_tokens_estimate(&self) -> Option<u32> {
        self.metrics.live_output_tokens_estimate()
    }

    /// Advances the spinner animation; call on a timer while `is_animating()`.
    pub fn tick(&mut self) {
        // Not reduced modulo any frame count: the ASCII and Unicode sets have
        // different lengths, so the wrap belongs at the indexing site.
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        // The one clock the UI already has. `sample_throughput` decides
        // whether enough of it has passed, so the sparkline's resolution does
        // not silently become whatever the spinner interval happens to be.
        self.metrics.sample_throughput();
    }

    /// Whether anything on screen is actually moving.
    ///
    /// The event loop redraws on a timer only while this is true, so it is the
    /// whole of acceptance criterion #10: an idle smith must do no work.
    ///
    /// **Waiting on the user is not animation.** This used to return `true`
    /// for *any* open modal, which meant a permission prompt redrew the whole
    /// frame eight times a second for as long as the user took to read it —
    /// while a spinner told them work was in progress and nothing whatsoever
    /// was happening. Both halves were wrong: the wakeups and the claim.
    ///
    /// A tool card mid-flight is still checked in those phases, because a
    /// parallel read-only call can genuinely be running behind the prompt.
    pub fn is_animating(&self) -> bool {
        if matches!(
            self.phase,
            AgentPhase::WaitingPermission | AgentPhase::Asking
        ) {
            return self.lines.iter().any(ChatLine::is_animating);
        }
        self.waiting_on_assistant || !matches!(self.phase, AgentPhase::Idle)
    }

    // --- Card focus -------------------------------------------------------
    //
    // The transcript is a list, and a list needs a cursor. The cursor lives on
    // the `ChatLine`s themselves (`ChatLine::selected`) rather than as an
    // index here: an index would have to be fixed up every time a line is
    // appended mid-stream, and would silently point at the wrong card if it
    // ever wasn't. Scanning for the flag is O(lines) on a keystroke, which is
    // nothing next to the render it replaces.

    /// Position of the tool card the cursor is on. `None` means the input owns
    /// the keyboard — the default, and what `Esc`/`Ctrl+O` return to.
    pub fn selected_card(&self) -> Option<usize> {
        self.lines.iter().position(ChatLine::selected)
    }

    /// The tool call id under the cursor, for tests and for anything that has
    /// to survive a re-render.
    pub fn selected_card_id(&self) -> Option<&str> {
        self.selected_card().and_then(|i| self.lines[i].tool_id())
    }

    fn last_card(&self) -> Option<usize> {
        self.lines.iter().rposition(|l| l.role() == ChatRole::Tool)
    }

    /// `Ctrl+O`: enter card focus at the newest tool card, or leave it.
    /// Returns whether focus is now held.
    pub fn toggle_card_focus(&mut self) -> bool {
        if self.selected_card().is_some() {
            self.select_card(None);
            return false;
        }
        match self.last_card() {
            Some(index) => {
                self.select_card(Some(index));
                true
            }
            None => false,
        }
    }

    /// Moves the cursor to the next/previous tool card. Clamps rather than
    /// wraps: wrapping from the newest card to the oldest in a thousand-line
    /// session is never what the keystroke meant.
    pub fn move_card_focus(&mut self, forward: bool) -> bool {
        let Some(current) = self.selected_card() else {
            return false;
        };
        let is_card = |(_, l): &(usize, &ChatLine)| l.role() == ChatRole::Tool;
        let next = if forward {
            self.lines
                .iter()
                .enumerate()
                .skip(current + 1)
                .find(is_card)
                .map(|(i, _)| i)
        } else {
            self.lines
                .iter()
                .enumerate()
                .take(current)
                .rev()
                .find(is_card)
                .map(|(i, _)| i)
        };
        match next {
            Some(index) => {
                self.select_card(Some(index));
                true
            }
            None => {
                // Still ask for a scroll: the user pressed a key and deserves
                // to see the edge card they are already on.
                self.scroll_to_selected = true;
                false
            }
        }
    }

    /// `Enter` in card focus. Returns whether a card was actually toggled.
    pub fn toggle_selected_card(&mut self) -> bool {
        let Some(index) = self.selected_card() else {
            return false;
        };
        self.lines[index].toggle_expanded();
        self.scroll_to_selected = true;
        true
    }

    fn select_card(&mut self, index: Option<usize>) {
        for (i, line) in self.lines.iter_mut().enumerate() {
            if line.role() == ChatRole::Tool {
                line.set_selected(Some(i) == index);
            }
        }
        self.scroll_to_selected = index.is_some();
        if index.is_some() {
            // Pinning to the live edge would yank the viewport off the card
            // the user just pointed at, on the very next streaming delta.
            self.follow_bottom = false;
        }
    }

    pub fn phase_label(&self) -> &'static str {
        match self.phase {
            AgentPhase::Idle => "idle",
            AgentPhase::Thinking => "thinking…",
            AgentPhase::Planning => "planning…",
            AgentPhase::Building => "building…",
            AgentPhase::Asking => "asking…",
            AgentPhase::WaitingPermission => "waiting for permission…",
            AgentPhase::Working => "working…",
            AgentPhase::Looping => "looping…",
        }
    }

    /// Start tracking a thinking gap — called when the model finishes a
    /// response and is about to call tools, or after a tool result arrives.
    fn begin_thinking(&mut self) {
        if self.thinking_since.is_none() {
            self.thinking_since = Some(Instant::now());
        }
    }

    /// The card an incoming call of `name` folds into, with whatever was only
    /// separating the two already dropped. `None` when the call opens a new
    /// card.
    ///
    /// A run stays open across a `Thought` row and closes on anything else.
    /// That row is the pause *inside* one activity and the card's own timer
    /// already covers it, so letting it close the run put the transcript back
    /// to one card per search — the stack this exists to remove. A reply, a
    /// system line or a different tool's card do close it: those read as the
    /// agent moving on, and folding a call into a card that sits above them
    /// would reorder the transcript.
    ///
    /// The `Thought` rows are dropped rather than stepped over, because a card
    /// is appended to in place: leaving them would put the group's newest step
    /// above rows that belong to the gap before it.
    fn join_groupable_card(&mut self, name: &str) -> Option<usize> {
        let mut end = self.lines.len();
        while end > 0 && self.lines[end - 1].role() == ChatRole::Thought {
            end -= 1;
        }
        let index = end.checked_sub(1)?;
        if !self.lines[index].can_group(name) {
            return None;
        }
        self.lines.truncate(end);
        Some(index)
    }

    /// End the thinking gap — called when the first text delta arrives or
    /// a tool call starts. If the gap exceeded the threshold, emit a
    /// `Thought` row.
    fn end_thinking(&mut self) {
        if let Some(started) = self.thinking_since.take() {
            let secs = started.elapsed().as_secs_f32();
            if secs >= THOUGHT_THRESHOLD_SECS {
                self.lines.push(ChatLine::thought(secs));
            }
        }
    }

    /// True while a first `Ctrl+C` is waiting for its confirmation — the
    /// status bar shows the hint for exactly this long.
    pub fn quit_pending(&self) -> bool {
        self.quit_armed_at
            .is_some_and(|at| at.elapsed() < QUIT_CONFIRM_WINDOW)
    }

    /// Drops an armed quit whose window has lapsed, reporting whether it
    /// actually cleared one. The event loop uses the return value to force
    /// one more redraw so the hint disappears on its own, without a keypress.
    pub fn expire_pending_quit(&mut self) -> bool {
        if self.quit_armed_at.is_some() && !self.quit_pending() {
            self.quit_armed_at = None;
            return true;
        }
        false
    }

    /// Hands back the next queued prompt once the agent is free, as the
    /// `Action` the run loop would have got from `Enter`.
    ///
    /// Polled by the run loop rather than returned from `on_agent_event`,
    /// which several different events would otherwise have to remember to do —
    /// `AssistantTurnComplete`, `LoopFinished`, `TurnLimitReached` and `Error`
    /// all end a turn, and one of them forgetting would strand the queue
    /// silently. Asking "is the agent free and is anything waiting" cannot be
    /// forgotten by a new event arm.
    ///
    /// One at a time, and never while a modal is up: a permission prompt is
    /// the agent blocked on the user, and answering it by starting a different
    /// turn is not what the queue is for.
    pub fn take_queued_prompt(&mut self) -> Option<Action> {
        if self.waiting_on_assistant || self.modal.is_some() || self.plan_gated {
            return None;
        }
        let text = self.queued.pop_front()?;
        self.lines.push(ChatLine::new(ChatRole::User, text.clone()));
        self.waiting_on_assistant = true;
        self.phase = AgentPhase::Thinking;
        self.in_flight_text = None;
        self.metrics.begin_turn();
        self.request_count += 1;
        Some(Action::SubmitMessage(text))
    }

    /// Rows a wheel notch moves. Three is the terminal convention and what
    /// every other pager in the user's shell already does.
    const WHEEL_ROWS: u16 = 3;

    /// Rows one `PageUp`/`PageDown` moves: the transcript's own height, less
    /// two rows kept as overlap.
    ///
    /// It used to be a flat 10 regardless of the pane, which is most of a
    /// screen on a short terminal and a third of one on a tall display —
    /// paging felt like a different key at every size. The overlap is what
    /// makes a page turn readable: land exactly one screen on and the
    /// sentence that straddled the seam is gone.
    ///
    /// `message_area` is whatever the last frame recorded, so before the
    /// first draw this falls back to the old constant rather than to zero —
    /// a page key that does nothing is worse than one that moves the wrong
    /// distance once.
    fn page_rows(&self) -> u16 {
        match self.message_area.height {
            0 => 10,
            height => height.saturating_sub(2).max(1),
        }
    }

    pub(crate) fn scroll_page_up(&mut self) {
        self.follow_bottom = false;
        self.scroll = self.scroll.saturating_sub(self.page_rows());
    }

    pub(crate) fn scroll_page_down(&mut self) {
        // No `follow_bottom = true` here: the renderer re-arms it the moment
        // the offset reaches the end, so setting it early would jump past
        // whatever is between here and the tail.
        self.scroll = self.scroll.saturating_add(self.page_rows());
    }

    pub(crate) fn scroll_to_top(&mut self) {
        self.follow_bottom = false;
        self.scroll = 0;
    }

    /// Back to the live edge.
    ///
    /// Expressed as re-arming follow-the-tail rather than as a large offset:
    /// `App` does not know the document height — only the renderer does — so
    /// "the bottom" is a state, not a number.
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.follow_bottom = true;
    }

    /// Mouse input. Scrolling is the whole point; clicking selects the tool
    /// card under the pointer, which is the same selection `Ctrl+O` drives.
    pub fn on_mouse(&mut self, event: crossterm::event::MouseEvent) -> Option<Action> {
        use crossterm::event::{MouseButton, MouseEventKind};

        match event.kind {
            MouseEventKind::ScrollUp => {
                // Whatever is on top gets the wheel: a panel or modal covers
                // the transcript, so scrolling what is hidden behind it would
                // be scrolling something the user cannot see.
                if self.overlay.is_some() {
                    self.scroll_overlay(-(Self::WHEEL_ROWS as i32));
                } else if self.modal.is_some() {
                    self.scroll_modal(-(Self::WHEEL_ROWS as i32));
                } else {
                    self.follow_bottom = false;
                    self.scroll = self.scroll.saturating_sub(Self::WHEEL_ROWS);
                }
                None
            }
            MouseEventKind::ScrollDown => {
                if self.overlay.is_some() {
                    self.scroll_overlay(Self::WHEEL_ROWS as i32);
                } else if self.modal.is_some() {
                    self.scroll_modal(Self::WHEEL_ROWS as i32);
                } else {
                    self.scroll = self.scroll.saturating_add(Self::WHEEL_ROWS);
                }
                None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.click_at(event.column, event.row);
                None
            }
            _ => None,
        }
    }

    /// Select the tool card under a click, or clear the selection when the
    /// click lands somewhere that isn't one.
    fn click_at(&mut self, column: u16, row: u16) {
        if self.overlay.is_some() || self.modal.is_some() {
            return;
        }
        let area = self.message_area;
        if area.height == 0
            || row < area.y
            || row >= area.y + area.height
            || column < area.x
            || column >= area.x + area.width
        {
            return;
        }
        // Screen row -> document row: the pane starts at `area.y` and shows
        // the document from `self.scroll` down.
        let doc_row = (row - area.y) as usize + self.scroll as usize;
        let Some(index) = self.transcript.entry_at_row(doc_row) else {
            self.select_card(None);
            return;
        };
        if self
            .lines
            .get(index)
            .is_some_and(|l| l.role == ChatRole::Tool)
        {
            // A second click on the same card expands it, which is what
            // double-clicking a row means everywhere else.
            if self.selected_card() == Some(index) {
                self.toggle_selected_card();
            } else {
                self.select_card(Some(index));
            }
        } else {
            self.select_card(None);
        }
    }

    /// Step one entry further back in the prompt history. `false` means there
    /// was nothing to step to, so the caller can give the key its old meaning.
    fn history_back(&mut self) -> bool {
        let current = self.input.text();
        match self.history.back(&current) {
            Some(entry) => {
                self.input.set(&entry);
                true
            }
            None => false,
        }
    }

    /// Step one entry forward; past the newest, restore the saved draft.
    fn history_forward(&mut self) -> bool {
        match self.history.forward() {
            Some(text) => {
                self.input.set(&text);
                true
            }
            None => false,
        }
    }

    /// Record a submitted prompt and leave history-walking mode.
    ///
    /// Consecutive duplicates collapse — holding Enter on the same message,
    /// or resubmitting a recalled one, should not make Up press twice to get
    /// past it.
    fn remember_prompt(&mut self, text: &str) {
        self.history.remember(text);
    }

    /// Move the open modal by `delta` rows, if it has a scroll offset.
    /// Clamping to the end is the renderer's job, as it is for the overlay.
    fn scroll_modal(&mut self, delta: i32) {
        let scroll = match &mut self.modal {
            Modal::Permission(m) => &mut m.scroll,
            Modal::Plan(m) => &mut m.scroll,
            // The picker scrolls by moving its own cursor, so the shared
            // page-scroll does not apply to it.
            Modal::Question(_) | Modal::Model(_) | Modal::None => return,
        };
        *scroll = (*scroll as i32).saturating_add(delta).max(0) as u16;
    }

    /// Move the overlay by `delta` rows. Clamping to the *end* is left to the
    /// renderer, which is the only place that knows the panel's height; here
    /// we only keep it off the top.
    fn scroll_overlay(&mut self, delta: i32) {
        if let Some(o) = self.overlay.as_mut() {
            let next = (o.scroll as i32).saturating_add(delta).max(0);
            o.scroll = next.min(u16::MAX as i32) as u16;
        }
    }
}

#[cfg(test)]
mod tests;
