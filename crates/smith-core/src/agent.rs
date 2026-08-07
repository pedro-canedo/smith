use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;

use crate::event::Task;
use crate::hooks::{HookContext, HookSet};
use crate::message::{ContentBlock, Message, Usage};
use crate::provider::LlmProvider;
use crate::redact::Redactor;
use crate::retry::RetryPolicy;
use crate::subagent::{self, SubagentDefinition};
use crate::tool::{PermissionPolicy, ToolContext};

mod accounting;
mod compaction;
mod executor;
mod fallback;
mod interactive;
mod limits;
mod reasoning;
mod stream;
mod subagents;
mod tools;
mod turn;

pub use compaction::{CompactionConfig, CompactionOutcome, TurnAccounting};
pub use executor::{NoTools, PermissionAsk, QuestionAsk, ToolExecutor};
pub use interactive::parse_tasks;
pub use limits::TurnLimits;

use fallback::find_fallback_tool_call;

/// Suspends the turn for a backoff. Injectable because the alternative is
/// tests that really sleep: the delays are seconds by design, and a suite that
/// waits them out stops being run.
type Sleeper = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// The tools `run_one_tool` handles itself instead of dispatching to
/// `ToolExecutor::execute`.
///
/// Named in one place because two things have to agree about the list: the
/// interception arms below, and the schema check that has to run *before*
/// them precisely because `execute` — where every other call is validated —
/// is never reached.
const INTERCEPTED_TOOLS: &[&str] = &["ask_user", "write_tasks", subagent::TASK_TOOL];

pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<dyn ToolExecutor>,
    model: String,
    system: Option<String>,
    max_tokens: u32,
    messages: Vec<Message>,
    allowed_session_tools: HashSet<String>,
    tool_ctx: ToolContext,
    permission_policy: PermissionPolicy,
    /// While true, any tool above `ReadOnly` is blocked outright — set by
    /// `/plan` while a proposed plan awaits `/plan approve` (or `/plan
    /// reject`, which also clears it). Independent of `permission_policy`:
    /// even `skip` mode doesn't bypass an unapproved plan.
    plan_gated: bool,
    /// Set via `/goal`; folded into the system prompt on every request so
    /// the model keeps the session's objective in view.
    goal: Option<String>,
    /// Supplies environment context (today's date, and whatever else the
    /// frontend knows) that gets appended to the system prompt on *every*
    /// request. A closure rather than a string because the wall clock moves:
    /// a session left open overnight, or a long `/loop`, must not keep
    /// telling the model it's still yesterday. Injected rather than read
    /// here so `smith-core` stays free of environment knowledge — and so
    /// `effective_system` stays deterministically testable.
    context_provider: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    /// The agent's current checklist, replaced wholesale on each `write_tasks`
    /// call — see `AgentEvent::TasksUpdated`.
    tasks: Vec<Task>,
    /// Strips known API keys out of tool output before it reaches anything
    /// that keeps or forwards it. Empty by default so `smith-core` has no
    /// opinion about which secrets exist — the frontend, which loaded them,
    /// supplies the list.
    redactor: Redactor,
    limits: TurnLimits,
    retry_policy: RetryPolicy,
    sleeper: Sleeper,
    compaction: CompactionConfig,
    /// Usage from the most recent provider response, and how many messages
    /// were in history when it arrived.
    ///
    /// These two together are what make context tracking cheap *and* accurate:
    /// `input_tokens` is the provider's own exact count of everything it was
    /// sent — system prompt, tool definitions, the lot — so the only thing
    /// left to estimate is whatever has been appended since. `None` before the
    /// first response of a session, when there is nothing but estimate.
    last_usage: Option<Usage>,
    counted_messages: usize,
    /// Cumulative usage and cost for the whole session, including anything
    /// restored from a resumed one via `seed_session_totals`.
    session_usage: Usage,
    session_cost_usd: f64,
    /// Rounds billed against a model with no price in the table. Reported
    /// beside the total so "$0.00" and "we have no idea" stay distinguishable
    /// — the former is a claim about money, the latter about our own data.
    unpriced_turns: u32,
    /// Accounting for the turn currently running (or the last one that did).
    last_turn: Option<TurnAccounting>,
    /// Snapshots files before a mutating tool overwrites them, so `/rewind`
    /// has something to restore. `None` disables checkpointing entirely —
    /// which is the correct behaviour, not a degraded one, for a caller that
    /// has nowhere to put the objects.
    checkpointer: Option<Arc<dyn crate::checkpoint::Checkpointer>>,
    /// Sequence number of the turn in flight, allocated by the checkpointer at
    /// the top of `run_turn`. `None` when there is no checkpointer.
    turn_seq: Option<u64>,
    /// Facts the agent must tell the model before its next turn, prepended to
    /// that turn's user message.
    ///
    /// Not pushed into history as messages of their own: after a completed
    /// turn the last message is the assistant's, so a lone user message would
    /// be followed by the real one and leave two consecutive user messages —
    /// a shape some providers reject and others silently merge. Riding the
    /// next real message has none of that risk and reaches the model at
    /// exactly the same moment.
    pending_notes: Vec<String>,
    /// How many reasoning tags [`ReasoningFilter`] has removed from this
    /// session's replies.
    ///
    /// The reasoning itself is discarded rather than surfaced — there is no
    /// `AgentEvent` for it yet, and inventing one is a bigger change than
    /// "stop corrupting the transcript" warrants. Counting it is the honest
    /// minimum: the fact that a model is reasoning in the text channel is
    /// observable, and whoever adds a thinking pane later has the hook.
    reasoning_tags_stripped: u32,
    /// How many agents deep this one is: 0 for the one the user talks to, 1
    /// for a child it spawned via `task`. See [`subagent::MAX_DEPTH`].
    subagent_depth: u32,
    /// Messages the user typed while this turn was already running.
    ///
    /// Shared with whoever drives the agent, because the driver cannot reach
    /// the `Agent` mid-turn: the orchestrator holds it behind a `Mutex` that
    /// stays locked for the whole of `run_turn`. Same shape as the cancel
    /// token, and for the same reason.
    interjections: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    /// True when no human is watching — a headless run. See the two places
    /// that read it: the scratch-write exemption and the `task` gate, both of
    /// which are justified by interactive friction that does not exist here.
    unattended: bool,
    /// Child agents this one may spawn, beyond the built-in general-purpose
    /// one. Loaded from `~/.smith/agents/*.md` by the frontend — `smith-core`
    /// has no notion of a home directory.
    subagent_definitions: Vec<SubagentDefinition>,
    /// Tool calls *all* subagents spawned in the current turn may make between
    /// them, refilled at the top of every `run_turn`.
    ///
    /// One shared pool rather than a per-child cap, because per-child caps
    /// multiply: fifty parent tool calls each spawning a thirty-call child is
    /// fifteen hundred tool calls from a turn whose stated budget was fifty.
    /// A pool makes the worst case additive — a turn can spend at most twice
    /// its own tool-call cap in total — and bounded by a number the user
    /// already set.
    subagent_tool_budget: u32,
    /// When the turn in flight runs out of wall clock, so a child can be given
    /// what is actually left rather than a fresh full allowance.
    turn_deadline: Option<Instant>,
    /// User-configured shell commands run at three fixed points — see
    /// [`crate::hooks`] and `docs/hooks.md`.
    ///
    /// Behind an `Arc` and used from `&self`, because the read-only tool path
    /// dispatches concurrently: a hook set that needed `&mut self` would have
    /// silently excluded exactly the calls a logging hook most wants to see.
    /// Empty by default, and empty is a hard short-circuit — a session with no
    /// hooks configured pays nothing at all.
    hooks: Arc<HookSet>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Arc<dyn ToolExecutor>,
        model: String,
        tool_ctx: ToolContext,
    ) -> Self {
        Self {
            provider,
            tools,
            model,
            system: None,
            max_tokens: 4096,
            messages: Vec::new(),
            allowed_session_tools: HashSet::new(),
            tool_ctx,
            permission_policy: PermissionPolicy::default(),
            plan_gated: false,
            goal: None,
            context_provider: None,
            tasks: Vec::new(),
            redactor: Redactor::default(),
            limits: TurnLimits::default(),
            retry_policy: RetryPolicy::default(),
            sleeper: Arc::new(|d| Box::pin(tokio::time::sleep(d))),
            compaction: CompactionConfig::default(),
            last_usage: None,
            counted_messages: 0,
            session_usage: Usage::default(),
            session_cost_usd: 0.0,
            unpriced_turns: 0,
            last_turn: None,
            checkpointer: None,
            turn_seq: None,
            pending_notes: Vec::new(),
            reasoning_tags_stripped: 0,
            subagent_depth: 0,
            unattended: false,
            interjections: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            subagent_definitions: Vec::new(),
            subagent_tool_budget: 0,
            turn_deadline: None,
            hooks: Arc::new(HookSet::empty()),
        }
    }

    /// Attaches the user's configured hooks. See `docs/authorization.md` for
    /// where `PreToolUse` sits relative to the plan gate and the permission
    /// prompt, and why it can only ever subtract authority.
    pub fn with_hooks(mut self, hooks: Arc<HookSet>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn hooks(&self) -> &Arc<HookSet> {
        &self.hooks
    }

    /// Who the hooks are being run on behalf of. Rebuilt per call rather than
    /// stored, because `cwd` and the session id are the agent's own and a
    /// stale copy would misreport them to a policy hook.
    fn hook_ctx(&self) -> HookContext {
        HookContext::new(
            self.tool_ctx.session_id.clone(),
            self.tool_ctx.cwd.clone(),
            self.subagent_depth,
        )
    }

    /// Subagent definitions loaded from disk, on top of the built-in
    /// general-purpose one (which is always available and cannot be shadowed
    /// — a definition that took its name would silently redefine the default
    /// every `task` call gets).
    pub fn with_subagent_definitions(
        mut self,
        definitions: impl IntoIterator<Item = SubagentDefinition>,
    ) -> Self {
        self.subagent_definitions = definitions
            .into_iter()
            .filter(|d| d.name != subagent::GENERAL_PURPOSE)
            .collect();
        self
    }

    /// Exposed so a `/model` switch — which rebuilds the whole `Agent` — keeps
    /// the definitions it already loaded.
    pub fn subagent_definitions(&self) -> &[SubagentDefinition] {
        &self.subagent_definitions
    }

    /// 0 for the agent the user talks to; 1 inside a subagent.
    /// The queue a driver pushes mid-turn user messages onto.
    ///
    /// Handed out rather than written through, because `run_turn` borrows the
    /// agent mutably for its whole duration — a caller holding a clone of this
    /// can speak to a turn that is already in flight.
    pub fn interjection_queue(&self) -> Arc<std::sync::Mutex<std::collections::VecDeque<String>>> {
        self.interjections.clone()
    }

    /// Takes whatever the user said since the last round, if anything.
    ///
    /// A poisoned lock yields nothing rather than panicking: losing an
    /// interjection is recoverable — the user can say it again — and killing
    /// the turn is not.
    fn take_interjections(&self) -> Vec<String> {
        let Ok(mut queue) = self.interjections.lock() else {
            return Vec::new();
        };
        queue.drain(..).collect()
    }

    /// Marks this agent as running with nobody at the terminal.
    pub fn with_unattended(mut self, unattended: bool) -> Self {
        self.unattended = unattended;
        self
    }

    pub fn unattended(&self) -> bool {
        self.unattended
    }

    pub fn subagent_depth(&self) -> u32 {
        self.subagent_depth
    }

    pub fn with_checkpointer(
        mut self,
        checkpointer: Arc<dyn crate::checkpoint::Checkpointer>,
    ) -> Self {
        self.checkpointer = Some(checkpointer);
        self
    }

    /// Exposed so a `/model` switch — which rebuilds the whole `Agent` — keeps
    /// checkpointing instead of silently dropping it for the rest of the
    /// session.
    pub fn checkpointer(&self) -> Option<Arc<dyn crate::checkpoint::Checkpointer>> {
        self.checkpointer.clone()
    }

    /// Queues a fact for the model to read at the start of its next turn.
    ///
    /// The one caller today is `/rewind`: files the model believes it wrote
    /// have been put back, and a model that does not know that will happily
    /// build on edits that no longer exist.
    pub fn note_to_model(&mut self, note: impl Into<String>) {
        self.pending_notes.push(note.into());
    }

    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    pub fn with_limits(mut self, limits: TurnLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.limits.max_turns = max_turns;
        self
    }

    pub fn with_max_tool_calls_per_turn(mut self, max_tool_calls: u32) -> Self {
        self.limits.max_tool_calls_per_turn = max_tool_calls;
        self
    }

    pub fn with_max_wall_clock(mut self, max_wall_clock: Duration) -> Self {
        self.limits.max_wall_clock = max_wall_clock;
        self
    }

    /// The per-request completion cap. Separate from [`TurnLimits`] because it
    /// bounds one *response*, not the turn: it is a provider parameter that
    /// travels on every request.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Replaces the backoff sleep. Only tests should need this; production
    /// wants the default, which is `tokio::time::sleep`.
    pub fn with_sleeper(
        mut self,
        sleeper: impl Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Self {
        self.sleeper = Arc::new(sleeper);
        self
    }

    /// Exposed so a `/model` switch — which rebuilds the whole `Agent` — can
    /// carry these over instead of silently resetting them to the defaults.
    pub fn limits(&self) -> TurnLimits {
        self.limits
    }

    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }

    pub fn with_compaction(mut self, compaction: CompactionConfig) -> Self {
        self.compaction = compaction;
        self
    }

    pub fn compaction(&self) -> CompactionConfig {
        self.compaction
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_context_provider(
        mut self,
        provider: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        self.context_provider = Some(Arc::new(provider));
        self
    }

    pub fn with_permission_policy(mut self, policy: PermissionPolicy) -> Self {
        self.permission_policy = policy;
        self
    }

    pub fn permission_policy(&self) -> PermissionPolicy {
        self.permission_policy
    }

    pub fn set_permission_policy(&mut self, policy: PermissionPolicy) {
        self.permission_policy = policy;
    }

    pub fn plan_gated(&self) -> bool {
        self.plan_gated
    }

    pub fn set_plan_gated(&mut self, gated: bool) {
        self.plan_gated = gated;
    }

    pub fn goal(&self) -> Option<&str> {
        self.goal.as_deref()
    }

    pub fn set_goal(&mut self, goal: Option<String>) {
        self.goal = goal;
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// Restores the checklist from a resumed session's history (the last
    /// `write_tasks` call) so the TUI doesn't start blank on `--resume`.
    pub fn seed_tasks(&mut self, tasks: Vec<Task>) {
        self.tasks = tasks;
    }

    /// Tools the user has approved for the rest of the session ("allow
    /// always"). Exposed so a `/model` switch — which rebuilds the whole
    /// `Agent` — can carry the grants over instead of silently revoking
    /// them and re-prompting for things the user already approved.
    pub fn allowed_session_tools(&self) -> &HashSet<String> {
        &self.allowed_session_tools
    }

    pub fn seed_allowed_session_tools(&mut self, allowed: HashSet<String>) {
        self.allowed_session_tools = allowed;
    }

    pub fn tool_ctx(&self) -> &ToolContext {
        &self.tool_ctx
    }

    /// Reasoning tags removed from this session's replies — see the field.
    /// Non-zero means the model is emitting `<think>` blocks in its text
    /// channel and they were kept out of the transcript.
    pub fn reasoning_tags_stripped(&self) -> u32 {
        self.reasoning_tags_stripped
    }

    /// If `message` is a single, otherwise-final text reply that's actually a
    /// tool call in disguise — `{"name": ..., "arguments": {...}}` or the flat
    /// `{"action": "<tool>", ...}` form, resolving to exactly one *registered*
    /// tool via [`resolve_tool_name`], not just JSON-shaped text — rebuild it
    /// as a real `ToolUse` so the normal tool-execution path picks it up.
    /// `None` for anything that doesn't match, which is the overwhelming
    /// majority of replies — this only exists for providers/models that fall
    /// back to printing the call instead of using the structured channel.
    fn recover_text_tool_call(&self, message: &Message) -> Option<Message> {
        let [ContentBlock::Text { text }] = message.content.as_slice() else {
            return None;
        };
        let known = self.tools.tool_defs();
        let (name, arguments, before, after) = find_fallback_tool_call(text, &known)?;

        let mut content = Vec::new();
        if !before.is_empty() {
            content.push(ContentBlock::Text { text: before });
        }
        content.push(ContentBlock::ToolUse {
            id: "fallback-1".to_string(),
            name,
            input: arguments,
        });
        if !after.is_empty() {
            content.push(ContentBlock::Text { text: after });
        }
        Some(Message::assistant(content))
    }

    /// The base system prompt with environment context and the current goal
    /// folded in — what actually gets sent on each request.
    ///
    /// Order matters: the static prompt stays first so providers that cache
    /// on the system block's prefix keep hitting that cache. The volatile
    /// segments (date, goal) go behind it.
    fn effective_system(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(system) = &self.system {
            parts.push(system.clone());
        }
        if let Some(provider) = &self.context_provider {
            let context = provider();
            if !context.trim().is_empty() {
                parts.push(context);
            }
        }
        if let Some(goal) = &self.goal {
            let mut text = format!(
                "Current session goal: {goal}\nKeep this goal in mind and work toward it unless the user directs you otherwise."
            );
            // The `goal` skill carries the tracking workflow (milestones,
            // write_tasks, drift flagging). Conditioned on the tool actually
            // being registered so this never names a tool that does not
            // exist; the sentence itself is static, so the only volatility
            // in this segment remains the goal text it already carries.
            if self.tools.permission_class("skill").is_some() {
                text.push_str(
                    "\nIf you have not already this session, load the `goal` skill with the `skill` tool for how to track and report progress against this goal."
                );
            }
            parts.push(text);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    /// Preloads prior conversation history (e.g. when resuming a saved
    /// session) before the first `run_turn` call.
    pub fn seed_history(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        // Nothing here was measured by *this* agent's provider, so the whole
        // history is unsent delta until the first response comes back. Leaving
        // these stale would make a resumed session report the previous
        // agent's context occupancy.
        self.last_usage = None;
        self.counted_messages = 0;
    }
}

#[cfg(test)]
mod tests;
