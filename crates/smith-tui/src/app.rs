use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use smith_core::{
    Action, AgentEvent, AgentPhase, PermissionDecision, PermissionPolicy, PermissionRequest,
    ResourceStats, StopReason, Task, Usage, UserQuestion,
};

use crate::components::input::TextInput;
use crate::theme::Theme;
use crate::transcript::TranscriptCache;

pub const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", ""];
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
    /// while the card is still `Running`.
    started_at: Option<Instant>,
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

    /// Records a tool call's outcome, on the `ToolCallResult` that ends it.
    fn finish_tool(&mut self, status: ActivityStatus, output: String) {
        self.tool_status = Some(status);
        self.tool_output = Some(output);
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
    /// Loaded from `.smith/goal.md` at startup (if any).
    pub goal: Option<String>,
    /// Restored from a resumed session's last `write_tasks` call, if any.
    pub tasks: Vec<Task>,
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
    /// When true, tool cards expand to show full input + output + diffs.
    pub verbose_tools: bool,
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
        let theme = Theme::detect();
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
            thinking_since: None,
            provider_label: config.provider_label,
            model_label: config.model_label,
            cwd_display: config.cwd_display,
            git_branch: config.git_branch,
            idle_hint: config.idle_hint,
            usage: Usage::default(),
            context: None,
            resources: None,
            permission_policy: config.permission_policy,
            request_count: 0,
            tool_call_count: 0,
            plan_gated: false,
            plan_turn_active: false,
            slash_selected: 0,
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

    /// Slash-command suggestions for the current input (empty when not typing `/cmd`).
    pub fn slash_suggestions(&self) -> Vec<crate::slash::SlashSuggestion> {
        crate::slash::suggestions_for(&self.input.text())
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
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
    }

    pub fn is_animating(&self) -> bool {
        self.waiting_on_assistant || self.modal.is_some() || !matches!(self.phase, AgentPhase::Idle)
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

        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('o') {
            self.verbose_tools = !self.verbose_tools;
            return None;
        }

        let slash_hints = self.slash_suggestions();
        let slash_nav = !slash_hints.is_empty();

        match code {
            KeyCode::Esc if self.waiting_on_assistant => Some(Action::CancelGeneration),
            KeyCode::Tab if slash_nav => {
                if let Some(completed) =
                    crate::slash::complete(&self.input.text(), Some(self.slash_selected))
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
                let max = slash_hints.len().saturating_sub(1);
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
            KeyCode::Char('j')
                if modifiers.contains(KeyModifiers::CONTROL) && !self.waiting_on_assistant =>
            {
                self.input.insert_newline();
                None
            }
            // Arrows walk the caret through a multi-line prompt first; they
            // only fall through to scrolling the transcript once the caret is
            // already at the top/bottom of the input — which is always the
            // case for the single-row prompt this used to be.
            KeyCode::Up if self.input.move_up() => None,
            KeyCode::Down if self.input.move_down() => None,
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
                if self.input.text().trim().is_empty() || self.waiting_on_assistant {
                    return None;
                }
                // If a slash suggestion is highlighted and input is still only
                // a partial command, Tab-complete first instead of submitting.
                if slash_nav && !self.input.text().contains(char::is_whitespace) {
                    if let Some(completed) =
                        crate::slash::complete(&self.input.text(), Some(self.slash_selected))
                    {
                        self.input.set(&completed);
                        self.slash_selected = 0;
                        return None;
                    }
                }
                let text = self.input.take();
                self.slash_selected = 0;

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

    fn run_slash_command(&mut self, command: &str) -> Option<Action> {
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
                    "commands: /clear (clear the visible transcript), /model [<name>|<provider>/<name>] [--save] (show or switch model), /permission [ask|session|skip] [--save] (show or set the tool permission policy), /usage (session token/cost/tool-call summary), /plan <task>|approve|reject (plan before executing), /goal [<description>|clear] (set, show, or clear the session goal), /loop [<N>] <task>|goal (repeat a task until done, N iterations, or Esc), /compact (summarise old history to reclaim context), /remember <note> (append a standing note to this project's SMITH.md), /rewind [<turn>] [confirm] [--force] (undo a turn's file writes — shows the plan first; does NOT undo anything run_bash did), /help (this message)",
                ));
                None
            }
            "model" => self.run_model_command(args),
            "permission" => self.run_permission_command(args),
            "usage" => {
                self.show_usage();
                None
            }
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
            other => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("unknown command: /{other}"),
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

    fn show_usage(&mut self) {
        let total_tokens = self.usage.input_tokens + self.usage.output_tokens;
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!(
                "requests: {}   tools invoked: {}",
                self.request_count, self.tool_call_count
            ),
        ));
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!(
                "tokens: {} in / {} out ({total_tokens} total)",
                self.usage.input_tokens, self.usage.output_tokens
            ),
        ));
        match crate::pricing::estimate_cost_usd(
            &self.provider_label,
            &self.model_label,
            &self.usage,
        ) {
            Some(cost) => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!("estimated cost: ~${cost:.4} (est.)"),
                ));
            }
            None => {
                self.lines.push(ChatLine::new(
                    ChatRole::System,
                    format!(
                        "estimated cost: n/a (no pricing data for {}/{})",
                        self.provider_label, self.model_label
                    ),
                ));
            }
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
                    // Permanent transcript record — the tool card replaces
                    // the old activity strip, so this line is all we need.
                    self.lines
                        .push(ChatLine::tool(id.clone(), tool_name.clone(), label, input));
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
                if let Some(line) = self
                    .lines
                    .iter_mut()
                    .find(|l| l.tool_id() == Some(id.as_str()))
                {
                    let status = if is_error {
                        ActivityStatus::Error
                    } else {
                        ActivityStatus::Done
                    };
                    line.finish_tool(status, output.clone());
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
            AgentEvent::ResourceUsage(stats) => {
                self.resources = Some(stats);
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

/// Short, human-readable summary of what a tool call is doing, for the
/// activity widget — e.g. "Reading src/main.rs" rather than a raw JSON blob.
fn activity_label(tool_name: &str, input: &serde_json::Value) -> String {
    let field = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or_default();
    let label = match tool_name {
        "read_file" => format!("Reading {}", field("path")),
        "write_file" => format!("Writing {}", field("path")),
        "edit_file" => format!("Editing {}", field("path")),
        "list_dir" => format!("Listing {}", field("path")),
        "glob" => format!("Searching {}", field("pattern")),
        "grep" => format!("Searching for {}", field("pattern")),
        "multi_edit" => format!("Editing {}", field("path")),
        "run_bash" => format!("Running {}", field("command")),
        "write_tasks" => "Updating task list".to_string(),
        "web_search" => format!("Searching \"{}\"", field("query")),
        other => format!("Calling {other}"),
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

    fn test_app() -> App {
        App::new(TuiConfig {
            banner: String::new(),
            provider_label: "anthropic".to_string(),
            model_label: "claude-sonnet-5".to_string(),
            cwd_display: "~/proj".to_string(),
            git_branch: None,
            idle_hint: IdleHint::Tip("test".to_string()),
            initial_lines: Vec::new(),
            permission_policy: PermissionPolicy::default(),
            goal: None,
            tasks: Vec::new(),
        })
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
    fn usage_reports_requests_tools_and_tokens() {
        let mut app = test_app();
        app.request_count = 2;
        app.tool_call_count = 3;
        app.usage.input_tokens = 1000;
        app.usage.output_tokens = 500;

        let action = app.run_slash_command("usage");
        assert!(action.is_none());
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("requests: 2") && l.text.contains("tools invoked: 3")));
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("1000 in / 500 out (1500 total)")));
    }

    #[test]
    fn usage_shows_cost_estimate_for_known_model() {
        let mut app = test_app(); // anthropic/claude-sonnet-5, has pricing data
        app.usage.input_tokens = 1_000_000;
        app.usage.output_tokens = 1_000_000;
        app.run_slash_command("usage");
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.starts_with("estimated cost: ~$")));
    }

    #[test]
    fn usage_shows_na_for_unknown_pricing() {
        let mut app = test_app();
        app.provider_label = "ollama".to_string();
        app.model_label = "qwen2.5".to_string();
        app.run_slash_command("usage");
        assert!(app
            .lines
            .iter()
            .any(|l| l.text.contains("estimated cost: n/a")));
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

    #[test]
    fn ctrl_o_toggles_verbose_tools() {
        use crossterm::event::{KeyCode, KeyModifiers};
        let mut app = test_app();
        assert!(!app.verbose_tools);
        let action = app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(action.is_none());
        assert!(app.verbose_tools);
        app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(!app.verbose_tools);
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
