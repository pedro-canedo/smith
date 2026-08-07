//! One line of the transcript, and the grouping of repeated tool calls.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::modal::{format_thought, ActivityStatus};
/// Only the `#[cfg(test)]` constructors below take one.
#[cfg(test)]
use std::time::Duration;

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
pub(super) fn group_class(tool_name: &str) -> Option<&'static str> {
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
    pub(super) role: ChatRole,
    pub(super) text: String,
    /// Small dim caption under an assistant reply, e.g. "ollama · qwen3.5 · 4.2s".
    pub(super) meta: Option<String>,
    /// Set only for `ChatRole::Tool` lines — the tool call id, used to find
    /// and update this same line in place when its result arrives.
    pub(super) tool_id: Option<String>,
    /// Set only for `ChatRole::Tool` lines — live while the call is running,
    /// then flipped to its final state so the icon reflects outcome.
    pub(super) tool_status: Option<ActivityStatus>,
    /// Tool name (e.g. `run_bash`, `read_file`) — for the card header.
    tool_name: Option<String>,
    /// Raw JSON input from the provider — used to derive the target summary
    /// and, in verbose mode, to render the full input and a diff for edits.
    tool_input: Option<serde_json::Value>,
    /// Tool output text, populated on `ToolCallResult`.
    pub(super) tool_output: Option<String>,
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
    pub(super) fn tool(
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
    pub(super) fn thought(secs: f32) -> Self {
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
    pub(super) fn toggle_expanded(&mut self) {
        self.expanded = !self.expanded;
        self.touch();
    }

    /// Moves the selection cursor on or off this card. A no-op — including no
    /// new stamp — when the flag already holds the wanted value, so walking
    /// the whole transcript to re-sync the cursor invalidates at most the two
    /// cards that actually changed.
    pub(super) fn set_selected(&mut self, selected: bool) {
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
    pub(super) fn can_group(&self, name: &str) -> bool {
        let class = group_class(name);
        self.role == ChatRole::Tool
            && class.is_some()
            && group_class(self.tool_name.as_deref().unwrap_or_default()) == class
    }

    /// Folds another call of the same tool into this card.
    pub(super) fn group(&mut self, id: String, label: String) {
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
    pub(super) fn finish_grouped(&mut self, id: &str, status: ActivityStatus) -> bool {
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
    pub(super) fn finish_tool(&mut self, status: ActivityStatus, output: String) {
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
    pub(super) fn set_progress(&mut self, line: String) {
        if self.tool_status == Some(ActivityStatus::Running) {
            self.tool_output = Some(line);
        }
    }

    /// Marks a still-running card as failed when the turn dies under it.
    /// A no-op on any other line, so an error never invalidates the whole
    /// transcript's memo.
    pub(super) fn fail_if_running(&mut self) {
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
