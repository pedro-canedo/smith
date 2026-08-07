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

/// The activity a tool's card belongs to, for the tools whose card is pure
/// status.
///
/// Two calls share a card when they share a *class*, not when they share a
/// name. A research burst alternates `web_search` and `web_fetch`, so keying
/// on the name gave it one card per call — the stack this exists to remove.
///
/// `None` means the tool always gets its own card, which is deliberate for
/// everything else: a `read_file` or `edit_file` card carries a diff or an
/// error tail, and folding those hides the thing you opened the card for.
fn group_class(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "web_search" | "web_fetch" => Some("research"),
        _ => None,
    }
}

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

/// The two numbers a grouped card's header carries — see
/// `ChatLine::group_summary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSummary {
    /// Calls this card stands for, its own included.
    pub steps: usize,
    /// How many of them failed.
    pub failed: usize,
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
        let class = group_class(name);
        self.role == ChatRole::Tool
            && class.is_some()
            && group_class(self.tool_name.as_deref().unwrap_or_default()) == class
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

    /// The calls folded into this card *after* its own — see `group_summary`
    /// for the count that includes its own.
    pub fn grouped(&self) -> &[GroupedCall] {
        &self.grouped
    }

    /// How this card's own call ended, as opposed to the group's verdict.
    /// `None` while it is still running.
    pub fn own_status(&self) -> Option<ActivityStatus> {
        self.own_status
    }

    /// What a grouped card's header says in place of a target: how many calls
    /// it stands for, and how many of them failed.
    ///
    /// Counts this card's own call first — `grouped` holds only the ones
    /// folded in after it, so a card with three siblings stands for four
    /// steps.
    pub fn group_summary(&self) -> GroupSummary {
        let statuses =
            std::iter::once(self.own_status).chain(self.grouped.iter().map(|c| Some(c.status)));
        let mut summary = GroupSummary {
            steps: 0,
            failed: 0,
        };
        for status in statuses {
            summary.steps += 1;
            if status == Some(ActivityStatus::Error) {
                summary.failed += 1;
            }
        }
        summary
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
    /// Running while anything under it is. Failed only when *nothing* got
    /// through: a research burst that lost two fetches to a 404 while eight
    /// other steps answered is not a failed research, and marking the card
    /// failed made every real burst look broken. This used to fail on any one
    /// child, to stop a group reading as clean when a search was blocked —
    /// what actually protects that now is the header, which counts the
    /// failures out loud (`GroupSummary::failed`) whether the card is expanded
    /// or not. The fact is still stated; it is no longer stated by mislabelling
    /// the whole activity.
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
        let summary = self.group_summary();
        let failed = summary.failed == summary.steps;
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
}

/// Card constructors that only tests use. They are `ChatLine`'s own, because
/// they set private fields, but they are not `ChatLine`'s behaviour — so they
/// sit in their own block rather than interleaved with the methods that ship.
/// `pub(crate)`, not feature-gated: `ui.rs` and `transcript.rs` build cards to
/// render, and a `#[cfg(test)]` item is visible to the whole crate's own tests.
#[cfg(test)]
impl ChatLine {
    /// Builds an arbitrary tool card for rendering tests.
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
/// The `/model` picker.
///
/// A filter rather than a plain list, because a gateway can list dozens and
/// scrolling to `openrouter/nvidia/nemotron-3-nano-30b-a3b:free` with arrow
/// keys is worse than typing four characters of it.
#[derive(Debug, Clone, Default)]
pub struct ModelPicker {
    pub provider: String,
    /// Everything the provider offered, in the order it offered it.
    pub all: Vec<smith_core::ModelChoice>,
    /// What has been typed to narrow the list.
    pub filter: String,
    /// Index into `matches()`, not into `all`.
    pub selected: usize,
    /// Top row of the visible window, so a long list scrolls.
    pub scroll: usize,
}

impl ModelPicker {
    /// Case-insensitive substring, which is what someone types when they
    /// half remember a name. Sub-sequence matching was considered and rejected:
    /// it turns `gpt` into a match for `google/gemma-4-31b-it`, and a picker
    /// that surprises you is worse than one that finds less.
    pub fn matches(&self) -> Vec<&smith_core::ModelChoice> {
        let needle = self.filter.trim().to_ascii_lowercase();
        self.all
            .iter()
            .filter(|m| needle.is_empty() || m.id.to_ascii_lowercase().contains(&needle))
            .collect()
    }

    pub fn selected_id(&self) -> Option<String> {
        self.matches().get(self.selected).map(|m| m.id.clone())
    }

    /// Keeps the cursor inside the filtered list and the window around the
    /// cursor. Called after anything that can change either.
    pub fn clamp(&mut self, visible: usize) {
        let len = self.matches().len();
        if len == 0 {
            self.selected = 0;
            self.scroll = 0;
            return;
        }
        self.selected = self.selected.min(len - 1);
        let visible = visible.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        self.scroll = self.scroll.min(len.saturating_sub(1));
    }
}

#[derive(Debug, Clone, Default)]
pub enum Modal {
    #[default]
    None,
    Permission(PermissionModal),
    Plan(PlanModal),
    Question(QuestionModal),
    Model(ModelPicker),
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

    pub fn is_model(&self) -> bool {
        matches!(self, Modal::Model(_))
    }

    pub fn model(&self) -> Option<&ModelPicker> {
        match self {
            Modal::Model(m) => Some(m),
            _ => None,
        }
    }

    pub fn model_mut(&mut self) -> Option<&mut ModelPicker> {
        match self {
            Modal::Model(m) => Some(m),
            _ => None,
        }
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

        // Before every other modal: the picker owns the keyboard while it is
        // open, including plain characters, which are its filter rather than
        // prompt text.
        if self.modal.is_model() {
            match code {
                KeyCode::Esc => {
                    self.modal = Modal::None;
                    return None;
                }
                KeyCode::Enter => {
                    let picked = self.modal.model().and_then(|m| m.selected_id());
                    self.modal = Modal::None;
                    return picked.map(|model| Action::SwitchModel {
                        provider: None,
                        model,
                        // A pick is for this session. Persisting silently
                        // would make a keystroke outlive the conversation it
                        // was meant for; `/model <name> --save` is the way to
                        // say otherwise, and it still works.
                        save: false,
                    });
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Backspace | KeyCode::Char(_) => {
                    if let Some(m) = self.modal.model_mut() {
                        match code {
                            KeyCode::Up => m.selected = m.selected.saturating_sub(1),
                            KeyCode::Down => m.selected = m.selected.saturating_add(1),
                            KeyCode::Backspace => {
                                m.filter.pop();
                                // A narrower list can leave the cursor past
                                // the end; the renderer clamps, but the state
                                // must not be nonsense in the meantime.
                                m.selected = 0;
                            }
                            KeyCode::Char(c) => {
                                m.filter.push(c);
                                m.selected = 0;
                            }
                            _ => {}
                        }
                    }
                    return None;
                }
                _ => return None,
            }
        }

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
        if args.is_empty() {
            // Ask for the catalogue; the picker opens when it arrives. The
            // frontend cannot fetch it — `smith-tui` does not depend on
            // `smith-provider`, and it is a network call besides.
            self.lines
                .push(ChatLine::new(ChatRole::System, "reading the model list…"));
            return Some(Action::ListModels);
        }
        if args.eq_ignore_ascii_case("list") {
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
                if smith_store::is_known_provider(p) {
                    (Some(p.to_string()), m.to_string())
                } else if matches!(self.provider_label.as_str(), "openrouter" | "9router") {
                    // Model ids on these providers contain slashes
                    // (`qwen/qwen3-coder:free`), so under an active gateway
                    // session an unknown prefix is a *namespace*, not a typo'd
                    // provider — `/model qwen/qwen3-coder:free` must work.
                    // Under any other provider the old strictness stands: a
                    // typo must not silently become a model name.
                    (None, spec.to_string())
                } else {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        format!(
                            "unknown provider: {p} (expected anthropic, openai, openrouter, \
                             9router, or ollama)"
                        ),
                    ));
                    return None;
                }
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
                    // Calls of one activity fold into a single card rather
                    // than stacking — `join_groupable_card` owns what keeps a
                    // run open and what closes it.
                    match self.join_groupable_card(&tool_name) {
                        Some(index) => {
                            // The child's row carries only its target: the
                            // header above it already says the activity, and
                            // repeating "Searching the web…" per row is the
                            // noise this exists to remove.
                            self.lines[index].group(id.clone(), group_target(&tool_name, &input));
                        }
                        None => {
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
            AgentEvent::ModelsAvailable { provider, models } => {
                if models.is_empty() {
                    self.lines.push(ChatLine::new(
                        ChatRole::System,
                        format!(
                            "could not read {provider}'s model list — switch by name: \
                             /model <name> [--save]"
                        ),
                    ));
                    return;
                }
                let selected = models
                    .iter()
                    .position(|m| m.id == self.model_label)
                    .unwrap_or(0);
                self.modal = Modal::Model(ModelPicker {
                    provider,
                    all: models,
                    filter: String::new(),
                    selected,
                    scroll: 0,
                });
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

/// The header of a card standing for a whole run of calls.
///
/// Not `tool_labels`, which speaks for exactly one call: "Search completed" is
/// wrong on a card holding six searches and four page fetches, and picking the
/// first call's wording makes the header change meaning depending on which
/// tool happened to start the run.
pub(crate) fn group_labels(tool_name: &str) -> ToolLabels {
    let (running, done, failed) = match group_class(tool_name) {
        Some("research") => ("Researching the web…", "Research", "Research failed"),
        // Unreachable while `group_class` has one class, and a plain fallback
        // rather than a panic so adding a class can never take the UI down.
        _ => return tool_labels(tool_name),
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
mod tests;
