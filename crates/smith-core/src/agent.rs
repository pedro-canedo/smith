use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::context::{
    carry_over, compaction_split, estimate_messages_tokens, estimate_tokens, render_transcript,
    ContextUsage, COMPACT_THRESHOLD,
};
use crate::event::{
    AgentEvent, AgentPhase, PermissionDecision, PermissionRequest, ProgressReporter, Task,
    TaskStatus, TurnLimitKind, UserQuestion,
};
use crate::hooks::{HookContext, HookSet};
use crate::message::{
    CompletionRequest, ContentBlock, Message, Role, StopReason, StreamEvent, ToolDefinition, Usage,
};
use crate::permission_detail::format_permission_detail;
use crate::provider::{LlmProvider, ProviderError};
use crate::redact::Redactor;
use crate::retry::RetryPolicy;
use crate::subagent::{self, SubagentDefinition};
use crate::tool::{PermissionClass, PermissionPolicy, ToolContext, ToolResult};

/// Stand-in result recorded for a tool call the turn never got to run. The
/// model reads it, so it says plainly that nothing happened — an empty or
/// vague result would invite it to assume the call succeeded.
const NOT_EXECUTED_CANCELLED: &str = "not executed — the turn was cancelled by the user";

/// The same idea for a call the turn had no budget left to run.
const NOT_EXECUTED_TOOL_BUDGET: &str =
    "not executed — this turn reached its tool-call budget before this call";

/// How many tool calls from one round may be in flight at once.
///
/// Only `ReadOnly` calls are ever run concurrently (see
/// [`Agent::is_concurrency_safe`]), so this bounds reads, globs and greps —
/// work that is dominated by the filesystem, not the CPU.
///
/// Eight, rather than "however many the model asked for":
/// - A model exploring a codebase emits three to six reads in a batch, so
///   eight covers the realistic case without a queue ever forming.
/// - Past roughly eight concurrent traversals a single disk is seek-bound;
///   the extra parallelism buys latency, not throughput, and on a spinning
///   disk actively costs it.
/// - It keeps file-descriptor use trivially bounded. A `glob` over a deep
///   tree holds several directory handles open at once, and an unbounded
///   `join_all` over fifty of them can walk into `EMFILE` against the usual
///   1024 soft limit — a failure mode that shows up as tool calls failing at
///   random, which is far worse than being a little slower.
/// - Eight simultaneously-spinning tool cards is already the most a terminal
///   transcript can show without becoming noise.
const MAX_CONCURRENT_TOOLS: usize = 8;

/// What one entry of a round's execution plan does. The plan is built in the
/// model's own order before anything runs, so neither the tool-call budget
/// nor the grouping can depend on which call happens to finish first.
enum ToolStep {
    /// A maximal contiguous run of concurrency-safe calls, by slot index.
    Concurrent(Vec<usize>),
    /// One call that must run on its own, with `&mut self` available.
    Serial(usize),
    /// A call the turn had no budget left for.
    OverBudget(usize),
}

/// How long one call to `run_turn` may keep going on its own.
///
/// Without these the loop is unbounded in every direction: a model that
/// oscillates between two tool calls, or keeps "just checking one more file",
/// spends the user's money until they notice and kill the process. Each cap
/// covers a different runaway shape, so they are not redundant:
/// `max_turns` bounds *requests*, `max_tool_calls_per_turn` bounds *side
/// effects* (a single round can carry a dozen calls), and `max_wall_clock`
/// bounds the one thing neither of those sees — tools that are individually
/// slow rather than numerous.
///
/// All three are checked only at a round boundary, never mid-round. That is
/// what keeps the `tool_use`/`tool_result` invariant intact and means a cap
/// can never kill a command that is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnLimits {
    /// Tool-call rounds — provider request plus its tool executions — in one
    /// turn.
    pub max_turns: u32,
    /// Individual tool calls in one turn, summed across rounds.
    pub max_tool_calls_per_turn: u32,
    /// Elapsed time since the turn started.
    pub max_wall_clock: Duration,
}

impl Default for TurnLimits {
    /// - **`max_turns` 50**: real coding work routinely takes twenty or thirty
    ///   rounds, so anything much lower would cut off legitimate turns; fifty
    ///   still caps a two-call oscillation at fifty wasted requests instead of
    ///   an unbounded number. This is the cap that actually catches a loop,
    ///   because a loop is fast — it will never reach the wall clock.
    /// - **`max_tool_calls_per_turn` 100**: rounds and calls diverge as soon
    ///   as a model emits calls in parallel, so bounding rounds alone doesn't
    ///   bound side effects. A hundred tool calls is already far more than any
    ///   single user instruction plausibly needs, and being slightly too low
    ///   costs one "continue" — the turn stops cleanly with everything intact.
    /// - **`max_wall_clock` 10 minutes**: the legitimate consumer of wall
    ///   clock is a long `run_bash` (a full workspace build and test suite is
    ///   minutes), and since the check happens between rounds it never
    ///   interrupts one. Ten minutes is roughly where a user is still watching
    ///   an interactive turn; past it, the agent has quietly become a batch
    ///   job nobody asked for.
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_tool_calls_per_turn: 100,
            max_wall_clock: Duration::from_secs(600),
        }
    }
}

impl TurnLimitKind {
    /// One line naming the cap and the value it hit — shown to the user and
    /// folded into what the model is told, so the two never disagree.
    fn describe(self, limits: &TurnLimits) -> String {
        match self {
            TurnLimitKind::Rounds => format!(
                "reached the limit of {} tool-call rounds in one turn",
                limits.max_turns
            ),
            TurnLimitKind::ToolCalls => format!(
                "reached the limit of {} tool calls in one turn",
                limits.max_tool_calls_per_turn
            ),
            TurnLimitKind::WallClock => format!(
                "reached the {}s time limit for one turn",
                limits.max_wall_clock.as_secs()
            ),
        }
    }
}

/// What the model is told about a capped turn.
///
/// It goes into history as a text block on the *same* user message that
/// carries the round's tool results, rather than a message of its own: two
/// consecutive user messages is a shape some providers reject and others
/// silently merge, and there is nothing to gain by risking it. Not a system
/// prompt addition either — the system prompt is a cached prefix and a
/// standing instruction, while this is a one-off fact about one turn.
///
/// The model does not read it now (the turn ends without another request —
/// spending a request to narrate the moment we decided it was overspending
/// would be self-defeating). It reads it on the *next* turn, which is exactly
/// when it matters: the user types "continue" and the model needs to know why
/// it stopped rather than assuming the task was finished.
fn limit_note(kind: TurnLimitKind, limits: &TurnLimits) -> String {
    format!(
        "[smith] This turn was stopped automatically: it {}. \
         Everything already done is intact and nothing else was executed — \
         the task is not necessarily finished. If the user asks you to \
         continue, resume from here.",
        kind.describe(limits)
    )
}

/// Suspends the turn for a backoff. Injectable because the alternative is
/// tests that really sleep: the delays are seconds by design, and a suite that
/// waits them out stops being run.
type Sleeper = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// When and how aggressively history gets compacted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionConfig {
    /// Off entirely. Only tests and `--headless` single-shot runs, where the
    /// turn is short by construction, have a reason to disable it.
    pub enabled: bool,
    /// Fraction of the context window at which auto-compaction fires.
    pub threshold: f32,
    /// How many trailing messages survive untouched. Counted in *messages*,
    /// not exchanges: a tool-heavy round is one assistant message plus one
    /// results message, so eight is roughly the last three or four rounds.
    ///
    /// The real cut point is snapped to a clean user boundary (see
    /// `context::compaction_split`), so this is a target, not a guarantee.
    pub keep_recent: usize,
    /// Cap on the summary the model is asked to write. It has to be small —
    /// a summary that fills the space it just freed is not a compaction.
    pub summary_max_tokens: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: COMPACT_THRESHOLD,
            keep_recent: 8,
            summary_max_tokens: 1024,
        }
    }
}

/// What one call to `run_turn` consumed, and what it cost.
///
/// `cost_usd` is computed **here, when the turn runs**, from the price table
/// in force at that moment — and it is what gets persisted. Storing only the
/// tokens and recomputing later gives a different answer the day a model is
/// repriced or retired, which is exactly the drift `--resume` must not have.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnAccounting {
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    /// `None` when this build has no price for the provider/model — an honest
    /// gap, never a zero pretending to be free.
    pub cost_usd: Option<f64>,
}

/// The result of a successful compaction, for whoever wants to report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionOutcome {
    pub messages_before: usize,
    pub messages_after: usize,
    /// Estimated context tokens before and after, so a caller can say how much
    /// room it actually bought.
    pub tokens_before: u32,
    pub tokens_after: u32,
    /// Whether the model wrote a prose summary, or the compaction is carrying
    /// structure only.
    pub summarised: bool,
}

/// What the assistant "says" in the synthetic acknowledgement that follows the
/// compaction message.
///
/// It exists purely to keep roles alternating: the compaction message is a
/// user message, and the kept tail also begins with a user message (that is
/// what a clean split boundary *is*). Two user messages in a row is a shape
/// some providers reject outright and others silently merge, and there is
/// nothing to gain by finding out which.
const COMPACTION_ACK: &str =
    "Understood. I have the summary and carried-over state above, and I will continue from the \
     messages that follow.";

const SUMMARY_SYSTEM_PROMPT: &str =
    "You are compacting the transcript of a coding session so that work can continue in a smaller \
     context window. Write a dense factual summary in under 400 words. Cover, in this order: what \
     the user asked for; decisions that were made and the reasoning behind them; what was actually \
     changed and where; anything that was tried and failed, and why; and what remains unresolved. \
     Prefer concrete names — files, functions, commands, error messages — over description. Do not \
     speculate, do not offer next steps, and do not address the user. Output only the summary.";

/// Implemented by smith-tools::ToolRegistry. Kept as a trait here so smith-core
/// never depends on the concrete tool crate.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition>;
    fn permission_class(&self, name: &str) -> Option<PermissionClass>;
    /// Forwards `Tool::snapshot_paths` for the named tool. Defaulted so an
    /// executor that has no filesystem tools at all (and every existing
    /// implementation) compiles unchanged.
    fn snapshot_paths(
        &self,
        _name: &str,
        _input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
    /// Forwards `Tool::scratch_scoped` for the named tool. Defaulted to
    /// `false` — the safe answer — so executors without filesystem tools
    /// compile unchanged and never accidentally waive a prompt.
    fn scratch_scoped(&self, _name: &str, _input: &serde_json::Value, _ctx: &ToolContext) -> bool {
        false
    }
    /// Checks a call against the schema the model was shown, without running
    /// it.
    ///
    /// `execute` already does this at dispatch and still does; this exists so
    /// the *same* check can be applied to arguments a `PreToolUse` hook
    /// rewrote, at the point the rewrite happens. Without it the only place a
    /// hook's mistake would surface is a dispatch-time error that reads as if
    /// the model had written the bad arguments — blaming the one participant
    /// that did not.
    ///
    /// Defaulted to `Ok(())` so executors that publish no schemas compile
    /// unchanged. That is safe rather than lax: a default `Ok` only loses the
    /// attribution, never the check, because dispatch validates again.
    fn validate_input(&self, _name: &str, _input: &serde_json::Value) -> Result<(), String> {
        Ok(())
    }
    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult;
}

/// A no-op executor for when the agent is run without any tools wired in yet.
pub struct NoTools;

#[async_trait]
impl ToolExecutor for NoTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
    }

    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        None
    }

    async fn execute(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        ToolResult::error("no tools are registered")
    }
}

/// Asks the TUI (or any frontend) to resolve a permission prompt. The oneshot
/// sender is how the caller's answer makes it back into the orchestration loop.
pub struct PermissionAsk {
    pub request: PermissionRequest,
    pub respond_to: oneshot::Sender<PermissionDecision>,
}

/// Asks a frontend to resolve an `ask_user` question.
///
/// The oneshot carries `Ok(answer)` — one of the three suggestions or custom
/// input — or `Err(reason)` when the frontend cannot ask at all. That second
/// case is not hypothetical: headless runs have no user, and being able only
/// to *answer* forced them to put words in the user's mouth. A refusal comes
/// back to the model as a failed tool call, which is the honest shape.
pub struct QuestionAsk {
    pub question: UserQuestion,
    pub respond_to: oneshot::Sender<Result<String, String>>,
}

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

    /// Usage and cost for the most recent `run_turn` — what the caller
    /// persists as one `turns` row.
    pub fn last_turn(&self) -> Option<&TurnAccounting> {
        self.last_turn.as_ref()
    }

    pub fn session_usage(&self) -> Usage {
        self.session_usage
    }

    /// Accumulated cost for the session, in USD. Only ever the sum of costs
    /// computed at the time of each turn — never a recomputation from a price
    /// table that may have moved since.
    pub fn session_cost_usd(&self) -> f64 {
        self.session_cost_usd
    }

    /// Turns billed against a model this build has no price for.
    pub fn unpriced_turns(&self) -> u32 {
        self.unpriced_turns
    }

    /// Restores the running totals for a resumed session, from the numbers the
    /// session store recorded when those turns actually ran. Also used by a
    /// `/model` switch, which rebuilds the whole `Agent`.
    pub fn seed_session_totals(&mut self, usage: Usage, cost_usd: f64, unpriced_turns: u32) {
        self.session_usage = usage;
        self.session_cost_usd = cost_usd;
        self.unpriced_turns = unpriced_turns;
    }

    /// How full the context window is for the *next* request.
    ///
    /// Exact where it can be and estimated only where it must be: the last
    /// response's `prompt_tokens` (input + cache read + cache write — Anthropic
    /// reports those separately, and adding only `input_tokens` would miss the
    /// entire cached prefix) plus its `output_tokens`, which is the assistant
    /// message now sitting in history, plus a `chars/4` estimate of everything
    /// appended since. Before the first response there is nothing but estimate,
    /// and the system prompt and tool schemas have to be estimated too — they
    /// are a fixed several-thousand-token floor that a naive count of
    /// `messages` alone would miss entirely.
    pub fn context_usage(&self) -> ContextUsage {
        let window = self.provider.capabilities(&self.model).context_window;

        let (counted, exact) = match self.last_usage {
            Some(usage) => (
                usage.prompt_tokens().saturating_add(usage.output_tokens),
                true,
            ),
            None => (self.estimate_request_overhead(), false),
        };

        let pending = self
            .messages
            .get(self.counted_messages..)
            .map(estimate_messages_tokens)
            .unwrap_or(0);

        ContextUsage {
            used: counted.saturating_add(pending),
            window,
            // Exact only at the instant a response lands with nothing appended
            // after it; one tool result and it is an estimate again.
            estimated: !exact || pending > 0,
        }
    }

    /// The part of a request that is not conversation: system prompt and tool
    /// definitions. Only used before the first response, since after that the
    /// provider's `input_tokens` already includes it.
    fn estimate_request_overhead(&self) -> u32 {
        let system = self
            .effective_system()
            .map(|s| estimate_tokens(&s))
            .unwrap_or(0);
        let tools = self
            .tools
            .tool_defs()
            .iter()
            .map(|d| {
                estimate_tokens(&d.name)
                    .saturating_add(estimate_tokens(&d.description))
                    .saturating_add(estimate_tokens(&d.input_schema.to_string()))
            })
            .fold(0u32, u32::saturating_add);
        system.saturating_add(tools)
    }

    /// Whether history is due for compaction.
    pub fn should_compact(&self) -> bool {
        self.compaction.enabled && self.context_usage().ratio() >= self.compaction.threshold
    }

    fn emit_context(&self, events: &mpsc::UnboundedSender<AgentEvent>) {
        let context = self.context_usage();
        let _ = events.send(AgentEvent::ContextUsage {
            used: context.used,
            window: context.window,
            estimated: context.estimated,
        });
    }

    /// Folds one provider response's usage into the turn, session, and context
    /// bookkeeping. Called once per round, after the assistant message has
    /// been pushed, so `counted_messages` lines up with what the provider was
    /// actually charging for.
    fn note_usage(&mut self, usage: Usage) {
        self.last_usage = Some(usage);
        self.counted_messages = self.messages.len();
        self.session_usage.add(&usage);

        let provider = self.provider.id().to_string();
        // Through the provider, not `self.model` directly: a fallback chain
        // that advanced mid-session answers with the entry now serving, and
        // pricing/persisting the turn under the original model's name would
        // be a silent accounting error.
        let model = self.provider.effective_model(&self.model);
        let cost = crate::pricing::cost_usd(&provider, &model, &usage);
        match cost {
            Some(cost) => self.session_cost_usd += cost,
            None => self.unpriced_turns = self.unpriced_turns.saturating_add(1),
        }

        // One `TurnAccounting` spans every round of a turn: the model cannot
        // change mid-turn (a fallback advancement lands between requests, and
        // the round that failed never produced usage to note), so summing
        // rounds loses nothing, and it keeps the persisted `turns` table one
        // row per user-visible turn rather than one per HTTP request.
        let turn = self.last_turn.get_or_insert_with(|| TurnAccounting {
            provider: provider.clone(),
            model,
            usage: Usage::default(),
            cost_usd: None,
        });
        turn.usage.add(&usage);
        if let Some(cost) = cost {
            turn.cost_usd = Some(turn.cost_usd.unwrap_or(0.0) + cost);
        }
    }

    /// Bills a request the agent made on its own behalf (the compaction
    /// summary, a subagent's whole conversation) to the session and the
    /// current turn — but *not* to the context tracker. It was a different
    /// prompt entirely, so letting it overwrite `last_usage` would make the
    /// gauge describe a conversation that isn't the one in `self.messages`.
    ///
    /// `model` is passed rather than read from `self` because a subagent may
    /// be configured to run on a different one, and pricing that request at
    /// the parent's rate would be a silent error in the direction nobody
    /// checks.
    fn note_side_request_usage(&mut self, usage: Usage, model: &str) {
        self.session_usage.add(&usage);
        let provider = self.provider.id().to_string();
        let cost = crate::pricing::cost_usd(&provider, model, &usage);
        if let Some(cost) = cost {
            self.session_cost_usd += cost;
        }
        let turn = self.last_turn.get_or_insert_with(|| TurnAccounting {
            provider,
            model: self.model.clone(),
            usage: Usage::default(),
            cost_usd: None,
        });
        turn.usage.add(&usage);
        if let Some(cost) = cost {
            turn.cost_usd = Some(turn.cost_usd.unwrap_or(0.0) + cost);
        }
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
            parts.push(format!(
                "Current session goal: {goal}\nKeep this goal in mind and work toward it unless the user directs you otherwise."
            ));
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

    /// Runs one full user turn to completion: sends the user's message, streams
    /// the reply, and if the model asks for tools, executes them (round-tripping
    /// through `permission_tx` for anything above ReadOnly) until the model
    /// produces a final end-turn response.
    /// Returns `true` if the turn ran to a normal completion, `false` if it
    /// ended early via cancellation or a provider/stream error (an `Error`
    /// event has already been sent either way) — used by the `/loop` driver
    /// to tell "stopped cleanly" apart from "stopped because of a failure".
    pub async fn run_turn(
        &mut self,
        user_text: String,
        events: mpsc::UnboundedSender<AgentEvent>,
        permission_tx: mpsc::UnboundedSender<PermissionAsk>,
        question_tx: mpsc::UnboundedSender<QuestionAsk>,
        cancel: CancellationToken,
    ) -> bool {
        // Before the checkpoint is opened and before anything is pushed to
        // history: a turn a hook refuses must leave no trace at all, and an
        // allocated turn sequence with nothing in it would show up in
        // `/rewind` as an undo that undoes nothing.
        //
        // Only for the agent the user is actually talking to. A subagent's
        // "prompt" is written by the parent *model*; firing an event called
        // `UserPromptSubmit` on it would misreport who said it, and a hook
        // that redacts what the user typed has nothing to do there.
        let user_text = if self.subagent_depth == 0 {
            let outcome = self
                .hooks
                .user_prompt_submit(&self.hook_ctx(), user_text, &cancel)
                .await;
            for notice in &outcome.notices {
                let _ = events.send(AgentEvent::Error(notice.clone()));
            }
            if let Some(denial) = outcome.denial {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::Error(denial));
                return false;
            }
            outcome.prompt
        } else {
            user_text
        };

        // Allocated before anything can run a tool, and held for the whole
        // turn: every file this turn overwrites lands in the same checkpoint,
        // which is what makes "undo that turn" a single operation.
        self.turn_seq = match &self.checkpointer {
            Some(checkpointer) => Some(checkpointer.begin_turn(&self.tool_ctx.session_id).await),
            None => None,
        };

        let user_text = if self.pending_notes.is_empty() {
            user_text
        } else {
            let notes = std::mem::take(&mut self.pending_notes).join("\n");
            format!("{notes}\n\n{user_text}")
        };
        self.messages.push(Message::user_text(user_text));
        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Thinking));
        // Fresh accounting per turn — the caller persists exactly one `turns`
        // row from this, and a leftover from the previous turn would be
        // recorded twice.
        self.last_turn = None;

        // Some providers (Ollama cloud models especially) occasionally end a
        // turn with no text and no tool call right after a tool round — the
        // model just stops instead of writing up the results. Retrying the
        // exact same request a couple of times resolves it far more often
        // than not, and is exactly what a user does by nudging the model
        // ("did you finish?") — do that automatically before giving up.
        const MAX_EMPTY_RETRIES: u32 = 2;
        let mut empty_retries: u32 = 0;

        // Turn budget. Measured from here rather than from the first request
        // so that time spent in permission prompts and backoff sleeps counts
        // too — from the user's side that is all the same wait.
        let started_at = Instant::now();
        // Published on `self` so a subagent spawned mid-turn can be handed the
        // time this turn actually has left instead of a fresh allowance.
        self.turn_deadline = Some(started_at + self.limits.max_wall_clock);
        // Refilled per turn, exactly like `tool_calls` below: the pool bounds
        // one turn's delegation, not the session's.
        self.subagent_tool_budget = self.limits.max_tool_calls_per_turn;
        let mut rounds: u32 = 0;
        let mut tool_calls: u32 = 0;
        // Context size immediately after the last compaction this turn, if
        // there was one — see the guard at the top of the loop.
        let mut compacted_at: Option<u32> = None;

        loop {
            if cancel.is_cancelled() {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::Error("cancelled".into()));
                return false;
            }

            // Anything the user typed since the last round joins the
            // conversation here, as an ordinary user message.
            //
            // At a round boundary specifically: the messages list has just
            // been left consistent (assistant message pushed, every `tool_use`
            // answered), so inserting one cannot strand a tool call. Injecting
            // mid-stream would.
            //
            // It is a plain user message, with no wrapper telling the model
            // what to do with it. Whether "also handle the errors" changes the
            // job in flight or adds a new one is exactly the judgement a model
            // is for, and a framing that pre-decided it would be wrong half
            // the time — the user knows which they meant and wrote it that way.
            for text in self.take_interjections() {
                let _ = events.send(AgentEvent::UserInterjected(text.clone()));
                self.messages.push(Message::user_text(text));
            }

            // Checked here, at a round boundary, because that is the only
            // place history is guaranteed well-formed: every `tool_use` from
            // the previous round already has its matching `tool_result`
            // pushed. A failed compaction leaves history exactly as it was and
            // the turn proceeds normally — see `compact`.
            //
            // The `compacted_at` guard stops a turn compacting the compaction:
            // if a window is small enough that even the carried-over state sits
            // above the threshold, an unguarded check would fire again on the
            // very next round and this time throw the carry-over away. Growth
            // past the post-compaction level is the right condition rather than
            // "once per turn", because a tool-heavy turn genuinely can refill
            // the window and legitimately needs compacting twice.
            if self.should_compact()
                && compacted_at.is_none_or(|after| self.context_usage().used > after)
            {
                if let Ok(outcome) = self.compact(&events, &cancel).await {
                    compacted_at = Some(outcome.tokens_after);
                }
            }

            let request = CompletionRequest {
                model: self.model.clone(),
                system: self.effective_system(),
                messages: self.messages.clone(),
                tools: self.tools.tool_defs(),
                max_tokens: self.max_tokens,
                temperature: None,
            };

            let stream = match self.stream_with_retry(request, &events, &cancel).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                    let _ = events.send(AgentEvent::Error(e.to_string()));
                    return false;
                }
            };

            let outcome = match consume_stream(stream, &events, cancel.clone()).await {
                Ok(v) => v,
                Err(e) => {
                    let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                    let _ = events.send(AgentEvent::Error(e));
                    return false;
                }
            };
            let StreamOutcome {
                message: mut assistant_message,
                mut stop_reason,
                usage,
                reasoning_tags_stripped,
            } = outcome;
            self.reasoning_tags_stripped += reasoning_tags_stripped;

            // Some local models (small/quantized ones especially) don't
            // reliably use the provider's structured tool-calling channel —
            // they print the call as plain JSON text instead, which would
            // otherwise just sit there as a dead assistant message. Recover
            // it into a real tool call so it actually runs.
            if stop_reason == StopReason::EndTurn {
                if let Some(recovered) = self.recover_text_tool_call(&assistant_message) {
                    assistant_message = recovered;
                    stop_reason = StopReason::ToolUse;
                }
            }

            // A genuinely empty, non-tool-use turn is usually the provider
            // stalling rather than the model deliberately having nothing to
            // say — retry in place (nothing was pushed to history yet, so
            // this re-sends the identical request) before surfacing it to
            // the user as "no output".
            if stop_reason == StopReason::EndTurn
                && assistant_message.content.is_empty()
                && empty_retries < MAX_EMPTY_RETRIES
            {
                empty_retries += 1;
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Thinking));
                continue;
            }

            // A cancelled stream leaves a half-built message: any `ToolUse`
            // block in it was never dispatched, and its input JSON may have
            // stopped mid-token. Drop those blocks before the message reaches
            // history — a `tool_use` with no matching `tool_result` makes the
            // *next* request fail outright, so an interrupted turn would
            // otherwise poison the whole session. The text is kept: it's what
            // the model managed to say, and it's still useful context.
            if stop_reason == StopReason::Cancelled {
                assistant_message
                    .content
                    .retain(|b| !matches!(b, ContentBlock::ToolUse { .. }));
            }

            // A provider can legitimately return a completely empty turn
            // (no text, no tool use) — pushing that into history would
            // serialize as `content: null` on the next request, which
            // OpenAI-compatible endpoints (Ollama included) reject outright.
            // Skipping the push just drops the no-op round from history.
            if !assistant_message.content.is_empty() {
                self.messages.push(assistant_message.clone());
            }
            // After the push, so `counted_messages` marks the exact point up
            // to which the provider's own token count is authoritative.
            self.note_usage(usage);
            let _ = events.send(AgentEvent::SessionCost {
                usd: self.session_cost_usd,
                unpriced_turns: self.unpriced_turns,
            });
            self.emit_context(&events);
            let _ = events.send(AgentEvent::AssistantTurnComplete {
                message: assistant_message.clone(),
                stop_reason,
            });

            if stop_reason != StopReason::ToolUse {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                // Cancellation is not a normal completion — callers use the
                // return value to decide whether to keep driving the loop.
                return stop_reason != StopReason::Cancelled;
            }

            empty_retries = 0;
            rounds += 1;

            let tool_uses: Vec<(String, String, serde_json::Value)> = assistant_message
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            // Every `tool_use` must come back with a matching `tool_result`
            // or the provider rejects the next request. Seeding the answers
            // up front and *filling them in* — rather than appending as we
            // go — makes that invariant hold on every exit path, cancellation
            // included, instead of depending on the loop running to the end.
            let mut results: Vec<ContentBlock> = tool_uses
                .iter()
                .map(|(id, _, _)| ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: NOT_EXECUTED_CANCELLED.to_string(),
                    is_error: true,
                })
                .collect();

            // Grouping is decided here, up front, and preserves the model's
            // order: a maximal *contiguous* run of concurrency-safe calls
            // becomes one concurrent group, and anything else splits the run.
            //
            // The alternative — hoisting every ReadOnly call in the round to
            // the front and running the rest afterwards — would produce one
            // bigger group, but it reorders execution relative to what the
            // model asked for, so a read placed *after* a write in the same
            // round would stop seeing that write. Nothing in the protocol
            // promises a round's calls are independent, and a silently stale
            // read is exactly the class of bug that survives a test suite.
            // The cost of splitting instead is real but small: in
            // `[read, write, read, read]` the first read runs alone. Models
            // batch their reads together, which is the case this optimises.
            let mut steps: Vec<ToolStep> = Vec::new();
            for (slot, (_, name, _)) in tool_uses.iter().enumerate() {
                // The one cap that has to bite mid-round: a single round can
                // ask for more calls than the whole turn has left. Spending
                // the budget here, in order, keeps which calls get refused
                // independent of how long any of them takes to run.
                if tool_calls >= self.limits.max_tool_calls_per_turn {
                    steps.push(ToolStep::OverBudget(slot));
                    continue;
                }
                tool_calls += 1;
                match (self.is_concurrency_safe(name), steps.last_mut()) {
                    (true, Some(ToolStep::Concurrent(group))) => group.push(slot),
                    (true, _) => steps.push(ToolStep::Concurrent(vec![slot])),
                    (false, _) => steps.push(ToolStep::Serial(slot)),
                }
            }

            let mut cancelled = false;
            for step in steps {
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }

                match step {
                    // The seeded slot is overwritten rather than left at the
                    // cancellation wording, so the model isn't told the user
                    // stopped it when the user did nothing of the sort.
                    ToolStep::OverBudget(slot) => {
                        results[slot] = ContentBlock::ToolResult {
                            tool_use_id: tool_uses[slot].0.clone(),
                            content: NOT_EXECUTED_TOOL_BUDGET.to_string(),
                            is_error: true,
                        };
                    }
                    ToolStep::Serial(slot) => {
                        let (id, name, input) = &tool_uses[slot];
                        let result = self
                            .run_one_tool(
                                id,
                                name,
                                input.clone(),
                                &events,
                                &permission_tx,
                                &question_tx,
                                cancel.clone(),
                            )
                            .await;
                        results[slot] = ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: result.content,
                            is_error: result.is_error,
                        };
                    }
                    ToolStep::Concurrent(group) => {
                        self.run_concurrent_group(
                            &group,
                            &tool_uses,
                            &mut results,
                            &events,
                            &cancel,
                        )
                        .await;
                        // A group swallows the cancellation itself (it has to
                        // — the calls already in flight still owe the model a
                        // result), so re-check rather than waiting for the top
                        // of the next iteration to notice.
                        if cancel.is_cancelled() {
                            cancelled = true;
                            break;
                        }
                    }
                }
            }

            // Checked here, with the round's results in hand and before they
            // are pushed, so the explanation rides the same user message the
            // tool results do — one message, one push, invariant preserved on
            // this exit path exactly as on the cancellation one.
            let limit = (!cancelled)
                .then(|| self.limit_reached(rounds, tool_calls, started_at))
                .flatten();
            if let Some(kind) = limit {
                results.push(ContentBlock::Text {
                    text: limit_note(kind, &self.limits),
                });
            }

            self.messages.push(Message {
                role: Role::User,
                content: results,
            });
            // Tool results are the single biggest thing that lands in history
            // between responses, so this is where the gauge actually moves.
            self.emit_context(&events);

            if cancelled {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::Error("cancelled".into()));
                return false;
            }

            if let Some(kind) = limit {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::TurnLimitReached {
                    kind,
                    detail: kind.describe(&self.limits),
                });
                // Not a normal completion: `/loop` must not answer a runaway
                // turn by starting another one.
                return false;
            }

            let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Thinking));
        }
    }

    /// Which cap, if any, this turn has now reached. Called once per completed
    /// round; the order fixes which one is reported when several are hit at
    /// the same moment.
    ///
    /// Every comparison is `>=`, so exhausting a budget exactly stops the
    /// turn. Letting one more round run would buy the model a request it has
    /// no tool calls left to spend — and if it then asked for tools anyway,
    /// every one of them would be refused and we would stop on the next check
    /// regardless.
    fn limit_reached(
        &self,
        rounds: u32,
        tool_calls: u32,
        started_at: Instant,
    ) -> Option<TurnLimitKind> {
        if rounds >= self.limits.max_turns {
            Some(TurnLimitKind::Rounds)
        } else if tool_calls >= self.limits.max_tool_calls_per_turn {
            Some(TurnLimitKind::ToolCalls)
        } else if started_at.elapsed() >= self.limits.max_wall_clock {
            Some(TurnLimitKind::WallClock)
        } else {
            None
        }
    }

    /// Replaces the older part of history with a summary plus a structural
    /// carry-over, freeing context without losing what the session established.
    ///
    /// **Atomic.** The new history is assembled in a local vector and only
    /// assigned to `self.messages` on the last line. Every failure path —
    /// nothing safe to cut, provider error, cancellation — returns before that
    /// point with history byte-for-byte unchanged. The alternative, falling
    /// back to a mechanical drop when the summariser fails, would quietly
    /// destroy the reasoning behind everything already done at exactly the
    /// moment the provider is flaky; the turn continuing at full context and
    /// the trigger firing again next round is strictly better, because the
    /// retry layer will very likely have succeeded by then.
    ///
    /// **It spends one provider request, on the session's own model.** The
    /// structural half (todos, goal, files) is mechanical and free, but
    /// "decisions taken and why" exists only as prose in the transcript and no
    /// amount of scanning recovers it. Using a cheaper model was considered
    /// and rejected: `capabilities()` reports windows and features, not price,
    /// so it cannot actually identify the cheap one — that would take a second
    /// hardcoded model table, drifting exactly the way the pricing table
    /// drifts. And a cheaper model is not necessarily *available*: an Ollama
    /// user has one model pulled, and an API key does not imply access to
    /// every model behind it. The session's own model is the one we know
    /// works. The cost is bounded instead: the transcript is excerpted before
    /// it is sent (see `context::render_transcript`) and the reply is capped
    /// at `summary_max_tokens`.
    pub async fn compact(
        &mut self,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<CompactionOutcome, String> {
        let split = compaction_split(&self.messages, self.compaction.keep_recent)
            .ok_or("nothing safe to compact — history has no clean split point")?;

        let messages_before = self.messages.len();
        let tokens_before = self.context_usage().used;
        let dropped = &self.messages[..split];

        // Pure, and computed before anything can fail: whatever the provider
        // does next, these facts are already in hand.
        let carried = carry_over(dropped, self.goal.as_deref(), &self.tasks);
        // Todos recovered from history become the live list, if there wasn't
        // one. Without this, a *second* compaction would look for a
        // `write_tasks` call that is no longer anywhere in history — the first
        // compaction replaced it with prose — and the todos this one just
        // rescued would quietly not survive the next round.
        let recovered_tasks = (self.tasks.is_empty() && !carried.pending_tasks.is_empty())
            .then(|| carried.pending_tasks.clone());

        let (summary, summary_usage) = self.summarise(dropped, events, cancel).await?;
        // The user paid for that request whether or not the compaction is a
        // success, so it lands in the session totals either way.
        self.note_side_request_usage(summary_usage, &self.model.clone());

        let mut compacted = Vec::with_capacity(self.messages.len() - split + 2);
        compacted.push(Message::user_text(carried.render(Some(&summary))));
        compacted.push(Message::assistant(vec![ContentBlock::Text {
            text: COMPACTION_ACK.to_string(),
        }]));
        compacted.extend_from_slice(&self.messages[split..]);

        // The only mutations in this function, and they are unreachable from
        // every failure path above.
        self.messages = compacted;
        if let Some(tasks) = recovered_tasks {
            self.tasks = tasks.clone();
            let _ = events.send(AgentEvent::TasksUpdated(tasks));
        }
        // The provider's last token count described a prompt that no longer
        // exists, so the gauge falls back to a full estimate until the next
        // response corrects it.
        self.last_usage = None;
        self.counted_messages = 0;

        let outcome = CompactionOutcome {
            messages_before,
            messages_after: self.messages.len(),
            tokens_before,
            tokens_after: self.context_usage().used,
            summarised: true,
        };
        self.emit_context(events);
        Ok(outcome)
    }

    /// Asks the model to summarise `dropped`, as a single plain-text request.
    ///
    /// The transcript goes in as the *content of one user message* rather than
    /// as replayed conversation history. That makes the `tool_use` /
    /// `tool_result` pairing rules irrelevant — text cannot be malformed the
    /// way a message array can — and lets each tool result be excerpted, which
    /// is where nearly all the savings are.
    async fn summarise(
        &self,
        dropped: &[Message],
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<(String, Usage), String> {
        let window = self.provider.capabilities(&self.model).context_window;
        // Half the window, in characters, at the un-margined 4:1 ratio. The
        // summarisation request must comfortably fit alongside its own reply.
        let budget_chars = (window as usize / 2).saturating_mul(4);
        let transcript = render_transcript(dropped, budget_chars);

        let request = CompletionRequest {
            model: self.model.clone(),
            system: Some(SUMMARY_SYSTEM_PROMPT.to_string()),
            messages: vec![Message::user_text(format!(
                "Summarise this session transcript.\n\n<transcript>\n{transcript}\n</transcript>"
            ))],
            // No tools: this request has one job and must not start doing work
            // of its own halfway through a compaction.
            tools: Vec::new(),
            max_tokens: self.compaction.summary_max_tokens,
            temperature: None,
        };

        let stream = self
            .stream_with_retry(request, events, cancel)
            .await
            .map_err(|e| e.to_string())?;

        // A private channel, because `consume_stream` streams text deltas to
        // whoever it is handed — and the summary must never appear in the
        // chat pane as something the assistant said to the user. Token usage
        // is the one thing forwarded on: the user is paying for this request,
        // so it belongs in their totals.
        let (quiet_tx, mut quiet_rx) = mpsc::unbounded_channel();
        let result = consume_stream(stream, &quiet_tx, cancel.clone()).await;
        drop(quiet_tx);
        while let Ok(event) = quiet_rx.try_recv() {
            if let AgentEvent::TokenUsage(usage) = event {
                let _ = events.send(AgentEvent::TokenUsage(usage));
            }
        }

        let outcome = result?;
        if outcome.stop_reason == StopReason::Cancelled {
            return Err("compaction cancelled".to_string());
        }
        // Reasoning stripped out of a *summary* is deliberately not counted:
        // the counter reports on what the user's own turns produced, and this
        // request is internal plumbing they never see.
        let (text, usage) = (outcome.message.text(), outcome.usage);
        if text.trim().is_empty() {
            return Err("the summarising request returned no text".to_string());
        }
        Ok((text, usage))
    }

    /// Opens the completion stream, re-sending on failures worth re-sending.
    ///
    /// Only the *request* is retried, never a stream that already started:
    /// by then text deltas have reached the transcript, and replaying the
    /// request would duplicate the model's output on screen and in history.
    /// A mid-stream failure surfaces as an error, same as before.
    async fn stream_with_retry(
        &self,
        request: CompletionRequest,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let mut attempt: u32 = 1;
        loop {
            let error = match self
                .provider
                .stream_completion(request.clone(), cancel.clone())
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(e) => e,
            };

            let Some(delay) = self.retry_policy.delay_for(&error, attempt) else {
                return Err(error);
            };

            let _ = events.send(AgentEvent::ProviderRetry {
                attempt,
                max_attempts: self.retry_policy.max_attempts,
                delay_ms: delay.as_millis() as u64,
                reason: error.to_string(),
            });

            // Esc during a backoff has to take effect now, not when the timer
            // happens to expire: the whole point of showing the wait is that
            // the user can decide not to sit through it.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                _ = (self.sleeper)(delay) => {}
            }
            attempt += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_one_tool(
        &mut self,
        id: &str,
        name: &str,
        input: serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
        permission_tx: &mpsc::UnboundedSender<PermissionAsk>,
        question_tx: &mpsc::UnboundedSender<QuestionAsk>,
        cancel: CancellationToken,
    ) -> ToolResult {
        // `ask_user` (a modal), `write_tasks` (a checklist) and `task`
        // (delegation) are intercepted below and never reach the class lookup,
        // the plan gate, the permission prompt or `tools.execute`. Named once
        // here so the gates in between can say what they are exempting rather
        // than re-testing the names one at a time.
        let intercepted =
            name == "ask_user" || name == "write_tasks" || name == subagent::TASK_TOOL;

        let class = self
            .tools
            .permission_class(name)
            .unwrap_or(PermissionClass::Dangerous);

        if !intercepted && self.plan_gated && class != PermissionClass::ReadOnly {
            let result = ToolResult::error(
                "Blocked: a plan is awaiting approval. Tell the user to run `/plan approve` (or `/plan reject` to discard it) before this can run.",
            );
            let _ = events.send(AgentEvent::ToolCallStarted {
                id: id.to_string(),
                tool_name: name.to_string(),
                input,
            });
            let _ = events.send(AgentEvent::ToolCallResult {
                id: id.to_string(),
                output: result.content.clone(),
                is_error: true,
            });
            return result;
        }

        // `PreToolUse`, and this is the only defensible place for it — the
        // full argument is in `docs/authorization.md`, the two halves of it
        // are:
        //
        // - *After* the plan gate, because a hook can only ever subtract
        //   authority. Running it above the gate would spawn a process per
        //   call that could never have run, and would put a hook's "allow"
        //   syntactically upstream of the one gate in this system with no
        //   bypass — an invitation to someone later wiring it up as an
        //   override.
        // - *Before* everything below, because a hook that runs after the
        //   permission prompt cannot prevent the prompt (its main job), and a
        //   hook that rewrites arguments after `format_permission_detail` has
        //   built the modal would have the user approve one call and a
        //   different one run. Above `needs_prompt` rather than merely above
        //   the prompt itself, so a session grant, `scratch_scoped` or
        //   `/permission skip` cannot skip the hook along with the modal.
        //
        // The three intercepted tools below reach this too. They have no plan
        // gate to be after, so for them the hook is simply the first gate
        // there is — which is what lets a policy hook see `task`, the one
        // intercepted tool whose effects are not confined to the UI.
        let input = match self.pre_tool_hooks(id, name, input, events, &cancel).await {
            Ok(input) => input,
            Err(blocked) => return blocked,
        };

        // The intercepted tools below return before `ToolExecutor::execute`,
        // which is where every other call is checked against the schema the
        // model was shown. That made the registry's claim to be "the one
        // place" false, and left three tools re-implementing ad-hoc argument
        // parsing — decent today, but the invariant is what the next
        // intercepted tool inherits. Checked here instead, before the
        // interception, so the claim is true again for every path.
        //
        // A rejection is an ordinary tool error for the same reason it is in
        // the registry: the model sees it and a wrong argument is the most
        // correctable thing it can be told.
        if INTERCEPTED_TOOLS.contains(&name) {
            if let Err(message) = self.tools.validate_input(name, &input) {
                let _ = events.send(AgentEvent::ToolCallStarted {
                    id: id.to_string(),
                    tool_name: name.to_string(),
                    input: input.clone(),
                });
                let _ = events.send(AgentEvent::ToolCallResult {
                    id: id.to_string(),
                    output: message.clone(),
                    is_error: true,
                });
                return ToolResult::error(message);
            }
        }

        // Clarifying questions are allowed even while plan-gated.
        if name == "ask_user" {
            return self
                .run_ask_user(id, input, events, question_tx, cancel)
                .await;
        }

        // Bookkeeping only — no side effects, so it's exempt from both the
        // plan gate and the permission prompt, same reasoning as ask_user.
        if name == "write_tasks" {
            return self.run_write_tasks(id, input, events).await;
        }

        // Delegation. Intercepted for a structural reason rather than a UI
        // one: a subagent *is* an `Agent`, built from this agent's provider,
        // tool registry, model and context — none of which a `Tool` can reach
        // from behind `&self` in another crate. Everything the child is
        // allowed to be is decided from this agent's own state, which is
        // exactly what makes budget, depth and the tool set enforceable here
        // and unforgeable from the tool side. Its own tools are read-only, so
        // like `ask_user` it is exempt from the plan gate and the prompt.
        if name == subagent::TASK_TOOL {
            // `task` is classed `ReadOnly` because a child's own tools are,
            // and interactively that is right: the user is watching, and the
            // child can only look. Unattended, nobody is watching and the
            // child spends the user's money — so it has to be named like
            // anything else. `--allowed-tools` is the only control a headless
            // run has, and "spawn a whole agent" is not what a reader expects
            // it to leave open.
            if self.unattended && !self.allowed_session_tools.contains(name) {
                if let Err(refusal) = self
                    .request_permission(id, name, &input, events, permission_tx)
                    .await
                {
                    return refusal;
                }
            }
            return self.run_task(id, input, events, cancel).await;
        }

        // A call the tool itself vouches is confined to the session's scratch
        // directory skips the prompt: there is no user work in there to
        // protect, and prompting for throwaway files is exactly the friction
        // that pushes the model into writing them into the project instead.
        // Checked last — it can touch the filesystem (symlink resolution) and
        // only matters when the call would otherwise prompt. Deliberately
        // *after* the plan gate above: scratch writes are still side effects,
        // and an unapproved plan blocks them like everything else.
        let needs_prompt = class != PermissionClass::ReadOnly
            && !self.allowed_session_tools.contains(name)
            && !self.permission_policy.auto_allows(class)
            // The scratch exemption is a *friction* argument: prompting for
            // throwaway files is what pushes the model into writing them into
            // the project instead. Unattended there is no friction to spare —
            // the channel answers instantly from `--allowed-tools` — so the
            // exemption buys nothing and costs the only gate a headless run
            // has. It was the one case where a Mutating tool ran in a job that
            // named no tools at all.
            && (self.unattended || !self.tools.scratch_scoped(name, &input, &self.tool_ctx));

        if needs_prompt {
            if let Err(refusal) = self
                .request_permission(id, name, &input, events, permission_tx)
                .await
            {
                return refusal;
            }
        }

        self.dispatch_tool(id, name, input, class, events, cancel)
            .await
    }

    /// Puts one call to the permission channel and folds the answer back in.
    ///
    /// `Err` is the refusal to return to the model; `Ok` means proceed. Split
    /// out because two callers need it: the ordinary class-based check, and
    /// `task` under an unattended run, where there is no other gate at all.
    async fn request_permission(
        &mut self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
        permission_tx: &mpsc::UnboundedSender<PermissionAsk>,
    ) -> Result<(), ToolResult> {
        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::WaitingPermission));
        let detail = format_permission_detail(name, input);
        let (tx, rx) = oneshot::channel();
        let _ = events.send(AgentEvent::PermissionPromptNeeded(PermissionRequest {
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            detail: detail.clone(),
        }));
        let sent = permission_tx.send(PermissionAsk {
            request: PermissionRequest {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                detail,
            },
            respond_to: tx,
        });
        if sent.is_err() {
            return Err(ToolResult::error("permission channel closed"));
        }
        match rx.await.unwrap_or(PermissionDecision::Deny) {
            PermissionDecision::Deny => Err(ToolResult::error(
                "User denied permission to run this tool.",
            )),
            PermissionDecision::AllowSession => {
                self.allowed_session_tools.insert(name.to_string());
                Ok(())
            }
            PermissionDecision::AllowOnce => Ok(()),
        }
    }

    /// Runs the `PreToolUse` chain for one call, answering with the arguments
    /// to run with — or with the `ToolResult` that ends the call.
    ///
    /// Takes `&self`, which is what lets the concurrent read-only path call it
    /// too. If it needed `&mut self` the hook would silently not fire for
    /// batched reads, and "the hook did not run and nobody said so" is the one
    /// failure this feature must not have.
    async fn pre_tool_hooks(
        &self,
        id: &str,
        name: &str,
        input: serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value, ToolResult> {
        if self.hooks.is_empty() {
            return Ok(input);
        }
        // The same schema check dispatch applies, handed to the hook layer so
        // a rewrite is validated where it happens and the resulting error can
        // name the hook rather than implicating the model.
        let validate = |candidate: &serde_json::Value| self.tools.validate_input(name, candidate);
        let outcome = self
            .hooks
            .pre_tool_use(&self.hook_ctx(), name, input, &validate, cancel)
            .await;

        let Some(denial) = outcome.denial else {
            // Notices ride the tool's own card, so a hook that fired, rewrote
            // or misbehaved is attached to the call it acted on.
            for line in outcome.notices {
                let _ = events.send(AgentEvent::ToolProgress {
                    id: id.to_string(),
                    line,
                });
            }
            return Ok(outcome.input);
        };

        // A blocked call still gets a card: the user has to be able to see
        // that the agent tried, and what stopped it.
        let _ = events.send(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: name.to_string(),
            input: outcome.input,
        });
        for line in outcome.notices {
            let _ = events.send(AgentEvent::ToolProgress {
                id: id.to_string(),
                line,
            });
        }
        let result = ToolResult::error(denial);
        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: true,
        });
        Err(result)
    }

    /// Runs the `PostToolUse` chain over a finished call, folding whatever it
    /// wrote into the result the model will read.
    ///
    /// Never fails the call — see `HookEvent::fails_closed`. The tool has
    /// already run; the only thing left to decide is whether the model is told
    /// the truth about it.
    async fn post_tool_hooks(
        &self,
        id: &str,
        name: &str,
        input: &serde_json::Value,
        mut result: ToolResult,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> ToolResult {
        if self.hooks.is_empty() {
            return result;
        }
        let outcome = self
            .hooks
            .post_tool_use(
                &self.hook_ctx(),
                name,
                input,
                &result.content,
                result.is_error,
                cancel,
            )
            .await;
        for line in outcome.notices {
            let _ = events.send(AgentEvent::ToolProgress {
                id: id.to_string(),
                line,
            });
        }
        if let Some(extra) = outcome.extra {
            result.content = format!("{}\n{extra}", result.content);
        }
        result
    }

    /// Whether this call may run alongside others from the same round.
    ///
    /// `PermissionClass::ReadOnly` and nothing else — permanently, not
    /// pending further work. Three reasons, none of which a future tool
    /// changes:
    /// - Nearly all the wall-clock win is here anyway. A model exploring a
    ///   codebase issues reads, globs and greps in batches; it issues writes
    ///   one or two at a time.
    /// - A concurrency bug in a `Mutating` tool is a data-loss bug. Two
    ///   `edit_file` calls racing on one path corrupt it in a way no test
    ///   reliably catches, and the damage is to the user's work, not ours.
    /// - The permission round-trip is serial by construction: you cannot show
    ///   two modals at once. `Mutating` and `Dangerous` calls would spend
    ///   their time queued behind each other's prompts even if the execution
    ///   underneath them were parallel.
    ///
    /// `ask_user`, `write_tasks` and `task` all declare `ReadOnly`, but they
    /// are intercepted by name before dispatch and need `&mut self` (a
    /// checklist to rewrite, a modal to wait on, a delegation budget to
    /// spend) — so they are excluded here and stay on the serial path with
    /// the interception that owns them. For `task` that is also the right
    /// answer on its merits: two children running at once are two provider
    /// conversations billing in parallel, and their progress lines would
    /// interleave on cards the user cannot tell apart.
    fn is_concurrency_safe(&self, name: &str) -> bool {
        if name == "ask_user" || name == "write_tasks" || name == subagent::TASK_TOOL {
            return false;
        }
        self.tools.permission_class(name) == Some(PermissionClass::ReadOnly)
    }

    /// Runs one group of concurrency-safe calls, filling each one's slot in
    /// `results`.
    ///
    /// Slots are written by index as answers arrive, so the model still sees
    /// the round's results in the order it asked for them no matter which
    /// call finishes first. `results` is already seeded with "not executed",
    /// so a call this function never gets to still has an answer.
    async fn run_concurrent_group(
        &self,
        group: &[usize],
        tool_uses: &[(String, String, serde_json::Value)],
        results: &mut [ContentBlock],
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) {
        let mut running = futures::stream::iter(group.iter().copied().map(|slot| {
            let (id, name, input) = &tool_uses[slot];
            let cancel = cancel.clone();
            async move {
                // A queued call is admitted to the window only when an
                // earlier one frees a place, which makes this the point where
                // an Esc part-way through a group stops the rest of it.
                // Returning `None` leaves the seeded "not executed" answer in
                // place *and* emits no events, so the TUI never sees a card
                // start that will never finish. Calls already in flight are
                // not dropped: they hold the same token, race it themselves,
                // and come back with a real result and a real
                // `ToolCallResult`.
                if cancel.is_cancelled() {
                    return (slot, None);
                }
                // The second path through authorization, and every gate that
                // applies on the serial one has to be considered here too.
                // The plan gate and the permission prompt are no-ops for
                // `ReadOnly` calls, which is why they are absent — but a
                // `PreToolUse` hook is not, and a hook that silently skipped
                // every batched read would be worse than no hook at all.
                let input = match self
                    .pre_tool_hooks(id, name, input.clone(), events, &cancel)
                    .await
                {
                    Ok(input) => input,
                    Err(blocked) => return (slot, Some(blocked)),
                };
                let result = self
                    .dispatch_tool(id, name, input, PermissionClass::ReadOnly, events, cancel)
                    .await;
                (slot, Some(result))
            }
        }))
        .buffer_unordered(MAX_CONCURRENT_TOOLS);

        while let Some((slot, result)) = running.next().await {
            let Some(result) = result else { continue };
            results[slot] = ContentBlock::ToolResult {
                tool_use_id: tool_uses[slot].0.clone(),
                content: result.content,
                is_error: result.is_error,
            };
        }
    }

    /// The dispatch half of a tool call: announce, checkpoint, execute,
    /// redact, answer.
    ///
    /// It takes `&self`, and that is the whole reason concurrency is possible
    /// at all — everything in `run_one_tool` that needs `&mut self` (recording
    /// a session permission grant, rewriting the checklist) happens strictly
    /// before this point, and none of it applies to a `ReadOnly` call.
    async fn dispatch_tool(
        &self,
        id: &str,
        name: &str,
        input: serde_json::Value,
        class: PermissionClass,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> ToolResult {
        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Working));
        let _ = events.send(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: name.to_string(),
            input: input.clone(),
        });
        // The one place a tool learns which call it is: the context the agent
        // holds is session-long and call-agnostic, so the id and its progress
        // channel are stamped onto a per-dispatch clone.
        let ctx = self
            .tool_ctx
            .with_progress(ProgressReporter::new(id, events.clone()));

        // Here and nowhere else. This is the single point every tool call
        // funnels through, it is *after* the plan gate and the permission
        // prompt (a denied call returns above, so it never leaves an object
        // behind), and it is immediately before dispatch — the last instant
        // the old bytes still exist. Putting the hook inside `write_file` and
        // `edit_file` instead would have duplicated that timing decision in
        // every mutating tool and quietly skipped whichever one is added next.
        let snapshotted = self.checkpoint_before(name, &input, class, &ctx).await;

        let result = self
            .tools
            .execute(name, input.clone(), &ctx, cancel.clone())
            .await;
        self.checkpoint_after(&snapshotted).await;

        // `PostToolUse` lives here rather than in `run_one_tool` because this
        // is the point both authorization paths funnel through, and because it
        // is the only place a result exists at all. It runs before the
        // `ToolCallResult` event below so the card the user reads and the
        // string the model reads are the same string.
        //
        // The three tools intercepted in `run_one_tool` never reach here and
        // so observe `PreToolUse` only. That asymmetry is deliberate and
        // documented rather than papered over: their "results" are UI state (a
        // typed answer, a checklist, a child's report) that a post hook's
        // contract — observe what the tool wrote — does not describe.
        let mut result = self
            .post_tool_hooks(id, name, &input, result, events, &cancel)
            .await;

        // The only place raw tool output exists before it fans out to the
        // transcript, the session database and the next provider request.
        // Redacting here covers all three at once — and covers MCP tools for
        // free, which matters because their output is the least trusted of
        // the lot. The gated/denied paths above return strings we wrote
        // ourselves, so they can't carry a secret.
        if !self.redactor.is_empty() {
            result.content = self.redactor.redact(&result.content).into_owned();
        }

        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: result.is_error,
        });
        result
    }

    /// Snapshots whatever this call is about to overwrite, and returns the
    /// paths captured so `checkpoint_after` can revisit them.
    ///
    /// **Nothing in here can fail the tool call.** Every error is reported as
    /// an advisory progress line and swallowed: a user who cannot undo a write
    /// is worse off than before, but a user whose write was *refused* because
    /// we could not prepare to undo it is worse off still — and would have no
    /// way to make progress at all on a read-only `.smith` directory.
    async fn checkpoint_before(
        &self,
        name: &str,
        input: &serde_json::Value,
        class: PermissionClass,
        ctx: &ToolContext,
    ) -> Vec<std::path::PathBuf> {
        let (Some(checkpointer), Some(turn)) = (&self.checkpointer, self.turn_seq) else {
            return Vec::new();
        };
        let session = &self.tool_ctx.session_id;
        let paths = self.tools.snapshot_paths(name, input, ctx);

        // A tool that can change things but won't say what: `run_bash`, and
        // every MCP tool. Recorded so `/rewind` reports the hole rather than
        // implying the turn is fully covered.
        if paths.is_empty() {
            if class != PermissionClass::ReadOnly {
                let _ = checkpointer.note_uncovered(session, turn, name).await;
            }
            return Vec::new();
        }

        let mut captured = Vec::with_capacity(paths.len());
        for path in paths {
            match checkpointer.snapshot_before(session, turn, &path).await {
                Ok(()) => captured.push(path),
                Err(e) => ctx.report_progress(format!(
                    "checkpoint: could not snapshot {} — this write will not be undoable by /rewind ({e})",
                    path.display()
                )),
            }
        }
        captured
    }

    /// Records what the tool left behind, which is the only way `/rewind` can
    /// later tell a hand edit from its own handiwork. Same best-effort rules.
    async fn checkpoint_after(&self, paths: &[std::path::PathBuf]) {
        let (Some(checkpointer), Some(turn)) = (&self.checkpointer, self.turn_seq) else {
            return;
        };
        for path in paths {
            let _ = checkpointer
                .snapshot_after(&self.tool_ctx.session_id, turn, path)
                .await;
        }
    }

    async fn run_ask_user(
        &mut self,
        id: &str,
        input: serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
        question_tx: &mpsc::UnboundedSender<QuestionAsk>,
        cancel: CancellationToken,
    ) -> ToolResult {
        let prompt = input
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let opt = |k: &str| {
            input
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let options = [opt("option_a"), opt("option_b"), opt("option_c")];
        if prompt.is_empty() || options.iter().any(|o| o.is_empty()) {
            return ToolResult::error(
                "ask_user requires question, option_a, option_b, and option_c (all non-empty)",
            );
        }

        let question = UserQuestion {
            id: id.to_string(),
            prompt,
            options: options.clone(),
        };

        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Asking));
        let _ = events.send(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: "ask_user".into(),
            input: input.clone(),
        });
        let _ = events.send(AgentEvent::UserQuestionNeeded(question.clone()));

        let (tx, rx) = oneshot::channel();
        if question_tx
            .send(QuestionAsk {
                question,
                respond_to: tx,
            })
            .is_err()
        {
            let result = ToolResult::error("question channel closed");
            let _ = events.send(AgentEvent::ToolCallResult {
                id: id.to_string(),
                output: result.content.clone(),
                is_error: true,
            });
            return result;
        }

        let answer = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let result = ToolResult::error("question cancelled");
                let _ = events.send(AgentEvent::ToolCallResult {
                    id: id.to_string(),
                    output: result.content.clone(),
                    is_error: true,
                });
                return result;
            }
            answer = rx => answer.unwrap_or_else(|_| Ok("User dismissed the question.".into())),
        };

        let result = match answer {
            Ok(answer) => ToolResult::ok(format!("User answered: {answer}")),
            Err(reason) => ToolResult::error(reason),
        };
        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: result.is_error,
        });
        result
    }

    async fn run_write_tasks(
        &mut self,
        id: &str,
        input: serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> ToolResult {
        let _ = events.send(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: "write_tasks".into(),
            input: input.clone(),
        });

        let result = match parse_tasks(&input) {
            Ok(tasks) => {
                self.tasks = tasks.clone();
                let _ = events.send(AgentEvent::TasksUpdated(tasks));
                ToolResult::ok("tasks updated")
            }
            Err(e) => ToolResult::error(e),
        };

        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: result.is_error,
        });
        result
    }

    /// Wall clock the turn in flight still has, for a child to be capped by.
    fn remaining_wall_clock(&self) -> Duration {
        match self.turn_deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            // No turn in flight (a direct `run_one_tool` in a test): the
            // child gets its own cap and nothing more.
            None => subagent::MAX_WALL_CLOCK,
        }
    }

    /// Runs one subagent to completion and answers the parent's `tool_use`
    /// with its final report — and with nothing else it did.
    ///
    /// The child is a full `Agent` built here from this one's provider, tool
    /// registry, context and redactor, but with its *own* history, its own
    /// system prompt, a read-only slice of the tools, its own much smaller
    /// budget, and private permission/question channels that refuse. It shares
    /// nothing mutable with the parent: when it returns, everything it read is
    /// dropped and only `ChildReport::report` survives.
    ///
    /// Returns a `BoxFuture` rather than being an `async fn`, and that is not
    /// style: `run_turn` → `run_one_tool` → `run_task` → `run_turn` is a real
    /// cycle, and an `async fn`'s future is an anonymous type whose `Send`-ness
    /// is *inferred* — so the compiler would have to already know the answer
    /// to work it out. Naming the type here asserts `Send` at one point in the
    /// cycle and breaks it, and the `Box` is what gives the recursive future a
    /// finite size.
    fn run_task<'a>(
        &'a mut self,
        id: &'a str,
        input: serde_json::Value,
        events: &'a mpsc::UnboundedSender<AgentEvent>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Working));
            let _ = events.send(AgentEvent::ToolCallStarted {
                id: id.to_string(),
                tool_name: subagent::TASK_TOOL.to_string(),
                input: input.clone(),
            });
            let answer = |result: ToolResult| {
                let _ = events.send(AgentEvent::ToolCallResult {
                    id: id.to_string(),
                    output: result.content.clone(),
                    is_error: result.is_error,
                });
                result
            };

            let str_arg = |key: &str| {
                input
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string()
            };
            let prompt = str_arg("prompt");
            if prompt.is_empty() {
                return answer(ToolResult::error(
                "task requires a non-empty `prompt`: the subagent sees nothing but this string, \
                 so it must state the whole task and what to report back.",
            ));
            }

            // Checked before anything is built. A child cannot reach this branch
            // through its own tool list (`task` is never in it), but the JSON
            // envelope fallback resolves names against the registry rather than
            // the visible set, so the depth limit is enforced here too.
            if self.subagent_depth >= subagent::MAX_DEPTH {
                return answer(ToolResult::error(
                "You are already a subagent, and subagents cannot delegate further. Do this part \
                 of the work yourself, or report what is still needed so the agent that called \
                 you can delegate it.",
            ));
            }

            let requested = str_arg("subagent_type");
            let def = if requested.is_empty() || requested == subagent::GENERAL_PURPOSE {
                SubagentDefinition::general_purpose()
            } else {
                match self
                    .subagent_definitions
                    .iter()
                    .find(|d| d.name == requested)
                {
                    Some(def) => def.clone(),
                    None => {
                        let mut known = vec![subagent::GENERAL_PURPOSE.to_string()];
                        known.extend(self.subagent_definitions.iter().map(|d| d.name.clone()));
                        return answer(ToolResult::error(format!(
                            "no subagent named `{requested}` is configured. Available: {}.",
                            known.join(", ")
                        )));
                    }
                }
            };

            if self.subagent_tool_budget == 0 {
                return answer(ToolResult::error(
                "not executed — this turn has already spent its whole subagent tool-call budget. \
                 Do the remaining work directly, or finish the turn and let the user continue.",
            ));
            }

            let (allowed, refused) =
                subagent::resolve_tool_set(self.tools.as_ref(), def.tools.as_deref());
            if allowed.is_empty() {
                return answer(ToolResult::error(
                    "a subagent would have no tools at all (nothing read-only is registered), so \
                 delegating this can only cost tokens. Do it directly.",
                ));
            }
            for name in &refused {
                // Advisory, not fatal: the child still runs with what it may have.
                // Silence here would leave a definition quietly doing less than it
                // says for the rest of the session.
                let _ = events.send(AgentEvent::ToolProgress {
                    id: id.to_string(),
                    line: format!(
                        "{}: `{name}` was not granted — subagents only get read-only tools",
                        def.name
                    ),
                });
            }

            let limits = TurnLimits {
                max_turns: subagent::MAX_ROUNDS,
                // Whichever is smaller: this child's own cap, or everything the
                // turn has left to give away.
                max_tool_calls_per_turn: subagent::MAX_TOOL_CALLS.min(self.subagent_tool_budget),
                // A child cannot outlive the turn that is waiting on it.
                max_wall_clock: subagent::MAX_WALL_CLOCK.min(self.remaining_wall_clock()),
            };
            let _ = events.send(AgentEvent::ToolProgress {
                id: id.to_string(),
                line: format!(
                    "{}: started — {} tools, up to {} calls",
                    def.name,
                    allowed.len(),
                    limits.max_tool_calls_per_turn
                ),
            });

            let mut child = Agent::new(
                self.provider.clone(),
                Arc::new(subagent::RestrictedTools::new(
                    self.tools.clone(),
                    allowed.clone(),
                )),
                def.model.clone().unwrap_or_else(|| self.model.clone()),
                // The same session on disk — staging, scratch and checkpoints
                // all stay where the parent's `/rewind` can find them — but a
                // *different reader*. A subagent has its own conversation and
                // its own context window, so a file it read is a file the
                // parent has still never seen. Sharing the read set let a
                // delegated `read_file` satisfy the parent's overwrite guard,
                // which is exactly the guard's job to prevent.
                self.tool_ctx.for_delegate(&format!("task.{}", id)),
            )
            .with_system(subagent::child_system_prompt(&def, &allowed))
            .with_limits(limits)
            .with_retry_policy(self.retry_policy)
            .with_max_tokens(self.max_tokens)
            // The report lands in the parent's history and the parent's
            // transcript, so it goes through the same secret filter everything
            // else does.
            .with_redactor(self.redactor.clone())
            // Never inherited from the parent, even when the parent is running
            // under `skip`. A policy the user set for calls they can see is not
            // consent for calls they cannot.
            .with_permission_policy(PermissionPolicy::Ask);
            child.subagent_depth = self.subagent_depth + 1;
            child.sleeper = self.sleeper.clone();
            child.context_provider = self.context_provider.clone();
            child.compaction = self.compaction;
            // Belt and braces: the child's tools are read-only, so the gate has
            // nothing to block, but an unapproved plan must not become a hole the
            // moment delegation is involved.
            child.plan_gated = self.plan_gated;
            // Hooks *are* inherited, unlike the permission policy right above
            // — and the two are not in tension, because they point the same
            // way. A policy is authority the user granted and a child does not
            // get to inherit; a hook is authority the user withheld, and a
            // child is the last place to relax it. A child's calls are the
            // least-watched calls in the system (one summarised progress line
            // on a card), so if any calls need a policy hook, those do.
            //
            // The surprise — a hook the user wrote for their own calls firing
            // on a child's — is real, and is why the payload carries
            // `"agent": "subagent"` and `depth`: filtering it back out is one
            // line in the hook. The opposite default cannot be filtered back
            // *in*, because a hook that never runs cannot ask to.
            child.hooks = self.hooks.clone();
            // Deliberately no checkpointer: a read-only agent has nothing to
            // snapshot, and allocating a second turn sequence inside the parent's
            // turn would split one user action across two manifests.

            let (child_tx, child_rx) = mpsc::unbounded_channel();
            let (permission_tx, permission_rx) = mpsc::unbounded_channel();
            let (question_tx, question_rx) = mpsc::unbounded_channel();
            tokio::spawn(subagent::refuse_permissions(permission_rx));
            tokio::spawn(subagent::refuse_questions(question_rx));

            // A child of the parent's token: Esc reaches the child immediately,
            // through every layer it is blocked in — and this function still
            // returns a real `ToolResult`, so the parent's `tool_use` gets its
            // `tool_result` exactly as it does for any other tool.
            let child_cancel = cancel.child_token();
            // `run_turn` takes the sender by value and holds the only clones, so
            // the relay's `recv` ends on its own the moment the child is done —
            // no sentinel, no timeout, no chance of the relay outliving it.
            let running = child.run_turn(
                prompt,
                child_tx,
                permission_tx,
                question_tx,
                child_cancel.clone(),
            );
            let watching = subagent::relay_child(&def.name, id, child_rx, events);
            let (_completed, report) = futures::future::join(running, watching).await;

            self.subagent_tool_budget = self.subagent_tool_budget.saturating_sub(report.tool_calls);
            // Billed to this turn, on the child's model. `TurnAccounting` carries
            // one provider/model pair, so a child on a different model is folded
            // in at the parent's row — the tokens are real either way, and
            // dropping them would under-report a session's true cost.
            self.note_side_request_usage(child.session_usage(), &child.model.clone());
            let _ = events.send(AgentEvent::ToolProgress {
                id: id.to_string(),
                line: format!(
                    "{}: finished after {} tool calls",
                    def.name, report.tool_calls
                ),
            });

            answer(finish_subagent(report, cancel.is_cancelled()))
        })
    }
}

/// Turns what the relay collected into the single `tool_result` the parent's
/// model reads.
///
/// A partial report is still returned, with a note. A child stopped by its
/// budget has usually done most of the work, and throwing that away to return
/// a bare error would make the parent re-delegate the same task from scratch —
/// paying twice for the half it already has. The note is what stops the parent
/// mistaking a partial answer for a complete one.
fn finish_subagent(report: subagent::ChildReport, cancelled: bool) -> ToolResult {
    let body = report.report.trim().to_string();
    if body.is_empty() {
        let reason = if cancelled || report.error.as_deref() == Some("cancelled") {
            "the subagent was cancelled before it reported anything".to_string()
        } else if let Some(limit) = &report.limit {
            format!("the subagent {limit} before reporting anything")
        } else if let Some(error) = &report.error {
            format!("the subagent failed: {error}")
        } else {
            "the subagent returned no report at all".to_string()
        };
        return ToolResult::error(reason);
    }

    let mut note = None;
    if cancelled || report.error.as_deref() == Some("cancelled") {
        note = Some("it was cancelled by the user".to_string());
    } else if let Some(limit) = &report.limit {
        note = Some(format!("it {limit}"));
    } else if let Some(error) = &report.error {
        note = Some(format!("it stopped on an error: {error}"));
    }

    match note {
        None => ToolResult::ok(body),
        Some(note) => ToolResult::ok(format!(
            "{body}\n\n[This report is partial — {note}. Treat anything it does not mention as \
             unchecked rather than absent.]"
        )),
    }
}

/// Parses a `write_tasks` call's `{"tasks": [...]}` input into `Task`s.
/// Exposed (not just used internally) so a resumed session can rebuild its
/// checklist from the last `write_tasks` call in persisted history.
pub fn parse_tasks(input: &serde_json::Value) -> Result<Vec<Task>, String> {
    let items = input
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or("write_tasks requires a non-empty `tasks` array")?;
    if items.is_empty() {
        return Err("write_tasks requires a non-empty `tasks` array".into());
    }

    items
        .iter()
        .map(|item| {
            let content = item
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if content.is_empty() {
                return Err("each task requires a non-empty `content` string".into());
            }
            let status = match item.get("status").and_then(|v| v.as_str()) {
                Some("pending") => TaskStatus::Pending,
                Some("in_progress") => TaskStatus::InProgress,
                Some("completed") => TaskStatus::Completed,
                Some(other) => {
                    return Err(format!(
                        "unknown task status `{other}` — use pending, in_progress, or completed"
                    ))
                }
                None => return Err("each task requires a `status`".into()),
            };
            Ok(Task { content, status })
        })
        .collect()
}

/// Scans `text` left to right for the first `{...}` JSON object that is a tool
/// call in disguise, returning its name, arguments, and the (trimmed) text
/// before/after it. Uses a streaming JSON parser to find the object's end
/// rather than hand-rolled brace counting, so it doesn't get confused by
/// braces inside string values.
fn find_fallback_tool_call(
    text: &str,
    known_tools: &[ToolDefinition],
) -> Option<(String, serde_json::Value, String, String)> {
    for (i, byte) in text.bytes().enumerate() {
        if byte != b'{' {
            continue;
        }
        let mut de =
            serde_json::Deserializer::from_str(&text[i..]).into_iter::<serde_json::Value>();
        let Some(Ok(value)) = de.next() else {
            continue;
        };
        if let Some((name, arguments)) = parse_tool_call_envelope(&value, known_tools) {
            let end = i + de.byte_offset();
            return Some((
                name,
                arguments,
                text[..i].trim().to_string(),
                text[end..].trim().to_string(),
            ));
        }
    }
    None
}

/// The two envelopes a model may use to ask for a tool in plain text, both
/// gated on naming a *registered* tool — JSON-shaped prose that happens to
/// have a `name` or an `action` field must never dispatch anything.
///
/// 1. `{"name": "<tool>", "arguments": {...}}` — what a model trained on the
///    function-calling wire format falls back to printing.
/// 2. `{"action": "<tool>", ...}` — the flatter form, where the remaining
///    top-level fields *are* the arguments, e.g.
///    `{"action": "web_search", "query": "terms"}`. Small local models follow
///    a single flat object far more reliably than a nested one, and it is the
///    shape the system prompt asks for by name.
///
/// The name is matched through [`resolve_tool_name`] rather than compared
/// byte-for-byte, and the arguments through [`align_arguments`] — a model weak
/// enough to print its call as text is usually also weak enough to get the
/// spelling of the name and the keys slightly wrong.
fn parse_tool_call_envelope(
    value: &serde_json::Value,
    known_tools: &[ToolDefinition],
) -> Option<(String, serde_json::Value)> {
    let named = value.get("name").and_then(|v| v.as_str());
    let arguments = value.get("arguments").filter(|v| v.is_object());
    if let (Some(name), Some(arguments)) = (named, arguments) {
        if let Some(def) = resolve_tool_name(name, known_tools) {
            return Some((def.name.clone(), align_arguments(def, arguments.clone())));
        }
    }

    let name = value.get("action").and_then(|v| v.as_str())?;
    let def = resolve_tool_name(name, known_tools)?;
    let fields = value.as_object()?;
    // `{"action": "x", "arguments": {...}}` is the two envelopes crossed, and
    // a model that emits it means the inner object — not a lone `arguments`
    // key as the argument itself.
    if fields.len() == 2 {
        if let Some(arguments) = fields.get("arguments").filter(|v| v.is_object()) {
            return Some((def.name.clone(), align_arguments(def, arguments.clone())));
        }
    }
    let arguments = fields
        .iter()
        .filter(|(key, _)| key.as_str() != "action")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Some((
        def.name.clone(),
        align_arguments(def, serde_json::Value::Object(arguments)),
    ))
}

/// Shortest fragment of a name we will accept as a prefix/suffix of a
/// registered tool. Three characters (`run`, `web`, `ask`) carry too little
/// signal to be worth the risk of dispatching the wrong tool off them.
const MIN_PARTIAL_TOOL_NAME: usize = 4;

/// Folds a name to the form the match is made on: ASCII lowercase, with every
/// run of non-alphanumerics collapsed to a single `_` and camelCase humps
/// treated as the same boundary. `Web-Search`, `WEB_SEARCH`, `web search` and
/// `webSearch` all become `web_search`.
///
/// Splitting on case is a deterministic rewrite of the *same* identifier, not
/// a similarity judgement — which is the line this whole module draws.
fn normalize_ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev: Option<char> = None;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            let hump = ch.is_ascii_uppercase()
                && prev.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit());
            if hump && !out.ends_with('_') {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
        prev = Some(ch);
    }
    out.trim_matches('_').to_string()
}

/// Whether `want` is a whole leading or trailing *segment* of `name` — the
/// only partial match we accept. Anchoring on `_` boundaries is what keeps
/// `earch` from reaching `web_search` while letting `search` through.
fn is_segment_affix(name: &str, want: &str) -> bool {
    name.strip_prefix(want).is_some_and(|r| r.starts_with('_'))
        || name.strip_suffix(want).is_some_and(|r| r.ends_with('_'))
}

/// Maps the tool name a model *wrote* in a text envelope onto a registered
/// tool, in three widening passes. This decides which tool runs, so an
/// ambiguous or unrecognised name resolves to `None` — never to a best guess.
///
/// 1. Exact. The overwhelming majority, and free.
/// 2. Equal after [`normalize_ident`]. Case, hyphens and spaces are
///    presentation, not identity: no two tools can differ only in them without
///    already being indistinguishable to a user, and the ambiguity check below
///    covers the case where a registry somehow does contain both.
/// 3. A whole-segment prefix or suffix, and only when exactly one registered
///    tool matches — `search` → `web_search`. This is the case actually
///    observed in the wild (a model writing the bare verb). Two candidates —
///    `write` against both `write_file` and `write_tasks` — must fail rather
///    than pick one, because picking is how the wrong side effect happens.
///
/// Edit-distance/fuzzy matching is deliberately *not* a fourth pass. Its whole
/// premise is that a close-enough name is the right name, which is exactly the
/// judgement that must not be made here: at distance 2, `run_bash` is reachable
/// from names that have nothing to do with running a shell, and the cost of a
/// false positive (an arbitrary tool executing) is unbounded while the benefit
/// (recovering from a typo no observed model actually makes) is small.
fn resolve_tool_name<'a>(written: &str, known: &'a [ToolDefinition]) -> Option<&'a ToolDefinition> {
    if let Some(def) = known.iter().find(|d| d.name == written) {
        return Some(def);
    }

    let want = normalize_ident(written);
    if want.is_empty() {
        return None;
    }

    let mut normalized = known.iter().filter(|d| normalize_ident(&d.name) == want);
    if let Some(first) = normalized.next() {
        // Anything reaching here is decided by this pass alone: a second
        // candidate is an ambiguity, not an invitation to keep looking.
        return normalized.next().is_none().then_some(first);
    }

    if want.len() < MIN_PARTIAL_TOOL_NAME {
        return None;
    }
    let mut affixes = known
        .iter()
        .filter(|d| is_segment_affix(&normalize_ident(&d.name), &want));
    let first = affixes.next()?;
    affixes.next().is_none().then_some(first)
}

/// Renames arguments the model invented onto the property names the tool
/// actually declares — `max_results` onto `num_results` — using the same
/// unique-or-nothing discipline as [`resolve_tool_name`].
///
/// Unknown keys are **passed through**, never dropped. Dropping is the one
/// option that silently changes what the call means: a tool that would have
/// rejected `{"path": "/", "recursive": true}` instead runs the unqualified
/// version. A tool's own input validation is the right place to refuse a key
/// it doesn't understand, and it can only do that if it sees it.
///
/// The renaming is driven entirely by the schema each tool publishes, so it
/// stays in this layer — `smith-tools` needs no per-tool alias table, and MCP
/// tools nobody here has heard of get the same treatment.
fn align_arguments(def: &ToolDefinition, arguments: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(fields) = arguments else {
        return arguments;
    };
    let Some(declared) = def
        .input_schema
        .get("properties")
        .and_then(|p| p.as_object())
    else {
        return serde_json::Value::Object(fields);
    };

    // Trailing segment rather than any segment: the last word of an argument
    // name is what carries its meaning (`max_results` and `num_results` are the
    // same thing), while a shared *leading* word usually marks two genuinely
    // different arguments (`file_path` vs `file_mode`).
    let tail = |name: &str| {
        normalize_ident(name)
            .rsplit('_')
            .next()
            .unwrap_or_default()
            .to_string()
    };
    let names: Vec<&String> = declared.keys().collect();
    let unique = |predicate: &dyn Fn(&str) -> bool| {
        let mut matched = names.iter().filter(|d| predicate(d));
        let first = matched.next()?;
        matched.next().is_none().then(|| (*first).clone())
    };

    let supplied: HashSet<&String> = fields.keys().collect();
    let mut aliased: Vec<(String, String)> = Vec::new();
    for key in fields.keys() {
        if declared.contains_key(key) {
            continue;
        }
        let normalized = normalize_ident(key);
        let want_tail = tail(key);
        let target = unique(&|d| normalize_ident(d) == normalized).or_else(|| {
            (!want_tail.is_empty())
                .then(|| unique(&|d| tail(d) == want_tail))
                .flatten()
        });
        // An alias never displaces a key the model also supplied correctly,
        // and two aliases never race for the same target — in both cases the
        // rename is ambiguous, so the key stays as written.
        if let Some(target) = target {
            if !supplied.contains(&target) && !aliased.iter().any(|(_, t)| *t == target) {
                aliased.push((key.clone(), target));
            }
        }
    }

    let mut aligned = fields;
    for (from, to) in aliased {
        if let Some(value) = aligned.remove(&from) {
            aligned.insert(to, value);
        }
    }
    serde_json::Value::Object(aligned)
}

/// Reasoning markup a model may emit in the *text* channel when the provider
/// exposes no separate reasoning stream — the shape every open reasoning
/// fine-tune has converged on.
const REASONING_TAGS: [&str; 3] = ["think", "thinking", "reasoning"];

/// What [`ReasoningFilter::scan_tag`] made of the `<` it is sitting on.
enum TagScan {
    /// The buffer ends mid-candidate; hold it and wait for the next delta.
    Incomplete,
    /// Ordinary text that merely starts with `<`.
    NotATag,
    Tag {
        len: usize,
        open: bool,
    },
}

/// Removes `<think>`/`<thinking>`/`<reasoning>` blocks from a *streamed* text
/// channel, so reasoning never reaches the transcript, history, or the next
/// request's prompt.
///
/// It lives here, in the one funnel every provider's text deltas pass through,
/// rather than in each adapter. The leak is a property of the *model*, not of
/// the wire format: the same weights served over Ollama, an OpenAI-compatible
/// proxy, or anything else emit the same tags, so a per-adapter fix would have
/// to be written once per adapter and would still miss the next one. Providers
/// that do have a real reasoning channel are unaffected — they never put the
/// tags in the text channel in the first place.
///
/// Two rules keep it from eating legitimate text, and both fail *open* (a tag
/// survives) rather than closed (prose is deleted):
///
/// - Nothing inside a ``` fenced block is markup, and neither is a tag
///   immediately preceded by a backtick — which is how a document about
///   reasoning tags, including this codebase's own, writes them.
/// - A closing tag with no opener removes only the tag. The opener is usually
///   lost upstream (consumed as a role marker by a chat template), so the
///   surrounding text is the model's real output and deleting it back to the
///   start of the message would throw away the reply.
struct ReasoningFilter {
    /// Text held back because it may be the start of a tag or fence marker
    /// that continues in the next delta.
    buf: String,
    /// Open blocks. Counted rather than a flag so a nested tag can't close the
    /// outer block early.
    depth: u32,
    in_fence: bool,
    at_line_start: bool,
    /// Last raw character consumed, for the backtick check.
    prev: Option<char>,
    /// Tags removed so far — reported so a caller can note that reasoning was
    /// dropped rather than silently losing the fact.
    stripped: u32,
}

impl ReasoningFilter {
    fn new() -> Self {
        Self {
            buf: String::new(),
            depth: 0,
            in_fence: false,
            at_line_start: true,
            prev: None,
            stripped: 0,
        }
    }

    /// Feeds one streamed delta; returns the part safe to emit now.
    fn push(&mut self, delta: &str) -> String {
        self.buf.push_str(delta);
        self.drain(false)
    }

    /// Flushes at end of stream. Anything still inside an unclosed block is
    /// dropped: the model marked it as reasoning itself, and a truncated
    /// thought is the least useful thing that could reach the transcript. If
    /// that empties the message, `run_turn`'s empty-turn retry picks it up.
    fn finish(&mut self) -> String {
        self.drain(true)
    }

    fn emit(&mut self, s: &str, out: &mut String) {
        if self.depth == 0 {
            out.push_str(s);
        }
        self.advance(s);
    }

    /// Line/backtick bookkeeping, which tracks the *raw* stream — suppressed
    /// text still moves the cursor.
    fn advance(&mut self, s: &str) {
        for ch in s.chars() {
            self.at_line_start = ch == '\n' || (self.at_line_start && ch.is_whitespace());
            self.prev = Some(ch);
        }
    }

    fn drain(&mut self, eos: bool) -> String {
        let buf = std::mem::take(&mut self.buf);
        let mut out = String::new();
        let mut cursor = 0;

        while cursor < buf.len() {
            let rest = &buf[cursor..];
            let Some(off) = rest.find(['<', '`']) else {
                self.emit(rest, &mut out);
                cursor = buf.len();
                break;
            };
            if off > 0 {
                self.emit(&rest[..off], &mut out);
                cursor += off;
                continue;
            }

            if rest.starts_with('`') {
                let run = rest.chars().take_while(|c| *c == '`').count();
                if run == rest.len() && !eos {
                    // The run may continue into the next delta and only then
                    // reach the three that open a fence.
                    break;
                }
                if run >= 3 && self.at_line_start {
                    self.in_fence = !self.in_fence;
                }
                self.emit(&rest[..run], &mut out);
                cursor += run;
                continue;
            }

            match self.scan_tag(rest, eos) {
                TagScan::Incomplete => break,
                TagScan::NotATag => {
                    self.emit("<", &mut out);
                    cursor += 1;
                }
                TagScan::Tag { len, open } => {
                    if open {
                        self.depth += 1;
                    } else {
                        self.depth = self.depth.saturating_sub(1);
                    }
                    self.stripped += 1;
                    // Consumed but never emitted — `advance` still runs so the
                    // line/backtick state stays aligned with the raw stream.
                    self.advance(&rest[..len]);
                    cursor += len;
                }
            }
        }

        self.buf = buf[cursor..].to_string();
        out
    }

    /// Classifies the `<` at the head of `rest`.
    fn scan_tag(&self, rest: &str, eos: bool) -> TagScan {
        if self.in_fence || self.prev == Some('`') {
            return TagScan::NotATag;
        }
        let after = &rest[1..];
        let (open, body) = match after.strip_prefix('/') {
            Some(body) => (false, body),
            None => (true, after),
        };
        let name_len = body
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(body.len());
        let name = body[..name_len].to_ascii_lowercase();
        // Bounded by the longest tag name, so a stray `<` can never hold the
        // stream open indefinitely.
        if !REASONING_TAGS.iter().any(|t| t.starts_with(&name)) {
            return TagScan::NotATag;
        }
        if name_len == body.len() {
            // Ran out of input mid-name.
            return if eos {
                TagScan::NotATag
            } else {
                TagScan::Incomplete
            };
        }
        if !body[name_len..].starts_with('>') || !REASONING_TAGS.contains(&name.as_str()) {
            return TagScan::NotATag;
        }
        let len = 1 + usize::from(!open) + name_len + 1;
        TagScan::Tag { len, open }
    }
}

/// One provider response, drained.
struct StreamOutcome {
    message: Message,
    stop_reason: StopReason,
    usage: Usage,
    /// Reasoning tags removed from the text channel on the way through — see
    /// [`ReasoningFilter`].
    reasoning_tags_stripped: u32,
}

/// Drains a provider's StreamEvent stream, forwarding text deltas as AgentEvents
/// as they arrive and accumulating everything into a final assistant Message.
///
/// The `Usage` is returned as well as forwarded on the event channel: the
/// caller needs it for context and cost accounting, and reading it back off a
/// channel it also owns would be a race.
async fn consume_stream(
    mut stream: futures::stream::BoxStream<
        'static,
        Result<StreamEvent, crate::provider::ProviderError>,
    >,
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Result<StreamOutcome, String> {
    use futures::StreamExt;

    let mut text = String::new();
    let mut tool_uses: Vec<(String, String, String)> = Vec::new(); // id, name, accumulated json
    let mut current_tool: Option<usize> = None;
    let mut stop_reason = StopReason::EndTurn;
    let mut total_usage = Usage::default();
    let mut reasoning = ReasoningFilter::new();

    loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                stop_reason = StopReason::Cancelled;
                break;
            }
            item = stream.next() => match item {
                Some(item) => item,
                None => break,
            },
        };
        match item.map_err(|e| e.to_string())? {
            StreamEvent::TextDelta(delta) => {
                // Filtered *before* it is forwarded, not just before it is
                // stored: the transcript is built from these deltas, so
                // stripping only the accumulated copy would still put the tags
                // on screen.
                let visible = reasoning.push(&delta);
                if !visible.is_empty() {
                    text.push_str(&visible);
                    let _ = events.send(AgentEvent::AssistantTextDelta(visible));
                }
            }
            StreamEvent::ToolUseStart { id, name } => {
                tool_uses.push((id, name, String::new()));
                current_tool = Some(tool_uses.len() - 1);
            }
            StreamEvent::ToolUseInputDelta { partial_json, .. } => {
                if let Some(idx) = current_tool {
                    tool_uses[idx].2.push_str(&partial_json);
                }
            }
            StreamEvent::ToolUseComplete { .. } => {
                current_tool = None;
            }
            StreamEvent::MessageComplete {
                stop_reason: sr,
                usage,
            } => {
                stop_reason = sr;
                total_usage.add(&usage);
                let _ = events.send(AgentEvent::TokenUsage(usage));
            }
            StreamEvent::Error(e) => return Err(e),
        }
    }

    let tail = reasoning.finish();
    if !tail.is_empty() {
        text.push_str(&tail);
        let _ = events.send(AgentEvent::AssistantTextDelta(tail));
    }
    // Removing a block leaves the blank lines that framed it. Only trimmed
    // when something was actually removed, so an untouched reply keeps
    // whatever whitespace the model chose.
    if reasoning.stripped > 0 {
        text = text.trim().to_string();
    }

    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    for (id, name, json) in tool_uses {
        let input = if json.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&json).unwrap_or(serde_json::json!({}))
        };
        content.push(ContentBlock::ToolUse { id, name, input });
    }

    Ok(StreamOutcome {
        message: Message::assistant(content),
        stop_reason,
        usage: total_usage,
        reasoning_tags_stripped: reasoning.stripped,
    })
}

#[cfg(test)]
mod tests;
