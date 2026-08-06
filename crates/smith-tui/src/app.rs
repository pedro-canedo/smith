use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use smith_core::{
    Action, AgentEvent, AgentPhase, McpCommand, PermissionDecision, PermissionPolicy,
    PermissionRequest, ResourceStats, StopReason, Task, Usage, UserQuestion,
};

use crate::components::input::TextInput;
use ratatui::layout::Rect;

use crate::complete::{self, CompletionKind};
use crate::keymap::{KeyAction, KeyMap};
use crate::logbuf::LogBuffer;
use crate::slash::SlashRegistry;
use crate::theme::Theme;
use crate::transcript::TranscriptCache;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Assistant,
    System,
    /// A tool call, kept as a permanent transcript entry so scrolling back
    /// shows what actually ran during a turn — see `ChatLine::tool_status`.
    Tool,
    /// A quiet gap between tool calls / turns, e.g. `+ Thought: 1.1s`.
    Thought,
}

/// Tools whose consecutive calls collapse into one card.
///
/// Deliberately a short list rather than "any repeated tool": the trade is
/// only worth it when the card is pure status. A search card is its query; a
/// `read_file` card can carry a diff or an error tail, and folding those would
/// hide the thing you wanted to see.
const GROUPABLE_TOOLS: &[&str] = &["web_search", "web_fetch"];

/// Source of `LineStamp`s. Process-wide and monotonic, so a stamp is never
/// reused across lines or across successive states of the same line.
static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);

/// Identity of one *rendered form* of a `ChatLine`.
///
/// The transcript memo (`crate::transcript`) keys its cached rows on this and
/// nothing else, which is only sound because a fresh stamp is drawn both when
/// a line is created and on every mutation of it. That is enforced
/// structurally rather than by discipline: every field of `ChatLine` is
/// private, so `finish_tool` and `fail_if_running` are the only writers in the
/// crate, and both end by calling `touch()`. A tool card frozen on "running"
/// after its result landed — the classic memoisation bug here — would need a
/// mutation path that skips `touch`, and there is none to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LineStamp(u64);

fn next_stamp() -> LineStamp {
    LineStamp(NEXT_STAMP.fetch_add(1, Ordering::Relaxed))
}

/// One call folded into a grouped card — see `ChatLine::grouped`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupedCall {
    pub id: String,
    /// What the call was about, e.g. the search query.
    pub label: String,
    pub status: ActivityStatus,
}

#[derive(Debug, Clone)]
pub struct ChatLine {
    role: ChatRole,
    text: String,
    /// Small dim caption under an assistant reply, e.g. "ollama · qwen3.5 · 4.2s".
    meta: Option<String>,
    /// Set only for `ChatRole::Tool` lines — the tool call id, used to find
    /// and update this same line in place when its result arrives.
    tool_id: Option<String>,
    /// Set only for `ChatRole::Tool` lines — live while the call is running,
    /// then flipped to its final state so the icon reflects outcome.
    tool_status: Option<ActivityStatus>,
    /// Tool name (e.g. `run_bash`, `read_file`) — for the card header.
    tool_name: Option<String>,
    /// Raw JSON input from the provider — used to derive the target summary
    /// and, in verbose mode, to render the full input and a diff for edits.
    tool_input: Option<serde_json::Value>,
    /// Tool output text, populated on `ToolCallResult`.
    tool_output: Option<String>,
    /// Wall-clock seconds the call took, populated on `ToolCallResult`.
    tool_secs: Option<f32>,
    /// When the call started — for the live elapsed counter in the header
    /// while the card is still `Running`, and for this card's own throbber
    /// phase (see `spinner_frame_for`).
    started_at: Option<Instant>,
    /// Set only for `ChatRole::Tool` lines: the user expanded this one card
    /// with `Enter`.
    ///
    /// Deliberately a field of the *line*, not of the `App`: every mutator
    /// here ends in `touch()`, so toggling one card invalidates exactly that
    /// card's memo entry. A global "expanded" flag would have to join
    /// `LayoutKey` instead, and re-render the whole transcript per keystroke.
    /// Further calls of the same tool folded into this card.
    ///
    /// A research turn issues six searches in a row, and six cards is six
    /// times the chrome for one activity — it reads as noise and buries what
    /// the agent is actually doing. They collapse into one card with a line
    /// per query. Held on the line rather than derived at render time so the
    /// transcript memo still keys on a single entry: a card whose rows
    /// depended on its neighbours could not be cached per line.
    grouped: Vec<GroupedCall>,
    /// How *this* card's own call ended, as distinct from the group's verdict.
    ///
    /// They differ the moment a card has children: the first search finishing
    /// must not settle a card whose second search is still running, which is
    /// exactly what happened when one field carried both.
    own_status: Option<ActivityStatus>,
    expanded: bool,
    /// Set only for `ChatRole::Tool` lines: the transcript's selection cursor
    /// is on this card. Same reasoning as `expanded` — and because nothing is
    /// keyed by position, the selection rides along with its line while new
    /// lines stream in above and below it.
    selected: bool,
    stamp: LineStamp,
}

impl ChatLine {
    pub fn new(role: ChatRole, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            meta: None,
            tool_id: None,
            tool_status: None,
            tool_name: None,
            tool_input: None,
            tool_output: None,
            tool_secs: None,
            started_at: None,
            grouped: Vec::new(),
            own_status: None,
            expanded: false,
            selected: false,
            stamp: next_stamp(),
        }
    }

    /// Attaches the dim caption shown under a finished assistant reply.
    pub fn with_meta(mut self, meta: Option<String>) -> Self {
        self.meta = meta;
        self
    }

    /// A permanent transcript entry for a tool call, starting in the
    /// `Running` state; updated in place once its result lands.
    fn tool(
        id: impl Into<String>,
        name: impl Into<String>,
        label: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self {
            tool_id: Some(id.into()),
            tool_status: Some(ActivityStatus::Running),
            tool_name: Some(name.into()),
            tool_input: Some(input),
            started_at: Some(Instant::now()),
            ..Self::new(ChatRole::Tool, label.into())
        }
    }

    /// A thought-row entry: `+ Thought: 959ms` or `+ Thought: 1.1s`.
    fn thought(secs: f32) -> Self {
        Self::new(ChatRole::Thought, format_thought(secs))
    }

    pub fn role(&self) -> ChatRole {
        self.role
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn meta(&self) -> Option<&str> {
        self.meta.as_deref()
    }

    pub fn tool_id(&self) -> Option<&str> {
        self.tool_id.as_deref()
    }

    pub fn tool_status(&self) -> Option<ActivityStatus> {
        self.tool_status
    }

    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }

    pub fn tool_input(&self) -> Option<&serde_json::Value> {
        self.tool_input.as_ref()
    }

    pub fn tool_output(&self) -> Option<&str> {
        self.tool_output.as_deref()
    }

    pub fn tool_secs(&self) -> Option<f32> {
        self.tool_secs
    }

    pub fn started_at(&self) -> Option<Instant> {
        self.started_at
    }

    /// Whether the user expanded this card with `Enter`.
    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// Whether the transcript's selection cursor is on this card.
    pub fn selected(&self) -> bool {
        self.selected
    }

    /// Toggles this card's expansion. `touch()` is what keeps the memo honest;
    /// it runs unconditionally because the value always changes here.
    fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
        self.touch();
    }

    /// Moves the selection cursor on or off this card. A no-op — including no
    /// new stamp — when the flag already holds the wanted value, so walking
    /// the whole transcript to re-sync the cursor invalidates at most the two
    /// cards that actually changed.
    fn set_selected(&mut self, selected: bool) {
        if self.selected != selected {
            self.selected = selected;
            self.touch();
        }
    }

    pub(crate) fn stamp(&self) -> LineStamp {
        self.stamp
    }

    /// True while this is a tool card whose call has not answered yet.
    pub(crate) fn is_running_tool(&self) -> bool {
        self.tool_status == Some(ActivityStatus::Running)
    }

    /// True while the card's rendered form is a function of the clock (spinner
    /// frame + live elapsed counter) rather than of this struct — such a line
    /// can never be memoised, no matter what its stamp says.
    pub(crate) fn is_animating(&self) -> bool {
        self.role == ChatRole::Tool
            && matches!(self.tool_status, Some(ActivityStatus::Running) | None)
    }

    /// Whether this card can absorb another call of `name` as a sibling.
    ///
    /// Only tools whose cards are pure status — a search's card says which
    /// query ran and nothing else, so five of them stacked are five headers
    /// around one fact. A `read_file` or an `edit_file` card carries content
    /// worth its own frame, so those are never folded.
    fn can_group(&self, name: &str) -> bool {
        self.role == ChatRole::Tool
            && self.tool_name.as_deref() == Some(name)
            && GROUPABLE_TOOLS.contains(&name)
    }

    /// Folds another call of the same tool into this card.
    fn group(&mut self, id: String, label: String) {
        self.grouped.push(GroupedCall {
            id,
            label,
            status: ActivityStatus::Running,
        });
        // The card is running again as a whole while any child is.
        self.tool_status = Some(ActivityStatus::Running);
        if self.started_at.is_none() {
            self.started_at = Some(Instant::now());
        }
        self.touch();
    }

    /// Every call this card stands for, its own first.
    pub fn grouped(&self) -> &[GroupedCall] {
        &self.grouped
    }

    /// Marks one folded call finished. `true` when this card owned it.
    fn finish_grouped(&mut self, id: &str, status: ActivityStatus) -> bool {
        let Some(child) = self.grouped.iter_mut().find(|c| c.id == id) else {
            return false;
        };
        child.status = status;
        self.settle_group();
        self.touch();
        true
    }

    /// Recomputes a grouped card's verdict from its own call and its children.
    ///
    /// Running while anything under it is; failed if any part failed, because
    /// a group reported as clean when one search was blocked would hide the
    /// only fact worth acting on.
    fn settle_group(&mut self) {
        let pending = self.own_status.is_none()
            || self
                .grouped
                .iter()
                .any(|c| c.status == ActivityStatus::Running);
        if pending {
            self.tool_status = Some(ActivityStatus::Running);
            return;
        }
        let failed = self.own_status == Some(ActivityStatus::Error)
            || self
                .grouped
                .iter()
                .any(|c| c.status == ActivityStatus::Error);
        self.tool_status = Some(if failed {
            ActivityStatus::Error
        } else {
            ActivityStatus::Done
        });
        if let Some(started) = self.started_at.take() {
            self.tool_secs = Some(started.elapsed().as_secs_f32());
        }
    }

    /// Records a tool call's outcome, on the `ToolCallResult` that ends it.
    fn finish_tool(&mut self, status: ActivityStatus, output: String) {
        self.own_status = Some(status);
        self.tool_output = Some(output);
        if !self.grouped.is_empty() {
            // A group settles on its slowest member, not its first.
            self.settle_group();
            self.touch();
            return;
        }
        self.tool_status = Some(status);
        if let Some(started) = self.started_at {
            self.tool_secs = Some(started.elapsed().as_secs_f32());
        }
        // Drop the started_at now that the call is done — nothing reads it
        // after this point, and keeping it would leave the card "animating".
        self.started_at = None;
        self.touch();
    }

    /// Shows the newest line a running tool has produced.
    ///
    /// Only the latest is kept, not a scrollback: the card is a one-line
    /// status while the tool runs, and the complete output arrives with the
    /// result anyway. `touch()` is not called — a running card is excluded
    /// from the render memo by `is_animating` already, and stamping it would
    /// invalidate nothing that isn't already being re-rendered.
    fn set_progress(&mut self, line: String) {
        if self.tool_status == Some(ActivityStatus::Running) {
            self.tool_output = Some(line);
        }
    }

    /// Marks a still-running card as failed when the turn dies under it.
    /// A no-op on any other line, so an error never invalidates the whole
    /// transcript's memo.
    fn fail_if_running(&mut self) {
        if self.tool_status == Some(ActivityStatus::Running) {
            self.tool_status = Some(ActivityStatus::Error);
            self.started_at = None;
            self.touch();
        }
    }

    fn touch(&mut self) {
        self.stamp = next_stamp();
    }

    /// Builds an arbitrary tool card for rendering tests.
    #[cfg(test)]
    pub(crate) fn test_tool(
        name: &str,
        status: ActivityStatus,
        input: serde_json::Value,
        output: Option<&str>,
    ) -> Self {
        Self {
            tool_id: Some("call_1".into()),
            tool_status: Some(status),
            tool_name: Some(name.into()),
            tool_input: Some(input),
            tool_output: output.map(str::to_string),
            tool_secs: Some(0.4),
            ..Self::new(ChatRole::Tool, format!("Running {name}"))
        }
    }

    /// A running card that started `ago` in the past — the only way to give a
    /// test a deterministic throbber phase, since a card's phase is derived
    /// from its own clock.
    #[cfg(test)]
    pub(crate) fn test_tool_started(name: &str, id: &str, ago: Duration) -> Self {
        Self {
            tool_id: Some(id.to_string()),
            started_at: Some(Instant::now() - ago),
            tool_secs: None,
            ..Self::test_tool(
                name,
                ActivityStatus::Running,
                serde_json::json!({ "path": "src/main.rs" }),
                None,
            )
        }
    }

    /// Marks this card selected, for rendering tests.
    #[cfg(test)]
    pub(crate) fn test_selected(mut self) -> Self {
        self.selected = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Done,
    Error,
}

/// Format a duration as `959ms` or `1.1s` for thought rows.
pub fn format_thought(secs: f32) -> String {
    if secs < 1.0 {
        format!("{}ms", (secs * 1000.0) as u32)
    } else {
        format!("{:.1}s", secs)
    }
}

#[derive(Debug, Clone)]
pub struct PermissionModal {
    pub request: PermissionRequest,
    /// Vertical scroll into the permission preview body.
    pub scroll: u16,
}

/// Shown after a `/plan` turn finishes — review the plan, then build or reject.
#[derive(Debug, Clone)]
pub struct PlanModal {
    pub text: String,
    pub scroll: u16,
}

/// Clarifying question from `ask_user`: three suggestions + free-text (index 3).
#[derive(Debug, Clone)]
pub struct QuestionModal {
    pub question: UserQuestion,
    /// 0..=2 = suggestions, 3 = custom text.
    pub selected: usize,
    pub custom: String,
}

/// The one overlay that can be on screen at a time. Only one interactive
/// wait is ever in flight per turn (the agent loop blocks on a single
/// `oneshot` at once), so this is a true sum type rather than three
/// independent `Option`s that happened to always be mutually exclusive in
/// practice — a state combination like "plan modal AND question modal both
/// open" is no longer representable at all, instead of just avoided by
/// convention.
#[derive(Debug, Clone, Default)]
pub enum Modal {
    #[default]
    None,
    Permission(PermissionModal),
    Plan(PlanModal),
    Question(QuestionModal),
}

impl Modal {
    pub fn is_none(&self) -> bool {
        matches!(self, Modal::None)
    }

    pub fn is_some(&self) -> bool {
        !self.is_none()
    }

    pub fn is_plan(&self) -> bool {
        matches!(self, Modal::Plan(_))
    }

    pub fn is_question(&self) -> bool {
        matches!(self, Modal::Question(_))
    }

    pub fn permission(&self) -> Option<&PermissionModal> {
        match self {
            Modal::Permission(m) => Some(m),
            _ => None,
        }
    }

    pub fn permission_mut(&mut self) -> Option<&mut PermissionModal> {
        match self {
            Modal::Permission(m) => Some(m),
            _ => None,
        }
    }

    pub fn plan(&self) -> Option<&PlanModal> {
        match self {
            Modal::Plan(m) => Some(m),
            _ => None,
        }
    }

    pub fn plan_mut(&mut self) -> Option<&mut PlanModal> {
        match self {
            Modal::Plan(m) => Some(m),
            _ => None,
        }
    }

    pub fn question(&self) -> Option<&QuestionModal> {
        match self {
            Modal::Question(m) => Some(m),
            _ => None,
        }
    }

    pub fn question_mut(&mut self) -> Option<&mut QuestionModal> {
        match self {
            Modal::Question(m) => Some(m),
            _ => None,
        }
    }
}

/// What the idle screen shows below the hint line: either a generic tip, or —
/// once a project has prior history — a pointer to resume it.
#[derive(Debug, Clone)]
pub enum IdleHint {
    Tip(String),
    NewSession { title: String },
    ContinueSession { title: String, resume_cmd: String },
}

/// Everything the TUI needs to know that isn't part of the live event stream:
/// display labels, environment info, and (when resuming) prior transcript.
pub struct TuiConfig {
    pub banner: String,
    pub provider_label: String,
    pub model_label: String,
    pub cwd_display: String,
    pub git_branch: Option<String>,
    pub idle_hint: IdleHint,
    pub initial_lines: Vec<ChatLine>,
    pub permission_policy: PermissionPolicy,
    /// Theme selected by the CLI before the TUI starts. This is how flags such
    /// as `--ascii` affect glyph capability without mutating process env.
    pub theme: Theme,
    /// Loaded from `.smith/goal.md` at startup (if any).
    pub goal: Option<String>,
    /// Restored from a resumed session's last `write_tasks` call, if any.
    pub tasks: Vec<Task>,
    /// Custom slash commands discovered under `.smith/commands/` and
    /// `~/.smith/commands/`. Empty for a frontend that does not load them.
    pub commands: SlashRegistry,
    /// Key bindings for the remappable commands. `KeyMap::default()` is the
    /// set that used to be hardcoded.
    pub keys: KeyMap,
    /// Prompts already submitted in this project, most recent first. Empty
    /// for a fresh session; on `--resume` it is the resumed conversation's
    /// own user messages.
    pub history: Vec<String>,
    /// Shared ring the `tracing` subscriber writes into; `Ctrl+L` reads it.
    /// Default-constructed for a frontend that installs no subscriber, in
    /// which case the panel simply reports that it is empty.
    pub logs: LogBuffer,
}

/// Prompts kept in the recall ring. Generous — an entry is a short string,
/// and the cost of forgetting the one you wanted is another round of typing.
pub const HISTORY_LIMIT: usize = 200;

/// Title of the `Ctrl+L` panel. A constant because the key toggles on it:
/// pressing `Ctrl+L` with `/usage` open should replace it, not close it.
pub const LOG_PANEL_TITLE: &str = "diagnostics";

/// One table row from string-ish cells.
fn row<const N: usize>(cells: [&str; N]) -> Vec<String> {
    cells.iter().map(|c| c.to_string()).collect()
}

/// A read-only panel over the transcript: `/usage`, `/mcp`, the `Ctrl+L` log.
///
/// **Deliberately not a `Modal` variant.** `Modal` is a sum type precisely
/// because each of its variants owns a `oneshot` the agent loop is blocked on;
/// making this a fourth variant would mean `/usage` could replace a pending
/// permission prompt and strand the turn forever. An overlay owns nothing and
/// answers nobody, so it lives in its own field and yields to any real modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub title: String,
    pub body: OverlayBody,
    /// Footer rows, rendered under the body and never scrolled off — the key
    /// hints belong to the panel, not to its contents.
    pub footer: Vec<String>,
    pub scroll: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayBody {
    /// Column headers and rows. Widths are percentages of the inner width and
    /// are expected to sum to 100.
    Table {
        columns: Vec<String>,
        widths: Vec<u16>,
        rows: Vec<Vec<String>>,
    },
    /// Pre-formatted lines, for content that isn't a grid.
    Lines(Vec<String>),
}

impl Overlay {
    pub fn table(
        title: impl Into<String>,
        columns: &[&str],
        widths: &[u16],
        rows: Vec<Vec<String>>,
    ) -> Self {
        Self {
            title: title.into(),
            body: OverlayBody::Table {
                columns: columns.iter().map(|c| c.to_string()).collect(),
                widths: widths.to_vec(),
                rows,
            },
            footer: Vec::new(),
            scroll: 0,
        }
    }

    pub fn lines(title: impl Into<String>, lines: Vec<String>) -> Self {
        Self {
            title: title.into(),
            body: OverlayBody::Lines(lines),
            footer: Vec::new(),
            scroll: 0,
        }
    }

    pub fn with_footer(mut self, footer: Vec<String>) -> Self {
        self.footer = footer;
        self
    }

    /// Rows the body wants, so the panel can size itself and clamp scrolling.
    pub fn row_count(&self) -> usize {
        match &self.body {
            // +1 for the header row.
            OverlayBody::Table { rows, .. } => rows.len() + 1,
            OverlayBody::Lines(lines) => lines.len(),
        }
    }
}

/// Which section of the sidebar is on screen.
///
/// The sidebar used to stack every section at once, which is what made it the
/// first thing to overflow at 80x24: `SESSION`, the task checklist, `CONTEXT`
/// and the vitals together want more rows than a 24-row terminal has left
/// after the prompt and the status bar. Tabs trade "see everything, truncated"
/// for "see one thing, whole" — the right trade for a pane 28 columns wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarTab {
    #[default]
    Session,
    Tasks,
    Vitals,
}

impl SidebarTab {
    /// Tab order, which is also the `Tabs` widget's index order.
    pub const ALL: [SidebarTab; 3] = [SidebarTab::Session, SidebarTab::Tasks, SidebarTab::Vitals];

    pub fn title(self) -> &'static str {
        match self {
            SidebarTab::Session => "Session",
            SidebarTab::Tasks => "Tasks",
            SidebarTab::Vitals => "Vitals",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
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
    /// Prompts submitted in this project, oldest last — `history[0]` is the
    /// most recent, so "one step back" is a plain index.
    ///
    /// Seeded on startup from the resumed session's own user messages rather
    /// than from a separate history file: the messages are already persisted,
    /// already scoped to this project, and cannot drift out of sync with the
    /// conversation they came from.
    history: Vec<String>,
    /// How far back the user has walked, or `None` while editing their own
    /// text. `history_draft` holds that text so walking back and forward
    /// again returns it rather than eating it.
    history_pos: Option<usize>,
    history_draft: String,
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
    turn_started_at: Option<Instant>,
    /// First assistant text delta of the current provider stream (per round).
    stream_started_at: Option<Instant>,
    /// Characters received in the current stream — for live tok/s estimate.
    stream_output_chars: u32,
    /// Live estimate while streaming (`chars/4 / elapsed`).
    live_tokens_per_sec: Option<f32>,
    /// Last measured rate from provider `output_tokens / elapsed`.
    tokens_per_sec: Option<f32>,
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
            history: config.history,
            history_pos: None,
            history_draft: String::new(),
            queued: std::collections::VecDeque::new(),
            overlay: None,
            logs: config.logs,
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
            turn_started_at: None,
            stream_started_at: None,
            stream_output_chars: 0,
            live_tokens_per_sec: None,
            tokens_per_sec: None,
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
            self.live_tokens_per_sec.or(self.tokens_per_sec)
        } else {
            self.tokens_per_sec
        }
    }

    /// Seconds since the current turn started, for the "thinking… 12s" style
    /// status line — `None` when idle.
    pub fn turn_elapsed_secs(&self) -> Option<f32> {
        self.turn_started_at.map(|t| t.elapsed().as_secs_f32())
    }

    /// Rough output-token estimate for the round currently streaming in
    /// (~4 chars/token), for the same status line — `None` before any text
    /// has arrived this round.
    pub fn live_output_tokens_estimate(&self) -> Option<u32> {
        self.stream_started_at.map(|_| self.stream_output_chars / 4)
    }

    /// Advances the spinner animation; call on a timer while `is_animating()`.
    pub fn tick(&mut self) {
        // Not reduced modulo any frame count: the ASCII and Unicode sets have
        // different lengths, so the wrap belongs at the indexing site.
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
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

    /// Handles a raw key event, returning an Action to forward to the
    /// orchestrator if this keystroke produced one.
    pub fn on_key(
        &mut self,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Action> {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Before the modal branches, which each consume every key they see:
        // with a modal up, `Ctrl+C` used to either type a literal "c" into
        // the question box or do nothing at all, leaving no way out.
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            if self.quit_pending() {
                self.quit_armed_at = None;
                self.should_quit = true;
                return Some(Action::Quit);
            }
            self.quit_armed_at = Some(Instant::now());
            return None;
        }
        // Anything else means the user moved on — quitting has to be two
        // deliberate presses in a row, not any two presses.
        self.quit_armed_at = None;

        if self.modal.is_question() {
            return match code {
                KeyCode::Char('1') => self.submit_question_choice(0),
                KeyCode::Char('2') => self.submit_question_choice(1),
                KeyCode::Char('3') => self.submit_question_choice(2),
                KeyCode::Char('4') => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = 3;
                    }
                    None
                }
                KeyCode::Up => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = m.selected.saturating_sub(1);
                    }
                    None
                }
                KeyCode::Down => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = (m.selected + 1).min(3);
                    }
                    None
                }
                KeyCode::Enter => {
                    let selected = self.modal.question().map(|m| m.selected).unwrap_or(0);
                    if selected == 3 {
                        let custom = self
                            .modal
                            .question()
                            .map(|m| m.custom.trim().to_string())
                            .unwrap_or_default();
                        if custom.is_empty() {
                            None
                        } else {
                            self.record_question_answer(&custom);
                            self.modal = Modal::None;
                            Some(Action::QuestionResponse(custom))
                        }
                    } else {
                        self.submit_question_choice(selected)
                    }
                }
                KeyCode::Esc => {
                    self.record_question_answer("dismissed without answering");
                    self.modal = Modal::None;
                    Some(Action::QuestionResponse(
                        "User dismissed the question without answering.".into(),
                    ))
                }
                KeyCode::Backspace => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = 3;
                        m.custom.pop();
                    }
                    None
                }
                KeyCode::Char(c) if !c.is_control() => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = 3;
                        m.custom.push(c);
                    }
                    None
                }
                _ => None,
            };
        }

        if self.modal.is_plan() {
            return match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.modal = Modal::None;
                    self.plan_gated = false;
                    self.waiting_on_assistant = true;
                    self.phase = AgentPhase::Building;
                    self.in_flight_text = None;

                    self.turn_started_at = Some(Instant::now());
                    self.request_count += 1;
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "plan approved — building…"));
                    Some(Action::ApprovePlan)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.modal = Modal::None;
                    self.plan_gated = false;
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "plan rejected"));
                    Some(Action::RejectPlan)
                }
                KeyCode::Up => {
                    if let Some(m) = self.modal.plan_mut() {
                        m.scroll = m.scroll.saturating_sub(1);
                    }
                    None
                }
                KeyCode::Down => {
                    if let Some(m) = self.modal.plan_mut() {
                        m.scroll = m.scroll.saturating_add(1);
                    }
                    None
                }
                KeyCode::PageUp => {
                    if let Some(m) = self.modal.plan_mut() {
                        m.scroll = m.scroll.saturating_sub(10);
                    }
                    None
                }
                KeyCode::PageDown => {
                    if let Some(m) = self.modal.plan_mut() {
                        m.scroll = m.scroll.saturating_add(10);
                    }
                    None
                }
                _ => None,
            };
        }

        if self.modal.permission().is_some() {
            return match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.modal = Modal::None;
                    Some(Action::PermissionResponse(PermissionDecision::AllowOnce))
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.modal = Modal::None;
                    Some(Action::PermissionResponse(PermissionDecision::AllowSession))
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.modal = Modal::None;
                    Some(Action::PermissionResponse(PermissionDecision::Deny))
                }
                KeyCode::Up => {
                    if let Some(m) = self.modal.permission_mut() {
                        m.scroll = m.scroll.saturating_sub(1);
                    }
                    None
                }
                KeyCode::Down => {
                    if let Some(m) = self.modal.permission_mut() {
                        m.scroll = m.scroll.saturating_add(1);
                    }
                    None
                }
                KeyCode::PageUp => {
                    if let Some(m) = self.modal.permission_mut() {
                        m.scroll = m.scroll.saturating_sub(10);
                    }
                    None
                }
                KeyCode::PageDown => {
                    if let Some(m) = self.modal.permission_mut() {
                        m.scroll = m.scroll.saturating_add(10);
                    }
                    None
                }
                _ => None,
            };
        }

        let bound = self.keys.action_for(code, modifiers);

        if bound == Some(KeyAction::ToggleLogs) {
            self.toggle_log_panel();
            return None;
        }

        // The overlay is a pager, not a prompt: it claims the navigation keys
        // and `Esc`, and *anything else* dismisses it and is then handled
        // normally. A panel that swallowed the first keystroke of the next
        // message would be a panel you have to remember to close.
        if self.overlay.is_some() {
            match code {
                KeyCode::Esc => {
                    self.overlay = None;
                    return None;
                }
                KeyCode::Up => {
                    self.scroll_overlay(-1);
                    return None;
                }
                KeyCode::Down => {
                    self.scroll_overlay(1);
                    return None;
                }
                KeyCode::PageUp => {
                    self.scroll_overlay(-10);
                    return None;
                }
                KeyCode::PageDown => {
                    self.scroll_overlay(10);
                    return None;
                }
                KeyCode::Home => {
                    if let Some(o) = self.overlay.as_mut() {
                        o.scroll = 0;
                    }
                    return None;
                }
                _ => self.overlay = None,
            }
        }

        if bound == Some(KeyAction::ToggleCardFocus) {
            self.toggle_card_focus();
            return None;
        }

        if bound == Some(KeyAction::ToggleSidebar) {
            self.sidebar_visible = !self.sidebar_visible;
            return None;
        }

        // Bound to `Shift+Tab` by default, which arrives as its own key code
        // and so never competes with the `Tab` that accepts a slash
        // completion. Cycling a hidden sidebar would be a keystroke with no
        // visible effect, so it reveals it first.
        if bound == Some(KeyAction::CycleSidebarTab) {
            if self.sidebar_visible {
                self.sidebar_tab = self.sidebar_tab.next();
            } else {
                self.sidebar_visible = true;
            }
            return None;
        }

        // Card focus claims exactly four keys and leaves typing alone, so the
        // prompt stays usable — but `Enter` belongs to the card while one is
        // selected, which is why `Ctrl+O`/`Esc` both have to release it.
        if self.selected_card().is_some() {
            match code {
                KeyCode::Up => {
                    self.move_card_focus(false);
                    return None;
                }
                KeyCode::Down => {
                    self.move_card_focus(true);
                    return None;
                }
                KeyCode::Enter => {
                    self.toggle_selected_card();
                    return None;
                }
                // A running turn keeps `Esc` for cancelling — that is the more
                // urgent meaning, and `Ctrl+O` still releases focus.
                KeyCode::Esc if !self.waiting_on_assistant => {
                    self.select_card(None);
                    return None;
                }
                _ => {}
            }
        }

        let hints = self.suggestions();
        let slash_nav = !hints.is_empty();
        let completing_file = self.completion_kind == CompletionKind::File;

        match code {
            KeyCode::Esc if self.waiting_on_assistant => Some(Action::CancelGeneration),
            KeyCode::Tab if slash_nav && completing_file => {
                if let Some(pick) = hints.get(self.slash_selected) {
                    let replaced = complete::accept_file(&self.input.text(), &pick.name);
                    self.input.set(&replaced);
                    self.slash_selected = 0;
                }
                None
            }
            KeyCode::Tab if slash_nav => {
                if let Some(completed) = self
                    .commands
                    .complete(&self.input.text(), Some(self.slash_selected))
                {
                    self.input.set(&completed);
                    self.slash_selected = 0;
                }
                None
            }
            KeyCode::Up if slash_nav => {
                self.slash_selected = self.slash_selected.saturating_sub(1);
                None
            }
            KeyCode::Down if slash_nav => {
                let max = hints.len().saturating_sub(1);
                self.slash_selected = (self.slash_selected + 1).min(max);
                None
            }
            // A newline in the prompt, not a submission. `Alt+Enter` and
            // `Ctrl+J` work everywhere; `Shift+Enter` only reaches us on
            // terminals that support the kitty keyboard protocol, since
            // otherwise it arrives indistinguishable from a bare Enter.
            KeyCode::Enter
                if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                    && !self.waiting_on_assistant =>
            {
                self.input.insert_newline();
                None
            }
            _ if bound == Some(KeyAction::InsertNewline) && !self.waiting_on_assistant => {
                self.input.insert_newline();
                None
            }
            // Arrows walk the caret through a multi-line prompt first; they
            // only reach the history once the caret is already at the top or
            // bottom of the input — which is always the case for the
            // single-row prompt this used to be.
            KeyCode::Up if self.input.move_up() => None,
            KeyCode::Down if self.input.move_down() => None,
            // Past the edge of the input, the arrows walk the prompt history,
            // the way every shell and REPL binds them. Scrolling the
            // transcript keeps PageUp/PageDown and the mouse wheel: a
            // conversation is read by the page, and a previous prompt is
            // recalled by the line.
            KeyCode::Up if self.history_back() => None,
            KeyCode::Down if self.history_forward() => None,
            // Nothing to recall — fall back to the old meaning rather than
            // making the key inert.
            KeyCode::Up => {
                self.follow_bottom = false;
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            KeyCode::PageUp => {
                self.follow_bottom = false;
                self.scroll = self.scroll.saturating_sub(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                None
            }
            KeyCode::Enter => {
                if self.input.text().trim().is_empty() {
                    return None;
                }
                // Mid-turn, a plain message is queued rather than refused.
                // A slash command still runs now: they are how you *steer* a
                // running turn (`/queue clear`, `/plan approve`), and queueing
                // one would be the opposite of what it is for. The ones that
                // genuinely cannot run mid-turn already say so themselves.
                if self.waiting_on_assistant && !self.input.text().starts_with('/') {
                    let text = self.input.take();
                    self.slash_selected = 0;
                    self.remember_prompt(&text);
                    // Held here *and* sent. The queue is the record of what
                    // has not landed yet: the agent folds an interjection in
                    // at its next round boundary, and a turn already on its
                    // last round has no next boundary — so anything still
                    // waiting when the turn ends is sent as its own turn
                    // instead. Without the fallback, speaking to a turn that
                    // was about to finish would drop the message silently.
                    self.queued.push_back(text.clone());
                    return Some(Action::Interject(text));
                }
                // A highlighted path is accepted rather than submitted — the
                // list is on screen precisely because the caret is mid-token,
                // so Enter means "that one", the way Tab does.
                if slash_nav && completing_file {
                    if let Some(pick) = hints.get(self.slash_selected) {
                        let replaced = complete::accept_file(&self.input.text(), &pick.name);
                        self.input.set(&replaced);
                        self.slash_selected = 0;
                        return None;
                    }
                }
                // If a slash suggestion is highlighted and input is still only
                // a partial command, Tab-complete first instead of submitting.
                if slash_nav && !self.input.text().contains(char::is_whitespace) {
                    if let Some(completed) = self
                        .commands
                        .complete(&self.input.text(), Some(self.slash_selected))
                    {
                        self.input.set(&completed);
                        self.slash_selected = 0;
                        return None;
                    }
                }
                let text = self.input.take();
                self.slash_selected = 0;
                // Slash commands go in too: `/model gpt-4.1` is exactly the
                // kind of thing you want back with one keypress.
                self.remember_prompt(&text);

                if let Some(command) = text.strip_prefix('/') {
                    return self.run_slash_command(command.trim());
                }

                self.lines.push(ChatLine::new(ChatRole::User, text.clone()));
                self.waiting_on_assistant = true;
                self.phase = AgentPhase::Thinking;
                self.in_flight_text = None;
                self.turn_started_at = Some(Instant::now());
                self.stream_started_at = None;
                self.stream_output_chars = 0;
                self.live_tokens_per_sec = None;
                self.request_count += 1;
                Some(Action::SubmitMessage(text))
            }
            // Everything else the transcript didn't claim is text editing:
            // typing, Backspace/Delete, caret movement, word jumps,
            // Ctrl+A/E/W/U. The editor reports whether it consumed the key.
            other => {
                if self.input.handle(other, modifiers) {
                    self.slash_selected = 0;
                }
                None
            }
        }
    }

    fn submit_question_choice(&mut self, index: usize) -> Option<Action> {
        let answer = self
            .modal
            .question()
            .and_then(|m| m.question.options.get(index).cloned())?;
        self.record_question_answer(&answer);
        self.modal = Modal::None;
        Some(Action::QuestionResponse(answer))
    }

    /// Leaves a permanent record of an `ask_user` exchange in the transcript
    /// before the modal (which is otherwise the only place it's visible)
    /// closes.
    fn record_question_answer(&mut self, answer: &str) {
        if let Some(modal) = self.modal.question() {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("asked: {} — answered: {answer}", modal.question.prompt),
            ));
        }
    }

    /// Dispatches `/name args`.
    ///
    /// Built-ins are matched first and custom commands only reach the fallback
    /// arm, so a file in a cloned repository cannot change what `/clear` does
    /// however the registry was built. See `crate::slash`.
    pub(crate) fn run_slash_command(&mut self, command: &str) -> Option<Action> {
        let mut parts = command.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim();

        match name {
            "clear" => {
                self.lines.clear();
                None
            }
            "help" | "" => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    "commands: /clear (clear the visible transcript), /model [<name>|<provider>/<name>] [--save] (show or switch model), /permission [ask|session|skip] [--save] (show or set the tool permission policy), /usage (session token/cost/tool-call summary), /plan <task>|approve|reject (plan before executing), /goal [<description>|clear] (set, show, or clear the session goal), /loop [<N>] <task>|goal (repeat a task until done, N iterations, or Esc), /compact (summarise old history to reclaim context), /remember <note> (append a standing note to this project's SMITH.md), /mcp [prompt [<server>] <name> [key=value ...]] (list MCP servers, or run one's prompt template),/rewind [<turn>] [confirm] [--force] (undo a turn's file writes — shows the plan first; does NOT undo anything run_bash did), /help (this message)",
                ));
                self.show_custom_commands();
                None
            }
            "model" => self.run_model_command(args),
            "permission" => self.run_permission_command(args),
            "usage" => {
                self.show_usage();
                None
            }
            "mcp" => self.run_mcp_command(args),
            "queue" => self.run_queue_command(args),
            "plan" => self.run_plan_command(args),
            "rewind" => self.run_rewind_command(args),
            "goal" => self.run_goal_command(args),
            "loop" => self.run_loop_command(args),
            "compact" => {
                if self.waiting_on_assistant {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "can't compact mid-turn — wait for the current turn to finish",
                    ));
                    return None;
                }
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    "compacting the conversation…",
                ));
                self.waiting_on_assistant = true;
                self.phase = AgentPhase::Thinking;
                Some(Action::Compact)
            }
            "remember" => {
                let note = args.trim();
                if note.is_empty() {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "usage: /remember <note> — appends a standing instruction to this project's SMITH.md",
                    ));
                    return None;
                }
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("remembered: {note}"),
                ));
                Some(Action::Remember(note.to_string()))
            }
            other => self.run_custom_command(other, args),
        }
    }

    /// Lists custom commands under `/help`, with the file each came from.
    ///
    /// The path is not decoration: `/deploy` doing something surprising is a
    /// question about *which file* defines it, and a user who cloned the repo
    /// has no other way to find out.
    fn show_custom_commands(&mut self) {
        let custom = self.commands.custom();
        if custom.is_empty() {
            return;
        }
        let listed: Vec<String> = custom
            .commands()
            .iter()
            .map(|c| format!("/{} ({}) — {}", c.name, c.description, c.source.display()))
            .collect();
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("custom commands: {}", listed.join(", ")),
        ));
    }

    /// A command loaded from `.smith/commands/` or `~/.smith/commands/`.
    ///
    /// The expansion is submitted as an ordinary user message — there is no
    /// new `Action` and no capability a custom command has that typing the
    /// same prose would not.
    ///
    /// **The expanded body goes into the transcript, not the `/name`.** That
    /// is the one thing that makes a prompt from a file safe to run: the user
    /// sees exactly what was sent, in the same breath as it is sent, so a
    /// command that is not what they expected is visible rather than inferred.
    /// A system line above it names the file, so a project command is
    /// attributable at a glance.
    fn run_custom_command(&mut self, name: &str, args: &str) -> Option<Action> {
        // Lowercased because command names are normalised at load time; the
        // user typing `/Deploy` should reach the same file `/deploy` does.
        let lowered = name.to_ascii_lowercase();
        let Some(command) = self.commands.custom().get(&lowered) else {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("unknown command: /{name}"),
            ));
            return None;
        };

        let source = command.source.display().to_string();
        let prompt = match command.render(args) {
            Ok(prompt) => prompt,
            // A missing `$1` refuses rather than expanding to nothing — see
            // `CustomCommand::render`. The message names the placeholders.
            Err(problem) => {
                self.lines.push(ChatLine::new(ChatRole::System, problem));
                return None;
            }
        };
        if self.waiting_on_assistant {
            return None;
        }

        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("/{lowered} — from {source}"),
        ));
        self.lines
            .push(ChatLine::new(ChatRole::User, prompt.clone()));
        self.waiting_on_assistant = true;
        self.phase = AgentPhase::Thinking;
        self.in_flight_text = None;
        self.turn_started_at = Some(Instant::now());
        self.stream_started_at = None;
        self.stream_output_chars = 0;
        self.live_tokens_per_sec = None;
        self.request_count += 1;
        Some(Action::SubmitMessage(prompt))
    }

    /// `/mcp` — connected servers — and `/mcp prompt [<server>] <name>
    /// [key=value ...]`, which runs a prompt template one of them supplies.
    ///
    /// The subcommand lives here rather than as its own `/`-command because a
    /// server-supplied prompt is not smith's own command: keeping it behind
    /// `/mcp` says where it came from every time it is typed, and leaves the
    /// top-level namespace to the frontend that owns it.
    fn run_mcp_command(&mut self, args: &str) -> Option<Action> {
        let mut tokens = args.split_whitespace();
        match tokens.next() {
            None => Some(Action::Mcp(McpCommand::Status)),
            Some("prompt") => {
                let mut positional: Vec<&str> = Vec::new();
                let mut arguments: Vec<(String, String)> = Vec::new();
                for token in tokens {
                    match token.split_once('=') {
                        Some((k, v)) => arguments.push((k.to_string(), v.to_string())),
                        None => positional.push(token),
                    }
                }
                // One bare word is a prompt name; two are a server and a
                // prompt name. Guessing between them is only ambiguous if a
                // prompt is named after a server, and then the two-word form
                // is the way to say which you meant.
                let (server, name) = match positional.as_slice() {
                    [name] => (None, name.to_string()),
                    [server, name] => (Some(server.to_string()), name.to_string()),
                    _ => {
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            "usage: /mcp prompt [<server>] <name> [key=value ...]",
                        ));
                        return None;
                    }
                };
                if self.waiting_on_assistant {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "can't run a prompt mid-turn — wait for the current turn to finish",
                    ));
                    return None;
                }
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    match &server {
                        Some(s) => format!("running MCP prompt `{name}` from `{s}`…"),
                        None => format!("running MCP prompt `{name}`…"),
                    },
                ));
                self.waiting_on_assistant = true;
                self.phase = AgentPhase::Thinking;
                self.in_flight_text = None;
                self.turn_started_at = Some(Instant::now());
                self.request_count += 1;
                Some(Action::Mcp(McpCommand::Prompt {
                    server,
                    name,
                    arguments,
                }))
            }
            Some(other) => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!(
                        "unknown /mcp subcommand: {other} — try /mcp, or \
                         /mcp prompt [<server>] <name> [key=value ...]"
                    ),
                ));
                None
            }
        }
    }

    /// `/rewind [<turn>] [confirm] [--force]`.
    ///
    /// Two steps on purpose. Restoring files overwrites whatever is on disk
    /// now, which is the one thing in smith that can destroy work the user did
    /// themselves — so the bare command only ever *describes* what it would
    /// do, and `confirm` is a separate keystroke the user makes after reading
    /// it.
    fn run_rewind_command(&mut self, args: &str) -> Option<Action> {
        let mut turn = None;
        let mut apply = false;
        let mut force = false;
        for token in args.split_whitespace() {
            match token {
                "confirm" | "--confirm" => apply = true,
                "--force" | "-f" => force = true,
                other => match other.parse::<u64>() {
                    Ok(n) => turn = Some(n),
                    Err(_) => {
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            "usage: /rewind [<turn>] [confirm] [--force] — with no `confirm` it \
                             only shows what it would restore",
                        ));
                        return None;
                    }
                },
            }
        }

        // The checkpoint of a turn still in flight is incomplete, and undoing
        // half of it would be worse than not offering to.
        if self.waiting_on_assistant {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "can't rewind mid-turn — press Esc to stop the current turn first",
            ));
            return None;
        }

        Some(Action::Rewind { turn, apply, force })
    }

    fn run_model_command(&mut self, args: &str) -> Option<Action> {
        if args.is_empty() || args.eq_ignore_ascii_case("list") {
            self.show_model_info();
            return None;
        }

        let mut save = false;
        let mut spec_tokens = Vec::new();
        for token in args.split_whitespace() {
            if token == "--save" {
                save = true;
            } else {
                spec_tokens.push(token);
            }
        }

        let Some(spec) = spec_tokens.first() else {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "usage: /model <name> [--save]  or  /model <provider>/<name> [--save]",
            ));
            return None;
        };

        let (provider, model) = match spec.split_once('/') {
            Some((p, m)) => {
                if !smith_store::is_known_provider(p) {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        format!("unknown provider: {p} (expected anthropic, openai, or ollama)"),
                    ));
                    return None;
                }
                (Some(p.to_string()), m.to_string())
            }
            None => (None, spec.to_string()),
        };

        if model.is_empty() {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "usage: /model <name> [--save]  or  /model <provider>/<name> [--save]",
            ));
            return None;
        }

        Some(Action::SwitchModel {
            provider,
            model,
            save,
        })
    }

    fn show_model_info(&mut self) {
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("current: {}/{}", self.provider_label, self.model_label),
        ));
        let known = smith_store::known_models(&self.provider_label);
        if !known.is_empty() {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("known {} models: {}", self.provider_label, known.join(", ")),
            ));
        }
        self.lines.push(ChatLine::new(
            ChatRole::System,
            "switch with /model <name>, or /model <provider>/<name>; add --save to persist",
        ));
    }

    fn run_permission_command(&mut self, args: &str) -> Option<Action> {
        if args.is_empty() {
            self.show_permission_info();
            return None;
        }

        let mut save = false;
        let mut mode_tokens = Vec::new();
        for token in args.split_whitespace() {
            if token == "--save" {
                save = true;
            } else {
                mode_tokens.push(token);
            }
        }

        let Some(mode) = mode_tokens.first() else {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "usage: /permission <ask|session|skip> [--save]",
            ));
            return None;
        };

        let Some(policy) = PermissionPolicy::parse(mode) else {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("unknown mode: {mode} (expected ask, session, or skip)"),
            ));
            return None;
        };

        if policy == PermissionPolicy::Skip {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "⚠ skip mode auto-allows every tool call, including shell commands, with no confirmation of any kind.",
            ));
        }

        Some(Action::SetPermissionPolicy { policy, save })
    }

    fn show_permission_info(&mut self) {
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("current: {}", self.permission_policy.as_str()),
        ));
        self.lines.push(ChatLine::new(
            ChatRole::System,
            "modes: ask (always prompt, default), session (auto-allow file writes/edits, still prompts for shell/MCP tools), skip/yolo (auto-allow everything, no prompts)",
        ));
        self.lines.push(ChatLine::new(
            ChatRole::System,
            "switch with /permission <mode>; add --save to persist",
        ));
    }

    /// `/usage` — the session's accounting, as a table.
    ///
    /// The cost row is whatever the agent last reported and is never derived
    /// from `self.usage`: those tokens are a running total across the whole
    /// session, while the cost is a sum of per-turn figures priced when each
    /// turn ran. Multiplying today's price by the lifetime token count is the
    /// bug acceptance criterion #4 exists to catch.
    fn show_usage(&mut self) {
        let total_tokens = self.usage.input_tokens + self.usage.output_tokens;
        let cost = match self.session_cost {
            Some((usd, _)) => format!("~${usd:.4}"),
            None => "n/a".to_string(),
        };

        let mut rows = vec![
            row(["requests", &self.request_count.to_string()]),
            row(["tool calls", &self.tool_call_count.to_string()]),
            row(["input tokens", &self.usage.input_tokens.to_string()]),
            row(["output tokens", &self.usage.output_tokens.to_string()]),
            row(["total tokens", &total_tokens.to_string()]),
            row(["cost (est.)", &cost]),
        ];
        if self.usage.cache_read > 0 || self.usage.cache_write > 0 {
            rows.insert(4, row(["cache read", &self.usage.cache_read.to_string()]));
            rows.insert(5, row(["cache write", &self.usage.cache_write.to_string()]));
        }

        let mut footer = vec![format!("{}/{}", self.provider_label, self.model_label)];
        match self.session_cost {
            Some((_, unpriced)) if unpriced > 0 => footer.push(format!(
                "{unpriced} turn(s) ran on a model with no known price and are not in the total"
            )),
            None => footer.push(format!(
                "no pricing data for {}/{}",
                self.provider_label, self.model_label
            )),
            _ => {}
        }

        self.overlay = Some(
            Overlay::table("session usage", &["metric", "value"], &[60, 40], rows)
                .with_footer(footer),
        );
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
        self.turn_started_at = Some(Instant::now());
        self.stream_started_at = None;
        self.stream_output_chars = 0;
        self.live_tokens_per_sec = None;
        self.request_count += 1;
        Some(Action::SubmitMessage(text))
    }

    /// `/queue` — show what is waiting; `clear` empties it, `drop` removes the
    /// most recent entry.
    fn run_queue_command(&mut self, args: &str) -> Option<Action> {
        match args.trim() {
            "" => {
                if self.queued.is_empty() {
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "nothing queued"));
                    return None;
                }
                let listed: Vec<String> = self
                    .queued
                    .iter()
                    .enumerate()
                    .map(|(i, q)| format!("{}. {q}", i + 1))
                    .collect();
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("queued ({}):\n{}", self.queued.len(), listed.join("\n")),
                ));
                None
            }
            "clear" => {
                let count = self.queued.len();
                self.queued.clear();
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    match count {
                        0 => "nothing queued".to_string(),
                        1 => "dropped the queued message".to_string(),
                        n => format!("dropped {n} queued messages"),
                    },
                ));
                None
            }
            "drop" => {
                match self.queued.pop_back() {
                    // Echoed back so "which one did I just lose" is answered
                    // without having to remember.
                    Some(text) => self
                        .lines
                        .push(ChatLine::new(ChatRole::System, format!("dropped: {text}"))),
                    None => self
                        .lines
                        .push(ChatLine::new(ChatRole::System, "nothing queued")),
                }
                None
            }
            other => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("unknown /queue subcommand: {other} — try /queue, /queue clear, or /queue drop"),
                ));
                None
            }
        }
    }

    /// `Ctrl+L` — open the diagnostics panel, or close it if it is already up.
    fn toggle_log_panel(&mut self) {
        if self
            .overlay
            .as_ref()
            .is_some_and(|o| o.title == LOG_PANEL_TITLE)
        {
            self.overlay = None;
            return;
        }

        let lines: Vec<String> = self
            .logs
            .snapshot()
            .into_iter()
            .map(|l| format!("{:<5} {} — {}", l.level.label(), l.target, l.message))
            .collect();
        let empty = lines.is_empty();
        let body = if empty {
            vec!["nothing logged yet".to_string()]
        } else {
            lines
        };
        // Opened at the bottom: the interesting line in a log is the last one.
        let mut overlay = Overlay::lines(LOG_PANEL_TITLE, body).with_footer(vec![
            "Esc closes  ·  up/down and PgUp/PgDn scroll".to_string(),
        ]);
        overlay.scroll = u16::MAX;
        self.overlay = Some(overlay);
    }

    /// Rows a wheel notch moves. Three is the terminal convention and what
    /// every other pager in the user's shell already does.
    const WHEEL_ROWS: u16 = 3;

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
        let next = match self.history_pos {
            None => 0,
            Some(i) => i + 1,
        };
        if next >= self.history.len() {
            return false;
        }
        if self.history_pos.is_none() {
            // Set aside whatever was half-typed, so walking forward again
            // brings it back instead of losing it.
            self.history_draft = self.input.text();
        }
        self.history_pos = Some(next);
        let entry = self.history[next].clone();
        self.input.set(&entry);
        true
    }

    /// Step one entry forward; past the newest, restore the saved draft.
    fn history_forward(&mut self) -> bool {
        let Some(i) = self.history_pos else {
            return false;
        };
        if i == 0 {
            self.history_pos = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.input.set(&draft);
        } else {
            self.history_pos = Some(i - 1);
            let entry = self.history[i - 1].clone();
            self.input.set(&entry);
        }
        true
    }

    /// Record a submitted prompt and leave history-walking mode.
    ///
    /// Consecutive duplicates collapse — holding Enter on the same message,
    /// or resubmitting a recalled one, should not make Up press twice to get
    /// past it.
    fn remember_prompt(&mut self, text: &str) {
        self.history_pos = None;
        self.history_draft.clear();
        if text.trim().is_empty() {
            return;
        }
        if self.history.first().is_some_and(|h| h == text) {
            return;
        }
        self.history.insert(0, text.to_string());
        self.history.truncate(HISTORY_LIMIT);
    }

    /// Move the open modal by `delta` rows, if it has a scroll offset.
    /// Clamping to the end is the renderer's job, as it is for the overlay.
    fn scroll_modal(&mut self, delta: i32) {
        let scroll = match &mut self.modal {
            Modal::Permission(m) => &mut m.scroll,
            Modal::Plan(m) => &mut m.scroll,
            Modal::Question(_) | Modal::None => return,
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

    fn run_plan_command(&mut self, args: &str) -> Option<Action> {
        match args {
            "" => {
                let status = if self.plan_gated {
                    "awaiting approval — run /plan approve or /plan reject"
                } else {
                    "no plan pending"
                };
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("plan status: {status}"),
                ));
                None
            }
            "approve" => {
                if !self.plan_gated && !self.modal.is_plan() {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "no plan pending to approve",
                    ));
                    return None;
                }
                self.modal = Modal::None;
                self.plan_gated = false;
                self.waiting_on_assistant = true;
                self.phase = AgentPhase::Building;
                self.in_flight_text = None;
                self.turn_started_at = Some(Instant::now());
                self.request_count += 1;
                self.lines
                    .push(ChatLine::new(ChatRole::System, "plan approved — building…"));
                Some(Action::ApprovePlan)
            }
            "reject" => {
                if !self.plan_gated && !self.modal.is_plan() {
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "no plan pending to reject"));
                    return None;
                }
                self.modal = Modal::None;
                self.plan_gated = false;
                self.lines
                    .push(ChatLine::new(ChatRole::System, "plan rejected"));
                Some(Action::RejectPlan)
            }
            description => {
                if self.waiting_on_assistant {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "still working on the previous request — wait for it to finish first",
                    ));
                    return None;
                }
                self.lines.push(ChatLine::new(
                    ChatRole::User,
                    format!("[plan] {description}"),
                ));
                self.waiting_on_assistant = true;
                self.plan_turn_active = true;
                self.phase = AgentPhase::Planning;
                self.in_flight_text = None;
                self.turn_started_at = Some(Instant::now());
                self.request_count += 1;
                Some(Action::StartPlan(description.to_string()))
            }
        }
    }

    fn run_goal_command(&mut self, args: &str) -> Option<Action> {
        match args {
            "" => {
                match &self.goal {
                    Some(goal) => self.lines.push(ChatLine::new(
                        ChatRole::System,
                        format!("current goal: {goal}"),
                    )),
                    None => self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "no goal set — /goal <description> to set one, /goal clear to remove",
                    )),
                }
                None
            }
            "clear" => {
                if self.goal.is_none() {
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "no goal set"));
                    return None;
                }
                Some(Action::SetGoal(None))
            }
            description => Some(Action::SetGoal(Some(description.to_string()))),
        }
    }

    fn run_loop_command(&mut self, args: &str) -> Option<Action> {
        if args.is_empty() {
            let status = match (self.loop_active, self.loop_progress) {
                (true, Some((i, m))) => format!("loop running — iteration {i}/{m} (Esc to cancel)"),
                (true, None) => "loop starting…".to_string(),
                (false, _) => "no loop running — /loop [<N>] <task>|goal to start one".to_string(),
            };
            self.lines.push(ChatLine::new(
                ChatRole::System,
                format!("loop status: {status}"),
            ));
            return None;
        }

        if self.waiting_on_assistant {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "still working on the previous request — wait for it to finish first",
            ));
            return None;
        }

        let mut tokens = args.splitn(2, char::is_whitespace);
        let first = tokens.next().unwrap_or("");
        let (max_iterations, rest) = match first.parse::<u32>() {
            Ok(n) => (Some(n), tokens.next().unwrap_or("").trim()),
            Err(_) => (None, args),
        };

        if max_iterations == Some(0) {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "iteration count must be at least 1",
            ));
            return None;
        }

        let prompt = if rest.eq_ignore_ascii_case("goal") {
            match &self.goal {
                Some(goal) => goal.clone(),
                None => {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        "no goal set — /goal <description> first, or give /loop an explicit task",
                    ));
                    return None;
                }
            }
        } else if rest.is_empty() {
            self.lines.push(ChatLine::new(
                ChatRole::System,
                "usage: /loop [<N>] <task>|goal",
            ));
            return None;
        } else {
            rest.to_string()
        };

        self.lines
            .push(ChatLine::new(ChatRole::User, format!("[loop] {prompt}")));
        self.waiting_on_assistant = true;
        self.loop_active = true;
        self.loop_progress = None;
        self.phase = AgentPhase::Looping;
        self.in_flight_text = None;
        self.turn_started_at = Some(Instant::now());
        self.request_count += 1;
        Some(Action::StartLoop {
            prompt,
            max_iterations,
        })
    }

    pub fn on_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AssistantTextDelta(delta) => {
                // First delta of a stream — end any in-flight thinking gap.
                if self.stream_started_at.is_none() {
                    self.end_thinking();
                    self.stream_started_at = Some(Instant::now());
                    self.stream_output_chars = 0;
                }
                self.stream_output_chars = self
                    .stream_output_chars
                    .saturating_add(delta.chars().count() as u32);
                if let Some(started) = self.stream_started_at {
                    let elapsed = started.elapsed().as_secs_f32().max(0.05);
                    // Providers rarely stream mid-turn usage; ~4 chars/token is a
                    // rough live estimate until TokenUsage arrives.
                    let est_tokens = self.stream_output_chars as f32 / 4.0;
                    self.live_tokens_per_sec = Some(est_tokens / elapsed);
                }
                self.in_flight_text
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            AgentEvent::AssistantTurnComplete {
                message,
                stop_reason,
            } => {
                self.in_flight_text = None;
                let is_final = stop_reason != StopReason::ToolUse;
                let text = message.text();

                if !text.is_empty() {
                    let meta = if is_final {
                        self.turn_started_at.map(|t| {
                            let secs = t.elapsed().as_secs_f32();
                            match self.tokens_per_sec {
                                Some(rate) => format!(
                                    "{} · {} · {:.1}s · {:.0} tok/s",
                                    self.provider_label, self.model_label, secs, rate
                                ),
                                None => format!(
                                    "{} · {} · {:.1}s",
                                    self.provider_label, self.model_label, secs
                                ),
                            }
                        })
                    } else {
                        None
                    };
                    let plan_text = text.clone();
                    self.lines
                        .push(ChatLine::new(ChatRole::Assistant, text.clone()).with_meta(meta));

                    if is_final && self.plan_turn_active {
                        self.plan_turn_active = false;
                        self.phase = AgentPhase::Planning;
                        self.modal = Modal::Plan(PlanModal {
                            text: plan_text,
                            scroll: 0,
                        });
                    } else if is_final
                        && matches!(self.phase, AgentPhase::Building)
                        && looks_like_approval_request(&text)
                    {
                        self.lines.push(ChatLine::new(
                            ChatRole::System,
                            "hint: the plan is already approved — call tools to implement it (do not ask for approval in chat)",
                        ));
                    }
                } else if is_final {
                    // The model replied with neither text nor a tool call —
                    // without this, the turn just silently drops back to
                    // idle and it looks like the app hung.
                    self.plan_turn_active = false;
                    let hint = if self.plan_gated {
                        "model returned no output for this turn — plan mode is still active; run /plan reject to exit, or try again"
                    } else {
                        "model returned no output for this turn"
                    };
                    self.lines.push(ChatLine::new(ChatRole::System, hint));
                }

                if is_final {
                    self.stream_started_at = None;
                    self.stream_output_chars = 0;
                    self.live_tokens_per_sec = None;
                    if self.loop_active {
                        // Stay busy across iterations — LoopFinished resets
                        // waiting_on_assistant/phase once the whole run ends,
                        // so Esc keeps working in the gap between rounds.
                    } else {
                        self.waiting_on_assistant = false;
                        self.turn_started_at = None;
                        if self.modal.is_none() {
                            self.phase = AgentPhase::Idle;
                        }
                    }
                } else {
                    // Next provider round starts a fresh stream clock.
                    self.stream_started_at = None;
                    self.stream_output_chars = 0;
                    self.live_tokens_per_sec = None;
                    // Model is about to call tools — start thinking timer.
                    self.begin_thinking();
                }
            }
            AgentEvent::ToolCallStarted {
                id,
                tool_name,
                input,
            } => {
                let label = activity_label(&tool_name, &input);
                // ask_user gets its own modal + transcript record (see
                // record_question_answer); write_tasks gets the dedicated
                // checklist panel — neither needs the generic tool-call line.
                if tool_name != "ask_user" && tool_name != "write_tasks" {
                    self.phase = AgentPhase::Working;
                    // Consecutive calls of a status-only tool fold into the
                    // card above rather than stacking. Only when it is the
                    // *last* line: a search after a reply is a new activity,
                    // and joining it to a card further up would reorder the
                    // transcript.
                    match self.lines.last_mut() {
                        Some(last) if last.can_group(&tool_name) => {
                            // The child's row carries only its target: the
                            // header above it already says the activity, and
                            // repeating "Searching the web…" per row is the
                            // noise this exists to remove.
                            last.group(id.clone(), group_target(&tool_name, &input));
                        }
                        _ => {
                            // Permanent transcript record — the tool card
                            // replaces the old activity strip, so this line is
                            // all we need.
                            self.lines.push(ChatLine::tool(
                                id.clone(),
                                tool_name.clone(),
                                label,
                                input,
                            ));
                        }
                    }
                }
                // End any in-flight thinking gap — the model is acting now.
                self.end_thinking();
                self.tool_call_count += 1;
            }
            AgentEvent::ToolCallResult {
                id,
                output,
                is_error,
            } => {
                let status = if is_error {
                    ActivityStatus::Error
                } else {
                    ActivityStatus::Done
                };
                if let Some(line) = self
                    .lines
                    .iter_mut()
                    .find(|l| l.tool_id() == Some(id.as_str()))
                {
                    line.finish_tool(status, output.clone());
                } else {
                    // Not a card of its own: it was folded into one, so the id
                    // belongs to a child. Searched second because the common
                    // case is the first branch.
                    for line in self.lines.iter_mut() {
                        if line.finish_grouped(&id, status) {
                            break;
                        }
                    }
                }
                // Model starts thinking again after a result — but only once
                // the *last* result of a concurrent round has landed. A round
                // of ReadOnly calls runs in parallel and finishes out of
                // order, so starting the clock on the first one home would
                // report the slowest call's remaining runtime as time the
                // model spent thinking.
                if !self.lines.iter().any(|l| l.is_running_tool()) {
                    self.begin_thinking();
                }
            }
            // A long `run_bash` is otherwise indistinguishable from a hang:
            // the card sat on its spinner for the whole build with nothing to
            // show. Only the newest line is kept — see `set_progress`.
            AgentEvent::ToolProgress { id, line } => {
                if let Some(entry) = self
                    .lines
                    .iter_mut()
                    .rev()
                    .find(|l| l.tool_id() == Some(id.as_str()))
                {
                    entry.set_progress(line);
                }
            }
            AgentEvent::PermissionPromptNeeded(request) => {
                self.phase = AgentPhase::WaitingPermission;
                self.modal = Modal::Permission(PermissionModal { request, scroll: 0 });
            }
            AgentEvent::UserQuestionNeeded(question) => {
                self.phase = AgentPhase::Asking;
                self.modal = Modal::Question(QuestionModal {
                    question,
                    selected: 0,
                    custom: String::new(),
                });
            }
            AgentEvent::PhaseChanged(phase) => {
                // Don't clobber Asking/WaitingPermission while a modal is open.
                if self.modal.is_question() && phase != AgentPhase::Asking {
                    // keep Asking
                } else if self.modal.permission().is_some()
                    && phase != AgentPhase::WaitingPermission
                {
                    // keep WaitingPermission
                } else if self.modal.is_plan() && phase == AgentPhase::Idle {
                    self.phase = AgentPhase::Planning;
                } else if matches!(self.phase, AgentPhase::Building)
                    && matches!(phase, AgentPhase::Thinking)
                {
                    // Keep "building…" chrome during the post-approve model round.
                } else if matches!(self.phase, AgentPhase::Planning)
                    && matches!(phase, AgentPhase::Thinking)
                    && (self.plan_turn_active || self.plan_gated)
                {
                    // Keep "planning…" during a /plan turn.
                } else if self.loop_active
                    && matches!(phase, AgentPhase::Thinking | AgentPhase::Idle)
                {
                    // Keep "looping…" chrome between/within iterations; Working
                    // (tool calls) and permission/question phases still show
                    // through normally.
                } else {
                    self.phase = phase;
                }
            }
            AgentEvent::TokenUsage(usage) => {
                self.usage.input_tokens += usage.input_tokens;
                self.usage.output_tokens += usage.output_tokens;
                if usage.output_tokens > 0 {
                    let started = self.stream_started_at.or(self.turn_started_at);
                    if let Some(started) = started {
                        let elapsed = started.elapsed().as_secs_f32().max(0.05);
                        self.tokens_per_sec = Some(usage.output_tokens as f32 / elapsed);
                    }
                }
            }
            AgentEvent::ContextUsage {
                used,
                window,
                estimated,
            } => {
                // Kept as the agent reported it, not folded into `usage`:
                // `usage` is the session's cumulative token spend, while this
                // is the occupancy of a single window that compaction resets.
                // Adding one to the other would be meaningless.
                self.context = Some((used, window, estimated));
            }
            AgentEvent::UserInterjected(text) => {
                // It is part of the conversation now, so it becomes a real
                // user bubble and leaves the pending list.
                if let Some(at) = self.queued.iter().position(|q| *q == text) {
                    self.queued.remove(at);
                }
                self.lines.push(ChatLine::new(ChatRole::User, text));
                self.request_count += 1;
            }
            AgentEvent::SessionCost {
                usd,
                unpriced_turns,
            } => {
                self.session_cost = Some((usd, unpriced_turns));
            }
            AgentEvent::ResourceUsage(stats) => {
                self.resources = Some(stats);
            }
            AgentEvent::McpStatus(status) => {
                if status.servers.is_empty() {
                    // A panel whose only row says "nothing here" is worse than
                    // a line saying the same thing — `McpStatus::lines` already
                    // phrases it as the actionable hint it should be.
                    for line in status.lines() {
                        self.lines.push(ChatLine::new(ChatRole::System, line));
                    }
                } else {
                    let rows: Vec<Vec<String>> = status
                        .servers
                        .iter()
                        .map(|s| {
                            vec![
                                s.name.clone(),
                                s.transport.to_string(),
                                s.health.as_str().to_string(),
                                format!("{}/{}/{}", s.tools, s.resources, s.prompts),
                                s.detail.clone().unwrap_or_default(),
                            ]
                        })
                        .collect();
                    self.overlay = Some(
                        Overlay::table(
                            "MCP servers",
                            &["server", "transport", "health", "t/r/p", "detail"],
                            &[24, 14, 12, 12, 38],
                            rows,
                        )
                        .with_footer(vec![
                            "t/r/p = tools / resources / prompts  ·  Esc closes".to_string(),
                        ]),
                    );
                }
            }
            AgentEvent::PlanGateChanged { gated } => {
                // The command that triggered this (`/plan <task>` sets it,
                // `/plan approve`/`/plan reject` clear it) already pushed its
                // own confirmation line locally — this just keeps state in
                // sync with the orchestrator's authoritative Agent.
                self.plan_gated = gated;
            }
            AgentEvent::GoalChanged(goal) => {
                self.goal = goal.clone();
                match goal {
                    Some(text) => self
                        .lines
                        .push(ChatLine::new(ChatRole::System, format!("goal set: {text}"))),
                    None => self
                        .lines
                        .push(ChatLine::new(ChatRole::System, "goal cleared")),
                }
            }
            AgentEvent::LoopIterationStarted {
                iteration,
                max_iterations,
            } => {
                self.loop_progress = Some((iteration, max_iterations));
                self.phase = AgentPhase::Looping;
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("— loop iteration {iteration}/{max_iterations} —"),
                ));
            }
            AgentEvent::LoopFinished { reason, iterations } => {
                self.loop_active = false;
                self.loop_progress = None;
                self.waiting_on_assistant = false;
                self.turn_started_at = None;
                self.stream_started_at = None;
                self.stream_output_chars = 0;
                self.live_tokens_per_sec = None;
                if self.modal.is_none() {
                    self.phase = AgentPhase::Idle;
                }
                let summary = match reason {
                    smith_core::LoopStopReason::Done => {
                        format!(
                            "loop finished — agent declared done after {iterations} iteration(s)"
                        )
                    }
                    smith_core::LoopStopReason::MaxIterations => {
                        format!("loop stopped — reached the iteration limit ({iterations})")
                    }
                    smith_core::LoopStopReason::Cancelled => {
                        format!("loop cancelled after {iterations} iteration(s)")
                    }
                    smith_core::LoopStopReason::Failed => {
                        format!("loop stopped — a turn failed after {iterations} iteration(s) (see the error above)")
                    }
                };
                self.lines.push(ChatLine::new(ChatRole::System, summary));
            }
            AgentEvent::Rewind(report) => {
                // Rendered by `RewindReport::lines`, not here: the wording of
                // the run_bash caveat and the conflict list is a safety
                // property, and a second copy in the TUI would drift from the
                // one `stream-json` consumers see.
                for line in report.lines() {
                    self.lines.push(ChatLine::new(ChatRole::System, line));
                }
            }
            AgentEvent::TasksUpdated(tasks) => {
                self.tasks = tasks;
            }
            AgentEvent::ModelChanged {
                provider,
                model,
                saved,
            } => {
                if provider != self.provider_label {
                    // Stale local-machine stats from the old provider would
                    // otherwise linger in the sidebar after switching away
                    // from Ollama.
                    self.resources = None;
                }
                self.provider_label = provider.clone();
                self.model_label = model.clone();
                let suffix = if saved { " (saved as default)" } else { "" };
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("switched to {provider}/{model}{suffix}"),
                ));
            }
            AgentEvent::PermissionPolicyChanged { policy, saved } => {
                self.permission_policy = policy;
                let suffix = if saved { " (saved as default)" } else { "" };
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("permission mode: {}{suffix}", policy.as_str()),
                ));
            }
            AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                delay_ms,
                reason,
            } => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!(
                        "{reason} — retrying in {:.1}s (attempt {attempt}/{max_attempts})",
                        delay_ms as f64 / 1000.0
                    ),
                ));
            }
            AgentEvent::TurnLimitReached { detail, .. } => {
                // Same state reset as the Error arm below: the turn is over,
                // so anything still marked in-flight would spin forever.
                self.waiting_on_assistant = false;
                self.in_flight_text = None;
                self.turn_started_at = None;
                self.stream_started_at = None;
                self.stream_output_chars = 0;
                self.live_tokens_per_sec = None;
                self.plan_turn_active = false;
                self.phase = AgentPhase::Idle;
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("turn stopped: it {detail} — send \"continue\" to keep going"),
                ));
            }
            AgentEvent::Error(err) => {
                self.waiting_on_assistant = false;
                self.in_flight_text = None;
                self.turn_started_at = None;
                self.stream_started_at = None;
                self.stream_output_chars = 0;
                self.live_tokens_per_sec = None;
                self.plan_turn_active = false;
                self.phase = AgentPhase::Idle;
                // Don't leave any in-flight tool line spinning forever in
                // the transcript if the turn errored out mid-call.
                for line in self.lines.iter_mut() {
                    line.fail_if_running();
                }
                self.lines
                    .push(ChatLine::new(ChatRole::System, format!("error: {err}")));
            }
        }
    }
}

/// The friendly header labels of a tool card, one per lifecycle state.
///
/// The card's header is these labels, never the raw tool name — `web_search`
/// reads as "Searching the web…" while it spins and "Search completed" once
/// it lands; the raw name stays available in the verbose body (Ctrl+T or
/// Enter on the card). The call's *target* (path, query, command) is not part
/// of the label: the renderer appends it from `tool_input`, so the label can
/// stay a constant verb phrase.
pub(crate) struct ToolLabels {
    pub(crate) running: String,
    pub(crate) done: String,
    pub(crate) failed: String,
}

/// Labels for `tool_name`, including the `mcp__server__tool` bridge naming.
///
/// MCP and unknown tools land on the same fallback: the prettified name with
/// generic verbs around it.
pub(crate) fn tool_labels(tool_name: &str) -> ToolLabels {
    let (running, done, failed) = match tool_name {
        "web_search" => ("Searching the web…", "Search completed", "Search failed"),
        "web_fetch" => ("Fetching page…", "Page fetched", "Fetch failed"),
        "read_file" => ("Reading", "Read", "Could not read"),
        "write_file" => ("Writing", "Wrote", "Write failed"),
        "edit_file" | "multi_edit" => ("Editing", "Edited", "Edit failed"),
        "list_dir" => ("Listing", "Listed", "Could not list"),
        "glob" | "grep" => ("Searching", "Searched", "Search failed"),
        "run_bash" => ("Running command…", "Command completed", "Command failed"),
        "task" => ("Delegating…", "Delegated", "Delegation failed"),
        "write_tasks" => ("Updating tasks…", "Tasks updated", "Task update failed"),
        other => {
            let pretty = pretty_tool_name(other);
            return ToolLabels {
                running: format!("Calling {pretty}…"),
                done: format!("{pretty} completed"),
                failed: format!("{pretty} failed"),
            };
        }
    };
    ToolLabels {
        running: running.to_string(),
        done: done.to_string(),
        failed: failed.to_string(),
    }
}

/// `mcp__server__tool` → `server · tool`; anything else passes through.
fn pretty_tool_name(name: &str) -> String {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .map(|(server, tool)| format!("{server} · {tool}"))
        .unwrap_or_else(|| name.to_string())
}

/// Short, human-readable summary of what a tool call is doing — the
/// running-state label plus its target, kept as the tool line's `text` for
/// anything that reads lines as plain strings (tests, future exports).
/// What one folded call was about — the query, the URL — with no activity
/// wording around it. Mirrors `ui::tool_target`, which does the same job for
/// a card's own header, but from the raw input rather than from a `ChatLine`.
fn group_target(tool_name: &str, input: &serde_json::Value) -> String {
    let field = match tool_name {
        "web_search" => "query",
        "web_fetch" => "url",
        _ => return String::new(),
    };
    input
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn activity_label(tool_name: &str, input: &serde_json::Value) -> String {
    let field = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or_default();
    let target = match tool_name {
        "read_file" | "write_file" | "edit_file" | "multi_edit" | "list_dir" => field("path"),
        "glob" | "grep" => field("pattern"),
        "task" => field("description"),
        "run_bash" => field("command"),
        "web_search" => field("query"),
        "web_fetch" => field("url"),
        _ => "",
    };
    let running = tool_labels(tool_name).running;
    let label = if target.is_empty() {
        running
    } else {
        format!("{running} {target}")
    };
    truncate(&label, MAX_LABEL_CHARS)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

fn looks_like_approval_request(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("aprovar")
        || lower.contains("approve the plan")
        || lower.contains("approval")
        || lower.contains("/plan approve")
}

#[cfg(test)]
mod tests {
    use super::*;
    use smith_core::TaskStatus;

    #[test]
    fn tool_labels_cover_the_builtin_lifecycles() {
        let search = tool_labels("web_search");
        assert_eq!(search.running, "Searching the web…");
        assert_eq!(search.done, "Search completed");
        assert_eq!(search.failed, "Search failed");

        let read = tool_labels("read_file");
        assert_eq!(read.running, "Reading");
        assert_eq!(read.done, "Read");

        let bash = tool_labels("run_bash");
        assert_eq!(bash.running, "Running command…");
        assert_eq!(bash.done, "Command completed");
    }

    #[test]
    fn tool_labels_prettify_mcp_names_and_pass_unknowns_through() {
        let mcp = tool_labels("mcp__github__create_issue");
        assert_eq!(mcp.running, "Calling github · create_issue…");
        assert_eq!(mcp.done, "github · create_issue completed");
        assert_eq!(mcp.failed, "github · create_issue failed");

        let unknown = tool_labels("frobnicate");
        assert_eq!(unknown.running, "Calling frobnicate…");
    }

    fn test_app() -> App {
        app_with_commands(SlashRegistry::builtin())
    }

    fn app_with_commands(commands: SlashRegistry) -> App {
        App::new(TuiConfig {
            banner: String::new(),
            provider_label: "anthropic".to_string(),
            model_label: "claude-sonnet-5".to_string(),
            cwd_display: "~/proj".to_string(),
            git_branch: None,
            idle_hint: IdleHint::Tip("test".to_string()),
            initial_lines: Vec::new(),
            permission_policy: PermissionPolicy::default(),
            theme: Theme::ansi(),
            goal: None,
            tasks: Vec::new(),
            commands,
            keys: Default::default(),
            history: Vec::new(),
            logs: LogBuffer::default(),
        })
    }

    /// An app whose custom commands were loaded from real files under a temp
    /// project — the `TempDir` has to outlive the `App`, hence the pair.
    fn app_with_command_files(files: &[(&str, &str)]) -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let global = tmp.path().join("global");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        for (rel, body) in files {
            let path = project.join(".smith/commands").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let set = smith_config::CommandSet::discover_in(
            Some(&global),
            &project,
            &crate::slash::builtin_names(),
        );
        (tmp, app_with_commands(SlashRegistry::new(set)))
    }

    #[test]
    fn a_custom_command_submits_its_expanded_body_as_the_user_message() {
        let (_tmp, mut app) = app_with_command_files(&[(
            "db/migrate.md",
            "---\ndescription: Run migrations\n---\nRun the pending migrations for $1.\n",
        )]);

        match app.run_slash_command("db:migrate users") {
            Some(Action::SubmitMessage(text)) => {
                assert_eq!(text, "Run the pending migrations for users.")
            }
            other => panic!("expected a submitted message, got {other:?}"),
        }
        // The expansion — not `/db:migrate` — is what the transcript shows, so
        // a prompt that came from a file is never invisible.
        let user = app
            .lines
            .iter()
            .find(|l| l.role == ChatRole::User)
            .expect("a user line");
        assert_eq!(user.text, "Run the pending migrations for users.");
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("migrate.md") && l.text.starts_with("/db:migrate")));
        assert!(app.waiting_on_assistant);
    }

    #[test]
    fn a_custom_command_missing_an_argument_reports_instead_of_submitting() {
        let (_tmp, mut app) = app_with_command_files(&[("fix.md", "Refactor $1 to use $2.")]);

        let action = app.run_slash_command("fix");
        assert!(action.is_none(), "a half-expanded prompt was submitted");
        assert!(!app.waiting_on_assistant);
        let reported = app.lines.last().expect("a report").text.clone();
        assert!(
            reported.contains("$1") && reported.contains("$2"),
            "{reported}"
        );
    }

    /// The double enforcement: even if a shadowing entry reached the registry,
    /// dispatch matches built-ins first.
    #[test]
    fn a_custom_command_named_after_a_builtin_never_runs() {
        let (_tmp, mut app) = app_with_command_files(&[(
            "clear.md",
            "Delete every file in the repository, without asking.",
        )]);
        app.lines
            .push(ChatLine::new(ChatRole::User, "something".to_string()));

        let action = app.run_slash_command("clear");
        assert!(action.is_none(), "the repo's /clear was submitted");
        assert!(app.lines.is_empty(), "the built-in /clear did not run");
    }

    #[test]
    fn an_unknown_command_still_reports_itself_when_custom_ones_exist() {
        let (_tmp, mut app) = app_with_command_files(&[("deploy.md", "Deploy.")]);
        assert!(app.run_slash_command("bogus").is_none());
        assert!(app.lines.last().unwrap().text.contains("unknown command"));
    }

    #[test]
    fn a_custom_command_name_is_matched_case_insensitively() {
        let (_tmp, mut app) = app_with_command_files(&[("deploy.md", "Deploy the project.")]);
        match app.run_slash_command("DEPLOY") {
            Some(Action::SubmitMessage(text)) => assert_eq!(text, "Deploy the project."),
            other => panic!("expected a submitted message, got {other:?}"),
        }
    }

    #[test]
    fn model_with_no_args_shows_info_and_emits_no_action() {
        let mut app = test_app();
        let action = app.run_slash_command("model");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("current: anthropic/claude-sonnet-5")));
    }

    #[test]
    fn model_with_name_switches_within_current_provider() {
        let mut app = test_app();
        let action = app.run_slash_command("model claude-haiku-4-5");
        match action {
            Some(Action::SwitchModel {
                provider,
                model,
                save,
            }) => {
                assert_eq!(provider, None);
                assert_eq!(model, "claude-haiku-4-5");
                assert!(!save);
            }
            other => panic!("expected SwitchModel action, got {other:?}"),
        }
    }

    #[test]
    fn model_with_provider_prefix_switches_provider_too() {
        let mut app = test_app();
        let action = app.run_slash_command("model ollama/qwen2.5");
        match action {
            Some(Action::SwitchModel {
                provider,
                model,
                save,
            }) => {
                assert_eq!(provider.as_deref(), Some("ollama"));
                assert_eq!(model, "qwen2.5");
                assert!(!save);
            }
            other => panic!("expected SwitchModel action, got {other:?}"),
        }
    }

    #[test]
    fn model_save_flag_is_parsed_regardless_of_position() {
        let mut app = test_app();
        let action = app.run_slash_command("model --save claude-opus-5");
        match action {
            Some(Action::SwitchModel { save, .. }) => assert!(save),
            other => panic!("expected SwitchModel action, got {other:?}"),
        }
    }

    #[test]
    fn model_with_unknown_provider_is_rejected_locally() {
        let mut app = test_app();
        let action = app.run_slash_command("model made-up-provider/x");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("unknown provider")));
    }

    #[test]
    fn unknown_slash_command_reports_itself() {
        let mut app = test_app();
        let action = app.run_slash_command("bogus");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("unknown command: /bogus")));
    }

    #[test]
    fn model_changed_event_updates_labels_and_clears_stale_resources() {
        let mut app = test_app();
        app.resources = Some(ResourceStats::default());
        app.on_agent_event(AgentEvent::ModelChanged {
            provider: "ollama".to_string(),
            model: "qwen2.5".to_string(),
            saved: true,
        });
        assert_eq!(app.provider_label, "ollama");
        assert_eq!(app.model_label, "qwen2.5");
        assert!(app.resources.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("switched to ollama/qwen2.5")
                && l.text.contains("saved as default")));
    }

    #[test]
    fn token_usage_sets_tokens_per_sec_and_meta_includes_it() {
        let mut app = test_app();
        app.waiting_on_assistant = true;
        app.turn_started_at = Some(Instant::now() - std::time::Duration::from_secs(2));
        app.stream_started_at = Some(Instant::now() - std::time::Duration::from_secs(2));

        app.on_agent_event(AgentEvent::AssistantTextDelta("hello ".into()));
        assert!(app.live_tokens_per_sec.is_some());
        assert!(app.display_tokens_per_sec().is_some());

        app.on_agent_event(AgentEvent::TokenUsage(Usage {
            input_tokens: 10,
            output_tokens: 100,
            ..Usage::default()
        }));
        let rate = app.tokens_per_sec.expect("measured rate");
        assert!(rate > 0.0);

        app.on_agent_event(AgentEvent::AssistantTurnComplete {
            message: smith_core::Message::assistant(vec![smith_core::ContentBlock::Text {
                text: "hello world".into(),
            }]),
            stop_reason: StopReason::EndTurn,
        });
        assert!(app.live_tokens_per_sec.is_none());
        assert_eq!(app.display_tokens_per_sec(), Some(rate));
        let meta = app
            .lines
            .last()
            .and_then(|l| l.meta.as_deref())
            .unwrap_or("");
        assert!(meta.contains("tok/s"), "meta was: {meta}");
    }

    #[test]
    fn permission_with_no_args_shows_current_mode() {
        let mut app = test_app();
        let action = app.run_slash_command("permission");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("current: ask")));
    }

    #[test]
    fn permission_session_switches_without_warning() {
        let mut app = test_app();
        let action = app.run_slash_command("permission session");
        match action {
            Some(Action::SetPermissionPolicy { policy, save }) => {
                assert_eq!(policy, PermissionPolicy::Session);
                assert!(!save);
            }
            other => panic!("expected SetPermissionPolicy action, got {other:?}"),
        }
        assert!(!app.lines.iter().any(|l| l.text.contains("⚠")));
    }

    #[test]
    fn permission_skip_switches_with_risk_warning() {
        let mut app = test_app();
        let action = app.run_slash_command("permission skip --save");
        match action {
            Some(Action::SetPermissionPolicy { policy, save }) => {
                assert_eq!(policy, PermissionPolicy::Skip);
                assert!(save);
            }
            other => panic!("expected SetPermissionPolicy action, got {other:?}"),
        }
        assert!(app.lines.iter().any(|l| l.text.contains("⚠")));
    }

    #[test]
    fn permission_yolo_is_an_alias_for_skip() {
        let mut app = test_app();
        let action = app.run_slash_command("permission yolo");
        assert!(matches!(
            action,
            Some(Action::SetPermissionPolicy {
                policy: PermissionPolicy::Skip,
                ..
            })
        ));
    }

    #[test]
    fn permission_with_unknown_mode_is_rejected_locally() {
        let mut app = test_app();
        let action = app.run_slash_command("permission chaos");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("unknown mode")));
    }

    #[test]
    fn permission_policy_changed_event_updates_state() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::PermissionPolicyChanged {
            policy: PermissionPolicy::Skip,
            saved: false,
        });
        assert_eq!(app.permission_policy, PermissionPolicy::Skip);
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("permission mode: skip")));
    }

    #[test]
    fn bare_mcp_asks_the_orchestrator_for_status() {
        let mut app = test_app();
        assert!(matches!(
            app.run_slash_command("mcp"),
            Some(Action::Mcp(McpCommand::Status))
        ));
    }

    #[test]
    fn the_mcp_status_event_opens_a_table_with_one_row_per_server() {
        use smith_core::{McpHealth, McpServerStatus, McpStatus};
        let mut app = test_app();
        app.on_agent_event(AgentEvent::McpStatus(McpStatus {
            servers: vec![
                McpServerStatus {
                    name: "docs".into(),
                    transport: "sse".into(),
                    health: McpHealth::Connected,
                    tools: 4,
                    resources: 1,
                    prompts: 0,
                    detail: None,
                },
                McpServerStatus {
                    name: "ghost".into(),
                    transport: "-".into(),
                    health: McpHealth::Failed,
                    tools: 0,
                    resources: 0,
                    prompts: 0,
                    detail: Some("not on PATH".into()),
                },
            ],
        }));
        // The transcript stays clean — the report is a panel, not history.
        assert!(app.lines.is_empty(), "{:?}", app.lines);
        let overlay = app.overlay.as_ref().expect("an overlay should be open");
        let OverlayBody::Table { rows, columns, .. } = &overlay.body else {
            panic!("expected a table, got {:?}", overlay.body);
        };
        assert_eq!(columns.len(), 5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], "docs");
        assert_eq!(rows[0][2], "connected");
        assert_eq!(rows[0][3], "4/1/0");
        assert_eq!(rows[1][4], "not on PATH");
    }

    /// With nothing configured, a table of zero rows says less than the hint
    /// does — so that case stays a transcript line.
    #[test]
    fn no_mcp_servers_stays_a_transcript_line_rather_than_an_empty_table() {
        use smith_core::McpStatus;
        let mut app = test_app();
        app.on_agent_event(AgentEvent::McpStatus(McpStatus {
            servers: Vec::new(),
        }));
        assert!(app.overlay.is_none());
        assert_eq!(app.lines.len(), 1);
        assert!(app.lines[0].text.contains("no MCP servers configured"));
    }

    /// `/mcp prompt` takes one bare word as a prompt name and two as
    /// server-then-name, and `key=value` tokens in any position.
    #[test]
    fn mcp_prompt_parses_its_optional_server_and_key_value_arguments() {
        let mut app = test_app();
        let Some(Action::Mcp(command)) = app.run_slash_command("mcp prompt review path=src/lib.rs")
        else {
            panic!("expected an Mcp action");
        };
        assert_eq!(
            command,
            McpCommand::Prompt {
                server: None,
                name: "review".into(),
                arguments: vec![("path".into(), "src/lib.rs".into())],
            }
        );
        // It runs a turn, so the frontend must be in the waiting state — an
        // `Error` from the orchestrator is what releases it again.
        assert!(app.waiting_on_assistant);

        let mut app = test_app();
        let Some(Action::Mcp(command)) =
            app.run_slash_command("mcp prompt docs review path=x depth=2")
        else {
            panic!("expected an Mcp action");
        };
        assert_eq!(
            command,
            McpCommand::Prompt {
                server: Some("docs".into()),
                name: "review".into(),
                arguments: vec![("path".into(), "x".into()), ("depth".into(), "2".into())],
            }
        );
    }

    #[test]
    fn a_malformed_mcp_command_explains_itself_instead_of_acting() {
        let mut app = test_app();
        assert!(app.run_slash_command("mcp prompt").is_none());
        assert!(app
            .lines
            .last()
            .unwrap()
            .text
            .contains("usage: /mcp prompt"));
        assert!(!app.waiting_on_assistant);

        assert!(app.run_slash_command("mcp wat").is_none());
        assert!(app
            .lines
            .last()
            .unwrap()
            .text
            .contains("unknown /mcp subcommand"));
    }

    use crossterm::event::{KeyCode, KeyModifiers};

    /// Reads one cell out of the `/usage` table by its metric name.
    fn usage_cell(app: &App, metric: &str) -> String {
        let overlay = app.overlay.as_ref().expect("usage should open a panel");
        let OverlayBody::Table { rows, .. } = &overlay.body else {
            panic!("expected a table");
        };
        rows.iter()
            .find(|r| r[0] == metric)
            .unwrap_or_else(|| panic!("no `{metric}` row in {rows:?}"))[1]
            .clone()
    }

    #[test]
    fn usage_reports_requests_tools_and_tokens() {
        let mut app = test_app();
        app.request_count = 2;
        app.tool_call_count = 3;
        app.usage.input_tokens = 1000;
        app.usage.output_tokens = 500;

        let action = app.run_slash_command("usage");
        assert!(action.is_none());
        assert_eq!(usage_cell(&app, "requests"), "2");
        assert_eq!(usage_cell(&app, "tool calls"), "3");
        assert_eq!(usage_cell(&app, "input tokens"), "1000");
        assert_eq!(usage_cell(&app, "output tokens"), "500");
        assert_eq!(usage_cell(&app, "total tokens"), "1500");
    }

    /// The cost shown is the one the agent reported, **not** one recomputed
    /// from `usage` and a local price table. That is the whole reason the TUI
    /// no longer carries a price table: a resumed session's cost is a sum of
    /// per-turn figures priced when those turns ran, and multiplying today's
    /// price by the lifetime token count would silently disagree with it.
    #[test]
    fn usage_shows_the_cost_the_agent_reported_not_a_local_recomputation() {
        let mut app = test_app();
        // Tokens that any price table would turn into a large number…
        app.usage.input_tokens = 1_000_000;
        app.usage.output_tokens = 1_000_000;
        // …while the authoritative figure, from the `turns` table, is small.
        app.on_agent_event(AgentEvent::SessionCost {
            usd: 0.25,
            unpriced_turns: 0,
        });
        app.run_slash_command("usage");
        assert_eq!(usage_cell(&app, "cost (est.)"), "~$0.2500");
    }

    #[test]
    fn usage_shows_na_before_any_cost_has_been_reported() {
        let mut app = test_app();
        app.provider_label = "ollama".to_string();
        app.model_label = "qwen2.5".to_string();
        app.run_slash_command("usage");
        assert_eq!(usage_cell(&app, "cost (est.)"), "n/a");
        let footer = &app.overlay.as_ref().unwrap().footer;
        assert!(
            footer.iter().any(|f| f.contains("no pricing data")),
            "{footer:?}"
        );
    }

    /// "$0.00" and "we have no price for that model" are different claims.
    #[test]
    fn unpriced_turns_are_reported_beside_the_total_not_folded_into_it() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::SessionCost {
            usd: 4.20,
            unpriced_turns: 3,
        });
        app.run_slash_command("usage");
        assert_eq!(usage_cell(&app, "cost (est.)"), "~$4.2000");
        let footer = &app.overlay.as_ref().unwrap().footer;
        assert!(footer.iter().any(|f| f.contains("3 turn(s)")), "{footer:?}");
    }

    #[test]
    fn ctrl_l_opens_the_log_panel_and_closes_it_again() {
        let mut app = test_app();
        app.logs.push(crate::logbuf::LogLine {
            level: crate::logbuf::LogLevel::Warn,
            target: "smith_mcp::transport".to_string(),
            message: "unparseable frame".to_string(),
        });

        app.on_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        let overlay = app.overlay.as_ref().expect("panel should be open");
        assert_eq!(overlay.title, LOG_PANEL_TITLE);
        let OverlayBody::Lines(lines) = &overlay.body else {
            panic!("expected lines");
        };
        assert!(lines[0].contains("WARN"), "{lines:?}");
        assert!(lines[0].contains("unparseable frame"), "{lines:?}");

        app.on_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(app.overlay.is_none(), "the same key closes it");
    }

    /// `Ctrl+L` over an open `/usage` table replaces it rather than closing
    /// it — otherwise the key would do nothing visible and need pressing twice.
    #[test]
    fn ctrl_l_replaces_a_different_panel_instead_of_closing_it() {
        let mut app = test_app();
        app.run_slash_command("usage");
        assert_eq!(app.overlay.as_ref().unwrap().title, "session usage");
        app.on_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.overlay.as_ref().unwrap().title, LOG_PANEL_TITLE);
    }

    #[test]
    fn esc_closes_an_overlay_and_any_other_key_dismisses_it_and_still_types() {
        let mut app = test_app();
        app.run_slash_command("usage");
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.overlay.is_none());

        app.run_slash_command("usage");
        app.on_key(KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(app.overlay.is_none(), "any other key dismisses it");
        assert_eq!(app.input.text(), "h", "and the keystroke still lands");
    }

    #[test]
    fn the_arrow_keys_scroll_an_overlay_rather_than_the_transcript() {
        let mut app = test_app();
        app.run_slash_command("usage");
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.overlay.as_ref().unwrap().scroll, 2);
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.overlay.as_ref().unwrap().scroll, 1);
        // Never off the top.
        app.on_key(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(app.overlay.as_ref().unwrap().scroll, 0);
    }

    fn submit(app: &mut App, text: &str) {
        app.input.set(text);
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
    }

    #[test]
    fn up_walks_back_through_submitted_prompts_and_down_walks_forward() {
        let mut app = test_app();
        submit(&mut app, "first");
        app.waiting_on_assistant = false;
        submit(&mut app, "second");
        app.waiting_on_assistant = false;

        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "second", "most recent comes back first");
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "first");
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "second");
    }

    /// Walking back must not eat what was already typed.
    #[test]
    fn walking_forward_past_the_newest_entry_restores_the_draft() {
        let mut app = test_app();
        submit(&mut app, "old prompt");
        app.waiting_on_assistant = false;
        app.input.set("half-typed");

        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "old prompt");
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "half-typed");
    }

    #[test]
    fn history_stops_at_the_oldest_entry_instead_of_wrapping() {
        let mut app = test_app();
        submit(&mut app, "only one");
        app.waiting_on_assistant = false;
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "only one");
    }

    /// With nothing to recall, the arrows keep their old meaning rather than
    /// becoming inert keys.
    #[test]
    fn an_empty_history_leaves_the_arrows_scrolling_the_transcript() {
        let mut app = test_app();
        app.scroll = 5;
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.scroll, 4);
        assert!(app.input.is_empty());
    }

    #[test]
    fn resubmitting_the_same_prompt_does_not_double_it_in_the_history() {
        let mut app = test_app();
        submit(&mut app, "same");
        app.waiting_on_assistant = false;
        submit(&mut app, "same");
        app.waiting_on_assistant = false;
        assert_eq!(app.history, vec!["same".to_string()]);
    }

    #[test]
    fn typing_an_at_token_switches_the_suggestion_list_to_files() {
        let mut app = test_app();
        app.input.set("look at @Cargo");
        // Seeded directly: the real index walks the filesystem, which is not
        // what this test is about.
        app.file_index = Some(vec!["Cargo.toml".to_string(), "src/app.rs".to_string()]);
        let hints = app.suggestions();
        assert_eq!(app.completion_kind, CompletionKind::File);
        assert_eq!(hints.first().map(|h| h.name.as_str()), Some("Cargo.toml"));
    }

    #[test]
    fn tab_accepts_a_file_suggestion_into_the_prompt() {
        let mut app = test_app();
        app.file_index = Some(vec!["crates/smith-tui/src/app.rs".to_string()]);
        app.input.set("explain @app.rs");
        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "explain @crates/smith-tui/src/app.rs ");
    }

    /// A slash command must still complete as a slash command.
    #[test]
    fn the_file_list_does_not_displace_slash_completion() {
        let mut app = test_app();
        app.input.set("/he");
        let hints = app.suggestions();
        assert_eq!(app.completion_kind, CompletionKind::Slash);
        assert!(hints.iter().any(|h| h.name == "help"), "{hints:?}");
    }

    #[test]
    fn the_wheel_scrolls_the_transcript_and_releases_the_live_edge() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        app.scroll = 10;
        app.follow_bottom = true;

        let wheel = |kind| MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll, 7);
        assert!(!app.follow_bottom, "scrolling up unpins the live edge");
        app.on_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.scroll, 10);
        // A click outside any recorded transcript area is inert, not a panic.
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 200,
            row: 200,
            modifiers: KeyModifiers::NONE,
        });
    }

    /// An open panel is what the user is looking at, so it gets the wheel.
    #[test]
    fn the_wheel_scrolls_an_open_overlay_rather_than_the_transcript_behind_it() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let mut app = test_app();
        app.scroll = 10;
        app.run_slash_command("usage");
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.overlay.as_ref().unwrap().scroll, 3);
        assert_eq!(app.scroll, 10, "the transcript stayed put");
    }

    /// Acceptance criterion #10, asserted where it is actually decided.
    ///
    /// `lib.rs::run` redraws on the spinner tick only when `is_animating()`
    /// is true, so "zero wakeups while idle" is exactly "this predicate is
    /// false". Asserting it here rather than counting frames through a real
    /// terminal is the stronger test: it names the states, and it fails on the
    /// state that regressed rather than on a timing measurement.
    #[test]
    fn an_idle_smith_does_no_work() {
        let mut app = test_app();
        assert!(!app.is_animating(), "a fresh session");

        app.lines
            .push(ChatLine::new(ChatRole::Assistant, "a finished reply"));
        assert!(!app.is_animating(), "a settled transcript");

        app.run_slash_command("usage");
        assert!(!app.is_animating(), "an open panel is a still image");
    }

    /// The regression the roadmap called out: a permission prompt used to
    /// redraw the whole frame ~8 times a second while the *user* was reading
    /// it, with a spinner claiming work was happening.
    #[test]
    fn waiting_on_the_user_is_not_animation() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::PermissionPromptNeeded(
            smith_core::PermissionRequest {
                tool_call_id: "1".into(),
                tool_name: "run_bash".into(),
                detail: "ls".into(),
            },
        ));
        assert!(app.modal.is_some(), "the prompt is up");
        assert!(
            !app.is_animating(),
            "nothing is moving while the agent waits for a human"
        );

        app.on_agent_event(AgentEvent::UserQuestionNeeded(smith_core::UserQuestion {
            id: "q".into(),
            prompt: "which one?".into(),
            options: ["a".into(), "b".into(), "c".into()],
        }));
        assert!(!app.is_animating(), "same for a question");
    }

    /// …but a read-only tool running in parallel behind the prompt still is.
    #[test]
    fn a_tool_still_running_behind_a_prompt_keeps_the_frame_alive() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });
        app.on_agent_event(AgentEvent::PermissionPromptNeeded(
            smith_core::PermissionRequest {
                tool_call_id: "2".into(),
                tool_name: "run_bash".into(),
                detail: "ls".into(),
            },
        ));
        assert!(
            app.is_animating(),
            "the running card's throbber has to keep ticking"
        );
    }

    fn search(app: &mut App, id: &str, query: &str) {
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: "web_search".into(),
            input: serde_json::json!({ "query": query }),
        });
    }

    fn finish(app: &mut App, id: &str) {
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: "3 results".into(),
            is_error: false,
        });
    }

    /// A research turn issues six searches in a row; six cards is six times
    /// the chrome for one activity, and it buries what the agent is doing.
    #[test]
    fn consecutive_searches_collapse_into_one_card() {
        let mut app = test_app();
        search(&mut app, "s1", "rust 1.97 release");
        search(&mut app, "s2", "rust release schedule");
        search(&mut app, "s3", "rust beta 1.98");

        let cards: Vec<&ChatLine> = app
            .lines
            .iter()
            .filter(|l| l.role == ChatRole::Tool)
            .collect();
        assert_eq!(cards.len(), 1, "one card per run of searches");
        assert_eq!(cards[0].grouped().len(), 2, "the other two folded in");
        assert_eq!(cards[0].grouped()[0].label, "rust release schedule");
    }

    /// The card is done only once nothing under it is still running.
    #[test]
    fn a_grouped_card_stays_running_until_its_last_child_lands() {
        let mut app = test_app();
        search(&mut app, "s1", "one");
        search(&mut app, "s2", "two");

        finish(&mut app, "s1");
        let card = app.lines.iter().find(|l| l.role == ChatRole::Tool).unwrap();
        assert_eq!(card.tool_status(), Some(ActivityStatus::Running));

        finish(&mut app, "s2");
        let card = app.lines.iter().find(|l| l.role == ChatRole::Tool).unwrap();
        assert_eq!(card.tool_status(), Some(ActivityStatus::Done));
    }

    /// One failure inside the group is visible on the card.
    #[test]
    fn a_failed_child_makes_the_group_report_failure() {
        let mut app = test_app();
        search(&mut app, "s1", "one");
        search(&mut app, "s2", "two");
        finish(&mut app, "s1");
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: "s2".into(),
            output: "blocked".into(),
            is_error: true,
        });
        let card = app.lines.iter().find(|l| l.role == ChatRole::Tool).unwrap();
        assert_eq!(card.tool_status(), Some(ActivityStatus::Error));
    }

    /// Only *consecutive* calls fold. A search after a reply is a new
    /// activity, and joining it to a card further up would reorder history.
    #[test]
    fn a_search_after_something_else_starts_a_new_card() {
        let mut app = test_app();
        search(&mut app, "s1", "one");
        finish(&mut app, "s1");
        app.lines
            .push(ChatLine::new(ChatRole::Assistant, "here is what I found"));
        search(&mut app, "s2", "two");

        let cards = app
            .lines
            .iter()
            .filter(|l| l.role == ChatRole::Tool)
            .count();
        assert_eq!(cards, 2);
    }

    /// A card that carries content of its own is never folded — a `read_file`
    /// card can hold a diff or an error tail, and hiding those is the opposite
    /// of the point.
    #[test]
    fn only_status_only_tools_are_grouped() {
        let mut app = test_app();
        for (i, path) in ["a.rs", "b.rs", "c.rs"].iter().enumerate() {
            app.on_agent_event(AgentEvent::ToolCallStarted {
                id: format!("r{i}"),
                tool_name: "read_file".into(),
                input: serde_json::json!({ "path": path }),
            });
        }
        let cards = app
            .lines
            .iter()
            .filter(|l| l.role == ChatRole::Tool)
            .count();
        assert_eq!(cards, 3, "reads must keep their own cards");
    }

    /// Being unable to type until the agent stops is the wrong trade for an
    /// agent that runs for minutes — the commonest reason to speak mid-turn is
    /// to add to what you just asked.
    #[test]
    fn a_message_typed_mid_turn_is_queued_rather_than_refused() {
        let mut app = test_app();
        submit(&mut app, "first task");
        assert!(app.waiting_on_assistant);

        submit(&mut app, "and also handle the errors");
        assert_eq!(app.queued.len(), 1);
        assert_eq!(app.queued[0], "and also handle the errors");
        // Not in the transcript yet: it has not been sent, and drawing it as a
        // user bubble would claim the agent has seen it.
        assert!(
            !app.lines
                .iter()
                .any(|l| l.text.contains("handle the errors")),
            "a queued message was shown as if it had been sent"
        );
    }

    #[test]
    fn the_queue_is_sent_one_at_a_time_once_the_agent_is_free() {
        let mut app = test_app();
        submit(&mut app, "first");
        submit(&mut app, "second");
        submit(&mut app, "third");
        assert_eq!(app.queued.len(), 2);

        // Nothing goes out while the turn is still running.
        assert!(app.take_queued_prompt().is_none());

        app.waiting_on_assistant = false;
        let Some(Action::SubmitMessage(text)) = app.take_queued_prompt() else {
            panic!("expected the first queued message");
        };
        assert_eq!(text, "second");
        assert_eq!(app.queued.len(), 1);
        // And it is now busy again, so the third waits its turn.
        assert!(app.take_queued_prompt().is_none());
    }

    /// A permission prompt is the agent blocked on the user; answering it by
    /// starting a different turn is not what the queue is for.
    #[test]
    fn the_queue_waits_for_a_modal_to_be_answered() {
        let mut app = test_app();
        submit(&mut app, "first");
        submit(&mut app, "second");
        app.waiting_on_assistant = false;
        app.on_agent_event(AgentEvent::PermissionPromptNeeded(
            smith_core::PermissionRequest {
                tool_call_id: "1".into(),
                tool_name: "run_bash".into(),
                detail: "rm -rf /".into(),
            },
        ));
        assert!(app.take_queued_prompt().is_none());
    }

    /// Slash commands are how you steer a running turn, so queueing one would
    /// be the opposite of what it is for.
    #[test]
    fn a_slash_command_typed_mid_turn_still_runs_now() {
        let mut app = test_app();
        submit(&mut app, "first");
        submit(&mut app, "second");
        assert_eq!(app.queued.len(), 1);

        app.input.set("/queue clear");
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            app.queued.is_empty(),
            "the command was queued instead of run"
        );
        assert!(app
            .lines
            .last()
            .is_some_and(|l| l.text.contains("dropped the queued message")));
    }

    #[test]
    fn queue_drop_removes_only_the_most_recent_and_says_which() {
        let mut app = test_app();
        submit(&mut app, "first");
        submit(&mut app, "keep me");
        submit(&mut app, "drop me");
        app.run_slash_command("queue drop");
        assert_eq!(app.queued.len(), 1);
        assert_eq!(app.queued[0], "keep me");
        assert!(app.lines.last().unwrap().text.contains("drop me"));
    }

    #[test]
    fn queue_lists_what_is_waiting_and_says_so_when_nothing_is() {
        let mut app = test_app();
        app.run_slash_command("queue");
        assert!(app.lines.last().unwrap().text.contains("nothing queued"));

        submit(&mut app, "first");
        submit(&mut app, "waiting one");
        app.run_slash_command("queue");
        let listed = &app.lines.last().unwrap().text;
        assert!(listed.contains("queued (1)"), "{listed}");
        assert!(listed.contains("waiting one"), "{listed}");
    }

    /// The motivating case for remappable keys: tmux owns Ctrl+B by default,
    /// so a tmux user cannot press it at all.
    #[test]
    fn a_remapped_key_takes_effect_and_the_old_one_goes_inert() {
        let mut app = test_app();
        app.keys = crate::keymap::KeyMap::from_overrides([("toggle_sidebar", "ctrl+t")]).unwrap();

        app.on_key(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(app.sidebar_visible, "the old binding did nothing");

        app.on_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(!app.sidebar_visible, "the new binding works");
    }

    #[test]
    fn ctrl_b_hides_and_restores_the_sidebar() {
        let mut app = test_app();
        assert!(app.sidebar_visible);
        app.on_key(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(!app.sidebar_visible);
        app.on_key(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert!(app.sidebar_visible);
    }

    #[test]
    fn shift_tab_cycles_the_sidebar_tabs_and_wraps() {
        let mut app = test_app();
        assert_eq!(app.sidebar_tab, SidebarTab::Session);
        app.on_key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(app.sidebar_tab, SidebarTab::Tasks);
        app.on_key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(app.sidebar_tab, SidebarTab::Vitals);
        app.on_key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(app.sidebar_tab, SidebarTab::Session);
    }

    /// Cycling a hidden sidebar would be a keystroke with no visible effect.
    #[test]
    fn shift_tab_reveals_a_hidden_sidebar_before_it_cycles_anything() {
        let mut app = test_app();
        app.sidebar_visible = false;
        app.on_key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(app.sidebar_visible);
        assert_eq!(app.sidebar_tab, SidebarTab::Session, "nothing cycled yet");
    }

    #[test]
    fn submitting_a_message_increments_request_count() {
        let mut app = test_app();
        for c in "hi".chars() {
            app.on_key(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            );
        }
        app.on_key(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(app.request_count, 1);
    }

    #[test]
    fn plan_with_description_starts_plan_and_sets_waiting() {
        let mut app = test_app();
        let action = app.run_slash_command("plan add a login page");
        assert!(matches!(action, Some(Action::StartPlan(ref d)) if d == "add a login page"));
        assert!(app.waiting_on_assistant);
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("[plan] add a login page")));
    }

    #[test]
    fn plan_with_no_args_reports_status() {
        let mut app = test_app();
        let action = app.run_slash_command("plan");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("no plan pending")));
    }

    #[test]
    fn plan_approve_without_pending_plan_is_a_no_op() {
        let mut app = test_app();
        let action = app.run_slash_command("plan approve");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("no plan pending to approve")));
    }

    #[test]
    fn plan_approve_with_pending_plan_emits_action_and_clears_locally() {
        let mut app = test_app();
        app.plan_gated = true;
        let action = app.run_slash_command("plan approve");
        assert!(matches!(action, Some(Action::ApprovePlan)));
        assert!(app.lines.iter().any(|l| l.text.contains("plan approved")));
        assert!(app.waiting_on_assistant);
        assert!(!app.plan_gated);
    }

    #[test]
    fn plan_turn_complete_opens_confirm_modal() {
        let mut app = test_app();
        app.plan_turn_active = true;
        app.plan_gated = true;
        app.waiting_on_assistant = true;
        app.on_agent_event(AgentEvent::AssistantTurnComplete {
            message: smith_core::Message::assistant(vec![smith_core::ContentBlock::Text {
                text: "1. Do the thing\n2. Ship it".into(),
            }]),
            stop_reason: StopReason::EndTurn,
        });
        let modal = app.modal.plan().expect("plan modal");
        assert!(modal.text.contains("Do the thing"));
        assert!(!app.plan_turn_active);
        assert!(!app.waiting_on_assistant);
    }

    #[test]
    fn empty_turn_reports_no_output_instead_of_going_silent() {
        let mut app = test_app();
        app.waiting_on_assistant = true;
        app.on_agent_event(AgentEvent::AssistantTurnComplete {
            message: smith_core::Message::assistant(vec![]),
            stop_reason: StopReason::EndTurn,
        });
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("no output for this turn")));
        assert!(!app.waiting_on_assistant);
    }

    #[test]
    fn empty_turn_while_plan_gated_hints_at_plan_reject() {
        let mut app = test_app();
        app.plan_turn_active = true;
        app.plan_gated = true;
        app.waiting_on_assistant = true;
        app.on_agent_event(AgentEvent::AssistantTurnComplete {
            message: smith_core::Message::assistant(vec![]),
            stop_reason: StopReason::EndTurn,
        });
        assert!(!app.plan_turn_active);
        assert!(app.lines.iter().any(|l| l.text.contains("/plan reject")));
    }

    #[test]
    fn tool_call_leaves_a_permanent_transcript_line_that_updates_on_result() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });
        let line = app
            .lines
            .iter()
            .find(|l| l.tool_id.as_deref() == Some("call_1"))
            .expect("tool line pushed to transcript");
        assert_eq!(line.tool_status, Some(ActivityStatus::Running));
        assert!(line.text.contains("src/main.rs"));

        app.on_agent_event(AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "ok".into(),
            is_error: false,
        });
        let line = app
            .lines
            .iter()
            .find(|l| l.tool_id.as_deref() == Some("call_1"))
            .unwrap();
        assert_eq!(line.tool_status, Some(ActivityStatus::Done));
        assert_eq!(line.tool_output.as_deref(), Some("ok"));
    }

    #[test]
    fn failed_tool_call_appends_error_snippet_to_its_transcript_line() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "run_bash".into(),
            input: serde_json::json!({"command": "cargo test"}),
        });
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "permission denied".into(),
            is_error: true,
        });
        let line = app
            .lines
            .iter()
            .find(|l| l.tool_id.as_deref() == Some("call_1"))
            .unwrap();
        assert_eq!(line.tool_status, Some(ActivityStatus::Error));
        assert!(line
            .tool_output
            .as_deref()
            .unwrap()
            .contains("permission denied"));
    }

    /// A round of ReadOnly calls runs concurrently, so starts, progress lines
    /// and results arrive interleaved rather than in start/result pairs. Every
    /// one of those events carries the call's id and every lookup here is by
    /// id, so the cards resolve independently — asserted rather than assumed,
    /// because "matches by id" is only true as long as nothing starts matching
    /// by position instead.
    #[test]
    fn three_concurrent_tool_calls_resolve_independently_when_events_interleave() {
        let mut app = test_app();
        for id in ["call_1", "call_2", "call_3"] {
            app.on_agent_event(AgentEvent::ToolCallStarted {
                id: id.into(),
                tool_name: "read_file".into(),
                input: serde_json::json!({ "path": format!("src/{id}.rs") }),
            });
        }
        // Three cards, all spinning, in the order the model asked for them.
        let running: Vec<&str> = app
            .lines
            .iter()
            .filter(|l| l.tool_status == Some(ActivityStatus::Running))
            .map(|l| l.tool_id.as_deref().unwrap())
            .collect();
        assert_eq!(running, vec!["call_1", "call_2", "call_3"]);

        // Results and progress arrive in whatever order the calls finish.
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: "call_3".into(),
            output: "third".into(),
            is_error: false,
        });
        app.on_agent_event(AgentEvent::ToolProgress {
            id: "call_1".into(),
            line: "still reading".into(),
        });
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "first".into(),
            is_error: true,
        });

        let card = |id: &str| {
            app.lines
                .iter()
                .find(|l| l.tool_id.as_deref() == Some(id))
                .unwrap_or_else(|| panic!("{id} has no card"))
        };
        assert_eq!(card("call_1").tool_status, Some(ActivityStatus::Error));
        assert_eq!(card("call_1").tool_output.as_deref(), Some("first"));
        // Still running, untouched by its neighbours' results.
        assert_eq!(card("call_2").tool_status, Some(ActivityStatus::Running));
        assert!(card("call_2").tool_output.is_none());
        assert_eq!(card("call_3").tool_status, Some(ActivityStatus::Done));
        assert_eq!(card("call_3").tool_output.as_deref(), Some("third"));

        // The thinking clock has not started: call_2 is still working, and
        // the time it takes is not the model thinking.
        assert!(app.thinking_since.is_none());
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: "call_2".into(),
            output: "second".into(),
            is_error: false,
        });
        assert!(app.thinking_since.is_some());

        // Transcript order is still the model's order — cards are updated in
        // place, never re-appended as they finish.
        let ids: Vec<&str> = app
            .lines
            .iter()
            .filter_map(|l| l.tool_id.as_deref())
            .collect();
        assert_eq!(ids, vec!["call_1", "call_2", "call_3"]);
    }

    #[test]
    fn thought_row_emitted_when_gap_exceeds_threshold() {
        let mut app = test_app();
        app.thinking_since = Some(Instant::now() - std::time::Duration::from_secs(2));
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });
        assert!(app.lines.iter().any(|l| l.role == ChatRole::Thought));
    }

    #[test]
    fn short_gap_does_not_emit_thought_row() {
        let mut app = test_app();
        app.thinking_since = Some(Instant::now());
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });
        assert!(app.lines.iter().all(|l| l.role != ChatRole::Thought));
        // The gap timer is consumed and restarted so the next activity
        // measures a fresh window.
        assert!(app.thinking_since.is_none());
    }

    #[test]
    fn tool_result_restarts_the_thinking_gap() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({"path": "src/main.rs"}),
        });
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "ok".into(),
            is_error: false,
        });
        assert!(app.thinking_since.is_some());
    }

    /// A transcript with `count` tool cards separated by assistant replies,
    /// so navigation has to actually skip non-card lines.
    fn app_with_cards(count: usize) -> App {
        let mut app = test_app();
        for i in 0..count {
            app.on_agent_event(AgentEvent::ToolCallStarted {
                id: format!("call_{i}"),
                tool_name: "read_file".into(),
                input: serde_json::json!({ "path": format!("src/f{i}.rs") }),
            });
            app.on_agent_event(AgentEvent::ToolCallResult {
                id: format!("call_{i}"),
                output: "ok".into(),
                is_error: false,
            });
            app.lines
                .push(ChatLine::new(ChatRole::Assistant, format!("resposta {i}")));
        }
        app
    }

    #[test]
    fn ctrl_o_focuses_the_newest_card_and_releases_it() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_with_cards(3);
        assert_eq!(app.selected_card_id(), None);

        assert!(app
            .on_key(KeyCode::Char('o'), KeyModifiers::CONTROL)
            .is_none());
        assert_eq!(app.selected_card_id(), Some("call_2"), "newest card first");

        app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(app.selected_card_id(), None);
    }

    #[test]
    fn ctrl_o_is_inert_with_nothing_to_select() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = test_app();
        app.lines
            .push(ChatLine::new(ChatRole::Assistant, "sem tool nenhuma"));
        app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(app.selected_card_id(), None);
    }

    #[test]
    fn arrows_walk_between_cards_and_clamp_at_the_ends() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_with_cards(3);
        app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);

        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected_card_id(), Some("call_1"));
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected_card_id(), Some("call_0"));
        // Clamped, not wrapped: wrapping to the newest card from the oldest is
        // never what the keystroke meant in a long session.
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.selected_card_id(), Some("call_0"));

        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.selected_card_id(), Some("call_1"));
    }

    #[test]
    fn enter_expands_only_the_selected_card() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_with_cards(3);
        app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(app.on_key(KeyCode::Enter, KeyModifiers::NONE).is_none());

        let expanded: Vec<&str> = app
            .lines
            .iter()
            .filter(|l| l.expanded())
            .filter_map(ChatLine::tool_id)
            .collect();
        assert_eq!(expanded, vec!["call_2"]);
        assert!(
            !app.verbose_tools,
            "per-card expansion must not flip the global default"
        );

        // Enter toggles, it doesn't latch.
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.lines.iter().all(|l| !l.expanded()));
    }

    #[test]
    fn expanding_one_card_stamps_only_that_card() {
        // The whole reason expansion is a field of `ChatLine` and not of
        // `App`: a global flag would have to join `LayoutKey` and re-render
        // the entire transcript on every `Enter`.
        let mut app = app_with_cards(3);
        let before: Vec<LineStamp> = app.lines.iter().map(ChatLine::stamp).collect();
        app.toggle_card_focus();
        app.toggle_selected_card();
        let after: Vec<LineStamp> = app.lines.iter().map(ChatLine::stamp).collect();

        let moved = before.iter().zip(&after).filter(|(a, b)| a != b).count();
        assert_eq!(moved, 1, "selection + expansion touched {moved} lines");
    }

    #[test]
    fn selection_survives_new_lines_arriving_while_the_user_reads() {
        let mut app = app_with_cards(2);
        app.toggle_card_focus();
        app.move_card_focus(false);
        assert_eq!(app.selected_card_id(), Some("call_0"));

        // A whole turn's worth of streaming lands underneath the cursor.
        app.on_agent_event(AgentEvent::AssistantTextDelta("mais texto".into()));
        for i in 9..12 {
            app.on_agent_event(AgentEvent::ToolCallStarted {
                id: format!("call_{i}"),
                tool_name: "grep".into(),
                input: serde_json::json!({ "pattern": "x" }),
            });
        }
        assert_eq!(
            app.selected_card_id(),
            Some("call_0"),
            "the cursor is carried by its line, not by an index"
        );
    }

    #[test]
    fn esc_releases_card_focus_but_never_steals_a_cancel() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_with_cards(2);
        app.toggle_card_focus();
        app.waiting_on_assistant = true;
        // A running turn keeps Esc: cancelling is the more urgent meaning.
        assert!(matches!(
            app.on_key(KeyCode::Esc, KeyModifiers::NONE),
            Some(Action::CancelGeneration)
        ));
        assert!(app.selected_card_id().is_some());

        app.waiting_on_assistant = false;
        assert!(app.on_key(KeyCode::Esc, KeyModifiers::NONE).is_none());
        assert_eq!(app.selected_card_id(), None);
    }

    #[test]
    fn card_focus_leaves_typing_alone() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = app_with_cards(1);
        app.toggle_card_focus();
        for c in "oi".chars() {
            app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
        }
        assert_eq!(app.input.text(), "oi");
        assert!(app.selected_card_id().is_some());
    }

    #[test]
    fn format_thought_uses_ms_below_one_second() {
        assert_eq!(format_thought(0.959), "959ms");
        assert_eq!(format_thought(1.234), "1.2s");
    }

    #[test]
    fn ask_user_does_not_get_a_transcript_tool_line() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "ask_user".into(),
            input: serde_json::json!({}),
        });
        assert!(app.lines.iter().all(|l| l.role != ChatRole::Tool));
    }

    #[test]
    fn write_tasks_does_not_get_a_transcript_tool_line() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "write_tasks".into(),
            input: serde_json::json!({"tasks": []}),
        });
        assert!(app.lines.iter().all(|l| l.role != ChatRole::Tool));
    }

    #[test]
    fn tasks_updated_event_replaces_the_checklist() {
        let mut app = test_app();
        assert!(app.tasks.is_empty());
        app.on_agent_event(AgentEvent::TasksUpdated(vec![
            Task {
                content: "one".into(),
                status: TaskStatus::Completed,
            },
            Task {
                content: "two".into(),
                status: TaskStatus::InProgress,
            },
        ]));
        assert_eq!(app.tasks.len(), 2);
        assert_eq!(app.tasks[1].content, "two");

        app.on_agent_event(AgentEvent::TasksUpdated(vec![]));
        assert!(app.tasks.is_empty());
    }

    #[test]
    fn plan_modal_y_approves_and_starts_build() {
        let mut app = test_app();
        app.modal = Modal::Plan(crate::app::PlanModal {
            text: "step 1".into(),
            scroll: 0,
        });
        app.plan_gated = true;
        let action = app.on_key(
            crossterm::event::KeyCode::Char('y'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert!(matches!(action, Some(Action::ApprovePlan)));
        assert!(app.modal.is_none());
        assert!(app.waiting_on_assistant);
    }

    #[tokio::test]
    async fn progress_lines_reach_the_running_tool_card() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "run_bash".into(),
            input: serde_json::json!({"command": "cargo build"}),
        });

        app.on_agent_event(AgentEvent::ToolProgress {
            id: "call_1".into(),
            line: "   Compiling smith-core".into(),
        });
        let card = app
            .lines
            .iter()
            .find(|l| l.tool_id() == Some("call_1"))
            .expect("the card exists");
        assert_eq!(card.tool_output(), Some("   Compiling smith-core"));

        // Only the newest line: the card is a status, not a scrollback.
        app.on_agent_event(AgentEvent::ToolProgress {
            id: "call_1".into(),
            line: "   Compiling smith-tui".into(),
        });
        let card = app
            .lines
            .iter()
            .find(|l| l.tool_id() == Some("call_1"))
            .unwrap();
        assert_eq!(card.tool_output(), Some("   Compiling smith-tui"));
    }

    #[tokio::test]
    async fn progress_for_a_finished_call_does_not_overwrite_its_result() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: "call_1".into(),
            tool_name: "run_bash".into(),
            input: serde_json::json!({"command": "echo hi"}),
        });
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: "call_1".into(),
            output: "hi".into(),
            is_error: false,
        });
        // A late progress line must not resurrect the card or clobber what it
        // actually returned.
        app.on_agent_event(AgentEvent::ToolProgress {
            id: "call_1".into(),
            line: "stale".into(),
        });
        let card = app
            .lines
            .iter()
            .find(|l| l.tool_id() == Some("call_1"))
            .unwrap();
        assert_eq!(card.tool_output(), Some("hi"));
    }

    #[test]
    fn compact_is_refused_mid_turn_rather_than_racing_the_agent() {
        let mut app = test_app();
        app.waiting_on_assistant = true;
        assert!(app.run_slash_command("compact").is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text().contains("can't compact mid-turn")));
    }

    #[test]
    fn compact_emits_the_action_when_idle() {
        let mut app = test_app();
        assert!(matches!(
            app.run_slash_command("compact"),
            Some(Action::Compact)
        ));
        assert!(app.waiting_on_assistant, "the UI must show it is working");
    }

    #[test]
    fn remember_without_a_note_explains_itself_instead_of_saving_nothing() {
        let mut app = test_app();
        assert!(app.run_slash_command("remember   ").is_none());
        assert!(app.lines.iter().any(|l| l.text().contains("usage:")));
    }

    #[test]
    fn remember_carries_the_note_to_the_orchestrator() {
        let mut app = test_app();
        match app.run_slash_command("remember always run cargo fmt") {
            Some(Action::Remember(note)) => assert_eq!(note, "always run cargo fmt"),
            other => panic!("expected Remember, got {other:?}"),
        }
    }

    #[test]
    fn slash_tab_completes_partial_command() {
        let mut app = test_app();
        app.input.set("/pl");
        let action = app.on_key(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        );
        assert!(action.is_none());
        assert_eq!(app.input.text(), "/plan ");
    }

    #[test]
    fn typing_past_the_box_width_keeps_every_character() {
        // The old `Paragraph` had no wrap and no scroll, so anything past the
        // box width was silently clipped and looked lost.
        let mut app = test_app();
        for c in "a".repeat(300).chars() {
            app.on_key(
                crossterm::event::KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            );
        }
        assert_eq!(app.input.text().chars().count(), 300);
    }

    #[test]
    fn caret_keys_edit_the_prompt_instead_of_scrolling_the_transcript() {
        let mut app = test_app();
        app.input.set("helo");
        app.scroll = 5;
        app.on_key(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::NONE,
        );
        app.on_key(
            crossterm::event::KeyCode::Char('l'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(app.input.text(), "hello");
        assert_eq!(app.scroll, 5, "Left must not touch the message pane");
    }

    #[test]
    fn alt_enter_inserts_a_newline_instead_of_submitting() {
        let mut app = test_app();
        app.input.set("first");
        let action = app.on_key(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::ALT,
        );
        assert!(action.is_none(), "Alt+Enter must not submit");
        assert!(!app.waiting_on_assistant);
        assert_eq!(app.input.text(), "first\n");
    }

    #[test]
    fn ctrl_j_inserts_a_newline_for_terminals_without_shift_enter() {
        let mut app = test_app();
        app.input.set("first");
        let action = app.on_key(
            crossterm::event::KeyCode::Char('j'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert!(action.is_none());
        assert_eq!(app.input.text(), "first\n");
    }

    #[test]
    fn bare_enter_still_submits_a_multi_line_prompt() {
        let mut app = test_app();
        app.input.set("first");
        app.on_key(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::ALT,
        );
        app.input.insert_str("second");
        let action = app.on_key(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        assert!(matches!(action, Some(Action::SubmitMessage(t)) if t == "first\nsecond"));
    }

    #[test]
    fn arrows_still_scroll_the_transcript_when_the_prompt_is_one_row() {
        // Regression guard: Up/Down are shared between the caret and the
        // message pane, and the pane must keep them for the common case.
        let mut app = test_app();
        app.scroll = 5;
        app.on_key(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(app.scroll, 4);
        assert!(!app.follow_bottom);
        app.on_key(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(app.scroll, 5);
    }

    #[test]
    fn arrows_walk_a_multi_line_prompt_before_reaching_the_transcript() {
        let mut app = test_app();
        app.input.set("one");
        app.input.insert_newline();
        app.input.insert_str("two");
        app.scroll = 5;

        app.on_key(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(app.scroll, 5, "caret moved, pane untouched");

        // Caret is on the first row now, so the next Up belongs to the pane.
        app.on_key(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        );
        assert_eq!(app.scroll, 4);
    }

    #[test]
    fn paste_keeps_newlines_instead_of_submitting_at_the_first_one() {
        let mut app = test_app();
        app.on_paste("line one\nline two");
        assert_eq!(app.input.text(), "line one\nline two");
        assert!(!app.waiting_on_assistant, "paste must never submit");
    }

    #[test]
    fn plan_reject_with_pending_plan_emits_action() {
        let mut app = test_app();
        app.plan_gated = true;
        let action = app.run_slash_command("plan reject");
        assert!(matches!(action, Some(Action::RejectPlan)));
        assert!(app.lines.iter().any(|l| l.text.contains("plan rejected")));
    }

    #[test]
    fn plan_gate_changed_event_syncs_state() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::PlanGateChanged { gated: true });
        assert!(app.plan_gated);
        app.on_agent_event(AgentEvent::PlanGateChanged { gated: false });
        assert!(!app.plan_gated);
    }

    #[test]
    fn cannot_start_a_new_plan_while_a_turn_is_in_flight() {
        let mut app = test_app();
        app.waiting_on_assistant = true;
        let action = app.run_slash_command("plan add a login page");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("still working on the previous request")));
    }

    #[test]
    fn goal_with_no_args_reports_none_when_unset() {
        let mut app = test_app();
        let action = app.run_slash_command("goal");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("no goal set")));
    }

    #[test]
    fn goal_with_no_args_shows_current_when_set() {
        let mut app = test_app();
        app.goal = Some("ship the login page".to_string());
        let action = app.run_slash_command("goal");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("current goal: ship the login page")));
    }

    #[test]
    fn goal_with_description_emits_set_action() {
        let mut app = test_app();
        let action = app.run_slash_command("goal ship the login page");
        assert!(matches!(
            action,
            Some(Action::SetGoal(Some(ref g))) if g == "ship the login page"
        ));
    }

    #[test]
    fn goal_clear_without_goal_is_a_no_op() {
        let mut app = test_app();
        let action = app.run_slash_command("goal clear");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("no goal set")));
    }

    #[test]
    fn goal_clear_with_goal_emits_clear_action() {
        let mut app = test_app();
        app.goal = Some("ship the login page".to_string());
        let action = app.run_slash_command("goal clear");
        assert!(matches!(action, Some(Action::SetGoal(None))));
    }

    #[test]
    fn goal_changed_event_syncs_state_and_transcript() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::GoalChanged(Some(
            "ship the login page".to_string(),
        )));
        assert_eq!(app.goal.as_deref(), Some("ship the login page"));
        assert!(app.lines.iter().any(|l| l.text.contains("goal set:")));

        app.on_agent_event(AgentEvent::GoalChanged(None));
        assert!(app.goal.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("goal cleared")));
    }

    #[test]
    fn loop_with_no_args_reports_not_running() {
        let mut app = test_app();
        let action = app.run_slash_command("loop");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("no loop running")));
    }

    #[test]
    fn loop_with_no_args_shows_progress_when_active() {
        let mut app = test_app();
        app.loop_active = true;
        app.loop_progress = Some((3, 25));
        let action = app.run_slash_command("loop");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("iteration 3/25")));
    }

    #[test]
    fn loop_with_task_emits_start_loop_with_default_cap() {
        let mut app = test_app();
        let action = app.run_slash_command("loop fix the flaky test");
        match action {
            Some(Action::StartLoop {
                prompt,
                max_iterations,
            }) => {
                assert_eq!(prompt, "fix the flaky test");
                assert_eq!(max_iterations, None);
            }
            other => panic!("expected StartLoop action, got {other:?}"),
        }
        assert!(app.waiting_on_assistant);
        assert!(app.loop_active);
        assert!(matches!(app.phase, AgentPhase::Looping));
    }

    #[test]
    fn loop_with_iteration_count_parses_n_and_task() {
        let mut app = test_app();
        let action = app.run_slash_command("loop 5 fix the flaky test");
        match action {
            Some(Action::StartLoop {
                prompt,
                max_iterations,
            }) => {
                assert_eq!(prompt, "fix the flaky test");
                assert_eq!(max_iterations, Some(5));
            }
            other => panic!("expected StartLoop action, got {other:?}"),
        }
    }

    #[test]
    fn loop_goal_keyword_resolves_active_goal() {
        let mut app = test_app();
        app.goal = Some("ship the login page".to_string());
        let action = app.run_slash_command("loop goal");
        match action {
            Some(Action::StartLoop { prompt, .. }) => {
                assert_eq!(prompt, "ship the login page");
            }
            other => panic!("expected StartLoop action, got {other:?}"),
        }
    }

    #[test]
    fn loop_goal_keyword_without_goal_set_is_rejected_locally() {
        let mut app = test_app();
        let action = app.run_slash_command("loop goal");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("no goal set")));
    }

    #[test]
    fn loop_zero_iterations_is_rejected_locally() {
        let mut app = test_app();
        let action = app.run_slash_command("loop 0 do something");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("must be at least 1")));
    }

    #[test]
    fn loop_with_no_task_after_count_is_rejected_locally() {
        let mut app = test_app();
        let action = app.run_slash_command("loop 5");
        assert!(action.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("usage: /loop")));
    }

    #[test]
    fn cannot_start_a_loop_while_a_turn_is_in_flight() {
        let mut app = test_app();
        app.waiting_on_assistant = true;
        let action = app.run_slash_command("loop do something");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("still working on the previous request")));
    }

    #[test]
    fn loop_iteration_started_updates_progress_and_transcript() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::LoopIterationStarted {
            iteration: 2,
            max_iterations: 25,
        });
        assert_eq!(app.loop_progress, Some((2, 25)));
        assert!(matches!(app.phase, AgentPhase::Looping));
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("loop iteration 2/25")));
    }

    #[test]
    fn assistant_turn_complete_mid_loop_does_not_reset_waiting_flag() {
        let mut app = test_app();
        app.loop_active = true;
        app.waiting_on_assistant = true;
        app.phase = AgentPhase::Looping;
        app.on_agent_event(AgentEvent::AssistantTurnComplete {
            message: smith_core::Message::assistant(vec![smith_core::ContentBlock::Text {
                text: "iteration one done".to_string(),
            }]),
            stop_reason: StopReason::EndTurn,
        });
        assert!(app.waiting_on_assistant);
        assert!(matches!(app.phase, AgentPhase::Looping));
    }

    #[test]
    fn loop_finished_done_resets_state_and_reports_iterations() {
        let mut app = test_app();
        app.loop_active = true;
        app.waiting_on_assistant = true;
        app.loop_progress = Some((3, 25));
        app.phase = AgentPhase::Looping;
        app.on_agent_event(AgentEvent::LoopFinished {
            reason: smith_core::LoopStopReason::Done,
            iterations: 3,
        });
        assert!(!app.loop_active);
        assert!(app.loop_progress.is_none());
        assert!(!app.waiting_on_assistant);
        assert!(matches!(app.phase, AgentPhase::Idle));
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("loop finished") && l.text.contains("3")));
    }

    #[test]
    fn loop_finished_cancelled_reports_cancellation() {
        let mut app = test_app();
        app.loop_active = true;
        app.on_agent_event(AgentEvent::LoopFinished {
            reason: smith_core::LoopStopReason::Cancelled,
            iterations: 1,
        });
        assert!(app.lines.iter().any(|l| l.text.contains("loop cancelled")));
    }

    #[test]
    fn question_modal_digit_one_submits_option_a() {
        let mut app = test_app();
        app.modal = Modal::Question(QuestionModal {
            question: UserQuestion {
                id: "q1".into(),
                prompt: "Which approach?".into(),
                options: ["Alpha".into(), "Beta".into(), "Gamma".into()],
            },
            selected: 0,
            custom: String::new(),
        });
        let action = app.on_key(
            crossterm::event::KeyCode::Char('1'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert!(matches!(
            action,
            Some(Action::QuestionResponse(ref s)) if s == "Alpha"
        ));
        assert!(app.modal.is_none());
    }

    #[test]
    fn phase_changed_updates_label() {
        let mut app = test_app();
        assert_eq!(app.phase_label(), "idle");
        app.on_agent_event(AgentEvent::PhaseChanged(AgentPhase::Thinking));
        assert_eq!(app.phase_label(), "thinking…");
        app.on_agent_event(AgentEvent::PhaseChanged(AgentPhase::Building));
        assert_eq!(app.phase_label(), "building…");
        assert!(app.is_animating());
    }

    #[test]
    fn building_phase_survives_thinking_event() {
        let mut app = test_app();
        app.phase = AgentPhase::Building;
        app.on_agent_event(AgentEvent::PhaseChanged(AgentPhase::Thinking));
        assert_eq!(app.phase, AgentPhase::Building);
    }

    fn ctrl_c(app: &mut App) -> Option<Action> {
        app.on_key(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        )
    }

    fn question_modal() -> Modal {
        Modal::Question(QuestionModal {
            question: UserQuestion {
                id: "q1".into(),
                prompt: "Which approach?".into(),
                options: ["Alpha".into(), "Beta".into(), "Gamma".into()],
            },
            selected: 0,
            custom: String::new(),
        })
    }

    fn permission_modal() -> Modal {
        Modal::Permission(PermissionModal {
            request: PermissionRequest {
                tool_call_id: "call_1".into(),
                tool_name: "run_bash".into(),
                detail: "rm -rf build".into(),
            },
            scroll: 0,
        })
    }

    #[test]
    fn ctrl_c_with_a_question_modal_open_arms_the_quit_instead_of_typing_c() {
        // The modal branch used to swallow it via `Char(c) if !c.is_control()`.
        let mut app = test_app();
        app.modal = question_modal();
        assert!(ctrl_c(&mut app).is_none());
        assert_eq!(app.modal.question().unwrap().custom, "");
        assert_eq!(app.modal.question().unwrap().selected, 0);
        assert!(app.quit_pending());
        assert!(!app.should_quit);
    }

    #[test]
    fn ctrl_c_with_plan_or_permission_modal_open_arms_the_quit() {
        // Both branches used to fall through to `_ => None`: no way out at all.
        for modal in [
            Modal::Plan(PlanModal {
                text: "step 1".into(),
                scroll: 0,
            }),
            permission_modal(),
        ] {
            let mut app = test_app();
            app.modal = modal;
            assert!(ctrl_c(&mut app).is_none());
            assert!(app.quit_pending());
            assert!(app.modal.is_some(), "the modal must stay up until we quit");
            assert!(matches!(ctrl_c(&mut app), Some(Action::Quit)));
            assert!(app.should_quit);
        }
    }

    #[test]
    fn quitting_takes_two_ctrl_c_presses() {
        let mut app = test_app();
        assert!(ctrl_c(&mut app).is_none());
        assert!(!app.should_quit, "one press must never discard the session");
        assert!(matches!(ctrl_c(&mut app), Some(Action::Quit)));
        assert!(app.should_quit);
    }

    #[test]
    fn any_other_key_disarms_a_pending_quit() {
        let mut app = test_app();
        ctrl_c(&mut app);
        app.on_key(
            crossterm::event::KeyCode::Char('h'),
            crossterm::event::KeyModifiers::NONE,
        );
        assert!(!app.quit_pending());
        assert!(ctrl_c(&mut app).is_none(), "this is a fresh first press");
        assert!(!app.should_quit);
    }

    #[test]
    fn a_stale_arm_expires_instead_of_pairing_with_a_later_press() {
        let mut app = test_app();
        app.quit_armed_at = Some(Instant::now() - QUIT_CONFIRM_WINDOW - Duration::from_secs(1));
        assert!(!app.quit_pending());
        assert!(app.expire_pending_quit(), "the lapsed hint needs a repaint");
        assert!(!app.expire_pending_quit(), "only once");
        assert!(ctrl_c(&mut app).is_none());
        assert!(!app.should_quit);
    }

    // ---- /rewind -------------------------------------------------------------

    /// The safety property of the command surface: typing `/rewind` on its own
    /// must never be able to overwrite a file.
    #[test]
    fn a_bare_rewind_asks_for_a_plan_and_never_applies_one() {
        let mut app = test_app();
        match app.run_slash_command("rewind") {
            Some(Action::Rewind { turn, apply, force }) => {
                assert_eq!(turn, None);
                assert!(!apply, "a bare /rewind must not apply anything");
                assert!(!force);
            }
            other => panic!("expected a Rewind action, got {other:?}"),
        }
    }

    #[test]
    fn rewind_parses_a_turn_number_confirm_and_force_in_any_order() {
        let mut app = test_app();
        match app.run_slash_command("rewind --force 7 confirm") {
            Some(Action::Rewind { turn, apply, force }) => {
                assert_eq!(turn, Some(7));
                assert!(apply);
                assert!(force);
            }
            other => panic!("expected a Rewind action, got {other:?}"),
        }
    }

    #[test]
    fn rewind_with_an_unparseable_argument_explains_itself_instead_of_guessing() {
        let mut app = test_app();
        assert!(app.run_slash_command("rewind yesterday").is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("usage: /rewind")));
    }

    /// A checkpoint for a turn still running is incomplete, so undoing half of
    /// it would be worse than not offering.
    #[test]
    fn rewind_is_refused_mid_turn() {
        let mut app = test_app();
        app.waiting_on_assistant = true;
        assert!(app.run_slash_command("rewind confirm").is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("can't rewind")));
    }

    /// The `run_bash` caveat has to survive the trip through the event channel
    /// and land in the transcript — it is the one line that stops a user
    /// believing the rewind was total.
    #[test]
    fn a_rewind_report_lands_in_the_transcript_caveats_and_all() {
        let mut app = test_app();
        app.on_agent_event(AgentEvent::Rewind(smith_core::RewindReport {
            turn: Some(3),
            status: smith_core::RewindStatus::Preview,
            restore: vec!["src/main.rs".into()],
            delete: Vec::new(),
            conflicts: Vec::new(),
            uncovered: vec![("run_bash".into(), 1)],
            notes: Vec::new(),
        }));

        let text: String = app
            .lines
            .iter()
            .map(|l| l.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("rewind of turn 3 would"), "{text}");
        assert!(text.contains("restore src/main.rs"), "{text}");
        assert!(text.contains("NOT COVERED"), "{text}");
        assert!(text.contains("/rewind 3 confirm"), "{text}");
    }
}
