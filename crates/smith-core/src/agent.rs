use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::event::{
    AgentEvent, AgentPhase, PermissionDecision, PermissionRequest, ProgressReporter, Task,
    TaskStatus, TurnLimitKind, UserQuestion,
};
use crate::message::{CompletionRequest, ContentBlock, Message, Role, StopReason, StreamEvent};
use crate::permission_detail::format_permission_detail;
use crate::provider::{LlmProvider, ProviderError};
use crate::redact::Redactor;
use crate::retry::RetryPolicy;
use crate::tool::{PermissionClass, PermissionPolicy, ToolContext, ToolResult};

/// Stand-in result recorded for a tool call the turn never got to run. The
/// model reads it, so it says plainly that nothing happened — an empty or
/// vague result would invite it to assume the call succeeded.
const NOT_EXECUTED_CANCELLED: &str = "not executed — the turn was cancelled by the user";

/// The same idea for a call the turn had no budget left to run.
const NOT_EXECUTED_TOOL_BUDGET: &str =
    "not executed — this turn reached its tool-call budget before this call";

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

/// Implemented by smith-tools::ToolRegistry. Kept as a trait here so smith-core
/// never depends on the concrete tool crate.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition>;
    fn permission_class(&self, name: &str) -> Option<PermissionClass>;
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
        }
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

    /// If `message` is a single, otherwise-final text reply that's actually a
    /// tool call in disguise — `{"name": ..., "arguments": {...}}` or the flat
    /// `{"action": "<tool>", ...}` form, naming a *known* tool specifically,
    /// not just JSON-shaped text — rebuild it as a real `ToolUse` so the
    /// normal tool-execution path picks it up. `None` for anything that
    /// doesn't match, which is the overwhelming majority of replies — this
    /// only exists for providers/models that fall back to printing the call
    /// instead of using the structured channel.
    fn recover_text_tool_call(&self, message: &Message) -> Option<Message> {
        let [ContentBlock::Text { text }] = message.content.as_slice() else {
            return None;
        };
        let known: HashSet<String> = self.tools.tool_defs().into_iter().map(|d| d.name).collect();
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
        self.messages.push(Message::user_text(user_text));
        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Thinking));

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
        let mut rounds: u32 = 0;
        let mut tool_calls: u32 = 0;

        loop {
            if cancel.is_cancelled() {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::Error("cancelled".into()));
                return false;
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

            let (mut assistant_message, mut stop_reason) =
                match consume_stream(stream, &events, cancel.clone()).await {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                        let _ = events.send(AgentEvent::Error(e));
                        return false;
                    }
                };

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

            let mut cancelled = false;
            for (slot, (id, name, input)) in tool_uses.into_iter().enumerate() {
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }

                // The one cap that has to bite mid-round: a single round can
                // ask for more calls than the whole turn has left. The seeded
                // slot is overwritten rather than left at the cancellation
                // wording, so the model isn't told the user stopped it when
                // the user did nothing of the sort.
                if tool_calls >= self.limits.max_tool_calls_per_turn {
                    results[slot] = ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: NOT_EXECUTED_TOOL_BUDGET.to_string(),
                        is_error: true,
                    };
                    continue;
                }
                tool_calls += 1;

                let result = self
                    .run_one_tool(
                        &id,
                        &name,
                        input,
                        &events,
                        &permission_tx,
                        &question_tx,
                        cancel.clone(),
                    )
                    .await;
                results[slot] = ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: result.content,
                    is_error: result.is_error,
                };
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

        let class = self
            .tools
            .permission_class(name)
            .unwrap_or(PermissionClass::Dangerous);

        if self.plan_gated && class != PermissionClass::ReadOnly {
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

        let needs_prompt = class != PermissionClass::ReadOnly
            && !self.allowed_session_tools.contains(name)
            && !self.permission_policy.auto_allows(class);

        if needs_prompt {
            let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::WaitingPermission));
            let detail = format_permission_detail(name, &input);
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
                return ToolResult::error("permission channel closed");
            }
            let decision = rx.await.unwrap_or(PermissionDecision::Deny);
            match decision {
                PermissionDecision::Deny => {
                    return ToolResult::error("User denied permission to run this tool.");
                }
                PermissionDecision::AllowSession => {
                    self.allowed_session_tools.insert(name.to_string());
                }
                PermissionDecision::AllowOnce => {}
            }
        }

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
        let mut result = self.tools.execute(name, input, &ctx, cancel).await;

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
    known_tools: &HashSet<String>,
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
fn parse_tool_call_envelope(
    value: &serde_json::Value,
    known_tools: &HashSet<String>,
) -> Option<(String, serde_json::Value)> {
    let named = value.get("name").and_then(|v| v.as_str());
    let arguments = value.get("arguments").filter(|v| v.is_object());
    if let (Some(name), Some(arguments)) = (named, arguments) {
        if known_tools.contains(name) {
            return Some((name.to_string(), arguments.clone()));
        }
    }

    let name = value.get("action").and_then(|v| v.as_str())?;
    if !known_tools.contains(name) {
        return None;
    }
    let fields = value.as_object()?;
    // `{"action": "x", "arguments": {...}}` is the two envelopes crossed, and
    // a model that emits it means the inner object — not a lone `arguments`
    // key as the argument itself.
    if fields.len() == 2 {
        if let Some(arguments) = fields.get("arguments").filter(|v| v.is_object()) {
            return Some((name.to_string(), arguments.clone()));
        }
    }
    let arguments = fields
        .iter()
        .filter(|(key, _)| key.as_str() != "action")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Some((name.to_string(), serde_json::Value::Object(arguments)))
}

/// Drains a provider's StreamEvent stream, forwarding text deltas as AgentEvents
/// as they arrive and accumulating everything into a final assistant Message.
async fn consume_stream(
    mut stream: futures::stream::BoxStream<
        'static,
        Result<StreamEvent, crate::provider::ProviderError>,
    >,
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Result<(Message, StopReason), String> {
    use futures::StreamExt;

    let mut text = String::new();
    let mut tool_uses: Vec<(String, String, String)> = Vec::new(); // id, name, accumulated json
    let mut current_tool: Option<usize> = None;
    let mut stop_reason = StopReason::EndTurn;

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
                text.push_str(&delta);
                let _ = events.send(AgentEvent::AssistantTextDelta(delta));
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
                let _ = events.send(AgentEvent::TokenUsage(usage));
            }
            StreamEvent::Error(e) => return Err(e),
        }
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

    Ok((Message::assistant(content), stop_reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{
        empty_reply, text_reply, tool_call_reply, tool_calls_reply, ScriptedProvider,
        ScriptedResponse,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The agent retries an empty turn twice before giving up, so a provider
    /// that only ever returns nothing has to be scripted for all three.
    const EMPTY_TURN_ATTEMPTS: usize = 3;

    fn always_empty() -> ScriptedProvider {
        ScriptedProvider::streams(std::iter::repeat_with(empty_reply).take(EMPTY_TURN_ATTEMPTS))
    }

    #[tokio::test]
    async fn empty_assistant_turn_is_not_pushed_to_history() {
        let provider = Arc::new(always_empty());
        let tools = Arc::new(NoTools);
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx);

        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "hello".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        // Only the user's own message should remain — the empty assistant
        // reply must not have been appended (it would break the *next*
        // request's wire serialization otherwise).
        assert_eq!(agent.history().len(), 1);
        assert_eq!(agent.history()[0].role, Role::User);
    }

    /// Empty turns twice, then text on the third attempt — exercises the
    /// auto-retry path for providers that stall right after a tool round
    /// instead of writing up the results.
    #[tokio::test]
    async fn empty_turns_are_retried_before_giving_up() {
        let provider = Arc::new(ScriptedProvider::streams([
            empty_reply(),
            empty_reply(),
            text_reply("finally"),
        ]));
        let tools = Arc::new(NoTools);
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(provider.clone(), tools, "fake-model".to_string(), tool_ctx);

        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "hello".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(provider.request_count(), 3);
        assert_eq!(agent.history().len(), 2);
        assert_eq!(agent.history()[1].role, Role::Assistant);
        assert_eq!(agent.history()[1].text(), "finally");
    }

    /// Proposes calling `write_file` (a Mutating tool), then ends the turn
    /// with plain text once its result comes back.
    fn write_file_then_done() -> ScriptedProvider {
        ScriptedProvider::tool_call_then_text("call_1", "write_file", serde_json::json!({}), "done")
    }

    /// Classifies `write_file` as Mutating and records whether it was ever
    /// actually invoked.
    struct RecordingTools {
        executed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl ToolExecutor for RecordingTools {
        fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
            Vec::new()
        }

        fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
            Some(PermissionClass::Mutating)
        }

        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            self.executed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            ToolResult::ok("wrote")
        }
    }

    #[tokio::test]
    async fn plan_gate_blocks_mutating_tools_even_under_skip_policy() {
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(write_file_then_done());
        let tools = Arc::new(RecordingTools {
            executed: executed.clone(),
        });
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
            .with_permission_policy(PermissionPolicy::Skip); // would normally auto-allow everything
        agent.set_plan_gated(true);

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "do it".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        assert!(
            !executed.load(std::sync::atomic::Ordering::SeqCst),
            "tool must not run while plan-gated"
        );

        let mut saw_blocked_result = false;
        while let Ok(event) = events_rx.try_recv() {
            if let AgentEvent::ToolCallResult {
                is_error, output, ..
            } = event
            {
                assert!(is_error);
                assert!(output.contains("plan is awaiting approval"));
                saw_blocked_result = true;
            }
        }
        assert!(
            saw_blocked_result,
            "expected a blocked ToolCallResult event"
        );
    }

    #[tokio::test]
    async fn plan_gate_lifted_allows_the_tool_to_run() {
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(write_file_then_done());
        let tools = Arc::new(RecordingTools {
            executed: executed.clone(),
        });
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
            .with_permission_policy(PermissionPolicy::Skip);
        assert!(!agent.plan_gated());

        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "do it".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        assert!(
            executed.load(std::sync::atomic::Ordering::SeqCst),
            "tool should run once ungated (skip policy auto-allows it)"
        );
    }

    #[tokio::test]
    async fn write_tasks_updates_agent_state_even_while_plan_gated() {
        let provider = Arc::new(ScriptedProvider::tool_call_then_text(
            "call_1",
            "write_tasks",
            serde_json::json!({
                "tasks": [
                    {"content": "step one", "status": "in_progress"},
                    {"content": "step two", "status": "pending"},
                ]
            }),
            "done",
        ));
        let tools = Arc::new(NoTools);
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx);
        agent.set_plan_gated(true);

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "plan it".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(agent.tasks().len(), 2);
        assert_eq!(agent.tasks()[0].content, "step one");
        assert_eq!(agent.tasks()[0].status, TaskStatus::InProgress);

        let mut saw_tasks_updated = false;
        let mut saw_blocked_result = false;
        while let Ok(event) = events_rx.try_recv() {
            match event {
                AgentEvent::TasksUpdated(tasks) => {
                    assert_eq!(tasks.len(), 2);
                    saw_tasks_updated = true;
                }
                AgentEvent::ToolCallResult { is_error: true, .. } => saw_blocked_result = true,
                _ => {}
            }
        }
        assert!(saw_tasks_updated, "expected a TasksUpdated event");
        assert!(
            !saw_blocked_result,
            "write_tasks must not be blocked by the plan gate"
        );
    }

    #[test]
    fn parse_tasks_rejects_empty_list() {
        let err = parse_tasks(&serde_json::json!({"tasks": []})).unwrap_err();
        assert!(err.contains("non-empty"));
    }

    #[test]
    fn parse_tasks_rejects_unknown_status() {
        let err = parse_tasks(&serde_json::json!({
            "tasks": [{"content": "x", "status": "done"}]
        }))
        .unwrap_err();
        assert!(err.contains("unknown task status"));
    }

    #[test]
    fn parse_tasks_reads_content_and_status() {
        let tasks = parse_tasks(&serde_json::json!({
            "tasks": [
                {"content": "a", "status": "completed"},
                {"content": "b", "status": "pending"},
            ]
        }))
        .unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].status, TaskStatus::Completed);
        assert_eq!(tasks[1].status, TaskStatus::Pending);
    }

    #[test]
    fn finds_fallback_tool_call_that_is_the_whole_message() {
        let known: HashSet<String> = ["write_file".to_string()].into_iter().collect();
        let text = r#"{"name": "write_file", "arguments": {"path": "a.txt", "content": "hi"}}"#;
        let (name, args, before, after) = find_fallback_tool_call(text, &known).unwrap();
        assert_eq!(name, "write_file");
        assert_eq!(args["path"], "a.txt");
        assert!(before.is_empty());
        assert!(after.is_empty());
    }

    #[test]
    fn finds_fallback_tool_call_with_leading_prose() {
        let known: HashSet<String> = ["write_file".to_string()].into_iter().collect();
        let text = "Sure, I'll create that file now.\n\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"a.txt\"}}";
        let (name, _args, before, after) = find_fallback_tool_call(text, &known).unwrap();
        assert_eq!(name, "write_file");
        assert_eq!(before, "Sure, I'll create that file now.");
        assert!(after.is_empty());
    }

    #[test]
    fn ignores_json_naming_an_unregistered_tool() {
        let known: HashSet<String> = ["write_file".to_string()].into_iter().collect();
        let text = r#"{"name": "delete_everything", "arguments": {}}"#;
        assert!(find_fallback_tool_call(text, &known).is_none());
    }

    #[test]
    fn ignores_plain_text_with_no_json() {
        let known: HashSet<String> = ["write_file".to_string()].into_iter().collect();
        assert!(find_fallback_tool_call("just a normal reply", &known).is_none());
    }

    /// The flat envelope the system prompt asks for when a model has no
    /// structured tool channel: the remaining top-level fields are the
    /// arguments.
    #[test]
    fn finds_the_flat_action_envelope() {
        let known: HashSet<String> = ["web_search".to_string()].into_iter().collect();
        let text = r#"{"action": "web_search", "query": "rust 2024 edition"}"#;
        let (name, args, before, after) = find_fallback_tool_call(text, &known).unwrap();
        assert_eq!(name, "web_search");
        assert_eq!(args, serde_json::json!({"query": "rust 2024 edition"}));
        assert!(before.is_empty());
        assert!(after.is_empty());
    }

    #[test]
    fn action_envelope_keeps_every_field_but_the_action_itself() {
        let known: HashSet<String> = ["web_search".to_string()].into_iter().collect();
        let text = r#"{"action": "web_search", "query": "rust", "num_results": 5}"#;
        let (_name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
        assert_eq!(args, serde_json::json!({"query": "rust", "num_results": 5}));
    }

    /// The two envelopes crossed. A model writing this means the inner object,
    /// not a literal `arguments` argument.
    #[test]
    fn action_envelope_unwraps_a_nested_arguments_object() {
        let known: HashSet<String> = ["web_search".to_string()].into_iter().collect();
        let text = r#"{"action": "web_search", "arguments": {"query": "rust"}}"#;
        let (_name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
        assert_eq!(args, serde_json::json!({"query": "rust"}));
    }

    /// The registered-tool check is the whole safety property: an `action`
    /// field is common enough in ordinary JSON that dispatching on it blindly
    /// would turn quoted data into tool calls.
    #[test]
    fn ignores_an_action_naming_an_unregistered_tool() {
        let known: HashSet<String> = ["web_search".to_string()].into_iter().collect();
        let text = r#"{"action": "delete_everything", "path": "/"}"#;
        assert!(find_fallback_tool_call(text, &known).is_none());
    }

    #[test]
    fn finds_the_action_envelope_after_prose() {
        let known: HashSet<String> = ["web_search".to_string()].into_iter().collect();
        let text = "I need to look this up.\n\n{\"action\": \"web_search\", \"query\": \"rust\"}";
        let (name, _args, before, after) = find_fallback_tool_call(text, &known).unwrap();
        assert_eq!(name, "web_search");
        assert_eq!(before, "I need to look this up.");
        assert!(after.is_empty());
    }

    /// End to end through a real turn: the model replies with nothing but the
    /// JSON envelope, and the search actually runs instead of being left on
    /// screen as dead text.
    #[tokio::test]
    async fn action_envelope_is_recovered_and_actually_executed() {
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(ScriptedProvider::streams([
            text_reply(r#"{"action": "web_search", "query": "rust 2024 edition"}"#),
            text_reply("Here's what I found."),
        ]));
        let tools = Arc::new(RecordingToolsNamed {
            name: "web_search".to_string(),
            executed: executed.clone(),
        });
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
            .with_permission_policy(PermissionPolicy::Skip);

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "who won yesterday?".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        assert!(
            executed.load(std::sync::atomic::Ordering::SeqCst),
            "the JSON action envelope should have dispatched a real search"
        );

        let mut dispatched_query = None;
        while let Ok(event) = events_rx.try_recv() {
            if let AgentEvent::ToolCallStarted {
                tool_name, input, ..
            } = event
            {
                assert_eq!(tool_name, "web_search");
                dispatched_query = input
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
        }
        assert_eq!(dispatched_query.as_deref(), Some("rust 2024 edition"));

        // And the results come back into context as a tool result, which is
        // what the model then synthesises its answer from.
        assert_eq!(
            agent.history().last().unwrap().text(),
            "Here's what I found."
        );
    }

    /// The tool call arrives as plain text content (no `ToolUseStart` at all)
    /// — a local model that doesn't use the structured tool-calling channel,
    /// e.g. Ollama serving a model with weak function-calling.
    #[tokio::test]
    async fn text_only_tool_call_is_recovered_and_actually_executed() {
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(ScriptedProvider::streams([
            text_reply(r#"{"name": "write_file", "arguments": {"path": "a.txt"}}"#),
            text_reply("done"),
        ]));
        let tools = Arc::new(RecordingToolsNamed {
            name: "write_file".to_string(),
            executed: executed.clone(),
        });
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
            .with_permission_policy(PermissionPolicy::Skip);

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "make a file".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        assert!(
            executed.load(std::sync::atomic::Ordering::SeqCst),
            "the text-only tool call should have run for real, not been left as dead text"
        );

        let mut saw_tool_call_started = false;
        while let Ok(event) = events_rx.try_recv() {
            if let AgentEvent::ToolCallStarted { tool_name, .. } = event {
                assert_eq!(tool_name, "write_file");
                saw_tool_call_started = true;
            }
        }
        assert!(saw_tool_call_started);
    }

    /// Like `RecordingTools`, but advertises a real `tool_defs()` entry
    /// under a caller-chosen name — needed so `recover_text_tool_call`'s
    /// known-tool-name check actually matches.
    struct RecordingToolsNamed {
        name: String,
        executed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl ToolExecutor for RecordingToolsNamed {
        fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
            vec![crate::message::ToolDefinition {
                name: self.name.clone(),
                description: "test tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
            Some(PermissionClass::Mutating)
        }

        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            self.executed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            ToolResult::ok("wrote")
        }
    }

    /// For the tests below, which only inspect agent state and never run a
    /// turn — hence the empty script.
    fn fake_agent() -> Agent {
        let provider = Arc::new(ScriptedProvider::streams([]));
        let tools = Arc::new(NoTools);
        let tool_ctx = ToolContext::new(".", "test-session");
        Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
    }

    #[test]
    fn effective_system_is_none_without_system_or_goal() {
        assert!(fake_agent().effective_system().is_none());
    }

    #[test]
    fn effective_system_uses_base_system_when_no_goal_set() {
        let agent = fake_agent().with_system("be concise");
        assert_eq!(agent.effective_system().as_deref(), Some("be concise"));
    }

    #[test]
    fn effective_system_folds_goal_into_base_system() {
        let mut agent = fake_agent().with_system("be concise");
        agent.set_goal(Some("ship the login page".to_string()));
        let system = agent.effective_system().unwrap();
        assert!(system.contains("be concise"));
        assert!(system.contains("ship the login page"));
    }

    #[test]
    fn effective_system_works_with_goal_but_no_base_system() {
        let mut agent = fake_agent();
        agent.set_goal(Some("ship the login page".to_string()));
        assert!(agent
            .effective_system()
            .unwrap()
            .contains("ship the login page"));
    }

    #[test]
    fn clearing_goal_reverts_to_base_system() {
        let mut agent = fake_agent().with_system("be concise");
        agent.set_goal(Some("ship the login page".to_string()));
        agent.set_goal(None);
        assert_eq!(agent.effective_system().as_deref(), Some("be concise"));
    }

    #[test]
    fn effective_system_appends_injected_context_after_the_base_prompt() {
        let mut agent = fake_agent()
            .with_system("be concise")
            .with_context_provider(|| "Current date: 2026-08-05".to_string());
        agent.set_goal(Some("ship the login page".to_string()));

        let system = agent.effective_system().unwrap();
        let base = system.find("be concise").unwrap();
        let date = system.find("Current date: 2026-08-05").unwrap();
        let goal = system.find("ship the login page").unwrap();
        // The static prompt must stay at the front so prefix-based prompt
        // caching isn't invalidated by the volatile segments behind it.
        assert!(base < date && date < goal, "unexpected order in: {system}");
    }

    #[test]
    fn effective_system_recomputes_context_on_every_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let agent = fake_agent()
            .with_system("be concise")
            .with_context_provider(move || {
                format!("call {}", counter.fetch_add(1, Ordering::SeqCst))
            });

        assert!(agent.effective_system().unwrap().contains("call 0"));
        assert!(agent.effective_system().unwrap().contains("call 1"));
    }

    #[test]
    fn effective_system_skips_a_blank_context() {
        let agent = fake_agent()
            .with_system("be concise")
            .with_context_provider(|| "   ".to_string());
        assert_eq!(agent.effective_system().as_deref(), Some("be concise"));
    }

    #[test]
    fn effective_system_works_with_context_but_no_base_system() {
        let agent = fake_agent().with_context_provider(|| "Current date: 2026-08-05".to_string());
        assert_eq!(
            agent.effective_system().as_deref(),
            Some("Current date: 2026-08-05")
        );
    }

    /// Asks for two `slow_tool` calls in one turn, then ends the turn.
    fn two_tool_calls_then_done() -> ScriptedProvider {
        ScriptedProvider::streams([
            tool_calls_reply(&[
                ("call_1", "slow_tool", serde_json::json!({})),
                ("call_2", "slow_tool", serde_json::json!({})),
            ]),
            text_reply("done"),
        ])
    }

    /// Cancels the turn from inside the first tool call — the exact shape of
    /// a user hitting Esc while a tool is running.
    struct CancelOnFirstCallTools {
        cancel: CancellationToken,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl ToolExecutor for CancelOnFirstCallTools {
        fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
            vec![crate::message::ToolDefinition {
                name: "slow_tool".into(),
                description: "test tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
            Some(PermissionClass::ReadOnly)
        }

        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.cancel.cancel();
            ToolResult::ok("first tool finished")
        }
    }

    /// Every `tool_use` block must be answered by a `tool_result`, even when
    /// the turn is cancelled halfway through the round. Without this the next
    /// request is rejected outright ("tool_use ids were found without
    /// tool_result blocks") and the session is unusable — acceptance
    /// criterion #1.
    #[tokio::test]
    async fn cancelling_mid_tool_round_still_answers_every_tool_use() {
        let cancel = CancellationToken::new();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = Arc::new(CancelOnFirstCallTools {
            cancel: cancel.clone(),
            calls: calls.clone(),
        });
        let provider = Arc::new(two_tool_calls_then_done());
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
            .with_permission_policy(PermissionPolicy::Skip);

        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        let completed = agent
            .run_turn("go".into(), events_tx, perm_tx, question_tx, cancel.clone())
            .await;

        assert!(!completed, "a cancelled turn is not a normal completion");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the second tool must not run after cancellation"
        );

        let tool_use_ids = collect_ids(agent.history(), true);
        let tool_result_ids = collect_ids(agent.history(), false);
        assert_eq!(
            tool_use_ids, tool_result_ids,
            "every tool_use must have a matching tool_result"
        );
        assert_eq!(tool_use_ids.len(), 2);

        // The call that never ran must say so rather than look successful.
        let unanswered = agent
            .history()
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == "call_2" => Some((content.clone(), *is_error)),
                _ => None,
            })
            .expect("call_2 must be answered");
        assert!(unanswered.1, "an unrun tool call is an error result");
        assert!(unanswered.0.contains("cancelled"), "got: {}", unanswered.0);
    }

    /// Collects `tool_use` ids (`want_use`) or `tool_result` ids from history.
    fn collect_ids(history: &[Message], want_use: bool) -> Vec<String> {
        history
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } if want_use => Some(id.clone()),
                ContentBlock::ToolResult { tool_use_id, .. } if !want_use => {
                    Some(tool_use_id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Reports progress mid-execution and records the call id it was handed,
    /// standing in for a tool that streams output while it runs.
    struct ProgressingTool {
        seen_call_id: std::sync::Mutex<Option<String>>,
    }

    #[async_trait]
    impl ToolExecutor for ProgressingTool {
        fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
            vec![crate::message::ToolDefinition {
                name: "slow_tool".into(),
                description: "test tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
            Some(PermissionClass::ReadOnly)
        }

        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            *self.seen_call_id.lock().unwrap() = ctx.tool_call_id().map(str::to_string);
            ctx.report_progress("line one");
            ctx.report_progress("line two");
            ToolResult::ok("finished")
        }
    }

    /// The channel a later task will use to stream `run_bash` output: a tool
    /// must be able to emit lines *between* its start and result events, and
    /// each line must carry the id of the call that produced it — otherwise a
    /// frontend can't attach output to the right call when several tools run
    /// in one round.
    #[tokio::test]
    async fn a_tool_can_report_progress_between_its_start_and_result_events() {
        let tools = Arc::new(ProgressingTool {
            seen_call_id: std::sync::Mutex::new(None),
        });
        let mut agent = Agent::new(
            Arc::new(two_tool_calls_then_done()),
            tools.clone(),
            "fake-model".to_string(),
            ToolContext::new(".", "test-session"),
        )
        .with_permission_policy(PermissionPolicy::Skip);

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "go".into(),
                events_tx,
                perm_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        // The tool learned which call it was without being told explicitly.
        assert_eq!(
            tools.seen_call_id.lock().unwrap().as_deref(),
            Some("call_2"),
            "the context must carry the id of the call currently executing"
        );

        // Ordering is the whole reason progress rides the same channel.
        let sequence: Vec<String> = std::iter::from_fn(|| events_rx.try_recv().ok())
            .filter_map(|e| match e {
                AgentEvent::ToolCallStarted { id, .. } => Some(format!("start:{id}")),
                AgentEvent::ToolProgress { id, line } => Some(format!("progress:{id}:{line}")),
                AgentEvent::ToolCallResult { id, .. } => Some(format!("result:{id}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            sequence,
            vec![
                "start:call_1",
                "progress:call_1:line one",
                "progress:call_1:line two",
                "result:call_1",
                "start:call_2",
                "progress:call_2:line one",
                "progress:call_2:line two",
                "result:call_2",
            ]
        );
    }

    /// The agent's own context is session-long and must stay call-agnostic —
    /// `/model` clones it into a rebuilt agent, so a stale call id or a
    /// channel from a finished turn would outlive its call.
    #[test]
    fn the_agents_own_context_carries_no_call_id() {
        assert!(fake_agent().tool_ctx().tool_call_id().is_none());
    }

    /// A tool that echoes a secret back, standing in for `run_bash {"command":
    /// "env"}` or `cat ~/.smith/config.toml`.
    struct LeakySecretTool {
        secret: String,
    }

    #[async_trait]
    impl ToolExecutor for LeakySecretTool {
        fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
            vec![crate::message::ToolDefinition {
                name: "slow_tool".into(),
                description: "test tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
            Some(PermissionClass::ReadOnly)
        }

        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            ToolResult::ok(format!("ANTHROPIC_API_KEY={}\nPATH=/usr/bin", self.secret))
        }
    }

    /// A leaked key must not survive into history: history is what gets
    /// persisted to SQLite *and* what is sent to the provider on the next
    /// request, so a secret landing there is handed to a third party.
    #[tokio::test]
    async fn a_secret_in_tool_output_never_reaches_history() {
        const SECRET: &str = "sk-ant-api03-supersecretvalue";

        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(
            Arc::new(two_tool_calls_then_done()),
            Arc::new(LeakySecretTool {
                secret: SECRET.to_string(),
            }),
            "fake-model".to_string(),
            tool_ctx,
        )
        .with_permission_policy(PermissionPolicy::Skip)
        .with_redactor(Redactor::new([SECRET.to_string()]));

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "what's in the env?".into(),
                events_tx,
                perm_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        let history = format!("{:?}", agent.history());
        assert!(!history.contains(SECRET), "secret reached history");
        assert!(history.contains(crate::redact::REDACTED));
        // Everything else in the output has to survive, or redaction would be
        // destroying the tool result it's protecting.
        assert!(history.contains("PATH=/usr/bin"));

        // The transcript the user sees comes from these events, not history.
        let mut saw_result = false;
        while let Ok(event) = events_rx.try_recv() {
            if let AgentEvent::ToolCallResult { output, .. } = event {
                saw_result = true;
                assert!(!output.contains(SECRET), "secret reached the transcript");
            }
        }
        assert!(saw_result, "expected at least one tool result event");
    }

    /// A tool call cut off mid-stream was never dispatched and its arguments
    /// may be truncated JSON — it must not reach history, or it becomes a
    /// dangling `tool_use` that breaks every later request. The script is
    /// spelled out rather than built from a helper: a half-emitted call with
    /// truncated input and no `ToolUseComplete` is exactly what makes this
    /// case interesting.
    #[tokio::test]
    async fn cancelling_mid_stream_drops_the_half_built_tool_call() {
        let provider = ScriptedProvider::streams([vec![
            StreamEvent::TextDelta("let me check ".to_string()),
            StreamEvent::ToolUseStart {
                id: "half_call".to_string(),
                name: "slow_tool".to_string(),
            },
            StreamEvent::ToolUseInputDelta {
                id: "half_call".to_string(),
                partial_json: "{\"pa".to_string(),
            },
            StreamEvent::MessageComplete {
                stop_reason: StopReason::Cancelled,
                usage: crate::message::Usage::default(),
            },
        ]]);
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(
            Arc::new(provider),
            Arc::new(NoTools),
            "fake-model".to_string(),
            tool_ctx,
        );

        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        let completed = agent
            .run_turn(
                "go".into(),
                events_tx,
                perm_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        assert!(!completed, "cancelled is not a normal completion");
        assert!(
            collect_ids(agent.history(), true).is_empty(),
            "no dangling tool_use may survive a cancelled stream"
        );
        // The text the model did manage to produce is still worth keeping.
        assert!(agent
            .history()
            .iter()
            .any(|m| m.text().contains("let me check")));
    }

    // ---- turn limits and provider retry -------------------------------

    fn api_error(status: u16, retry_after: Option<Duration>) -> ProviderError {
        ProviderError::Api {
            status,
            message: "boom".into(),
            retry_after,
        }
    }

    /// A sleeper that records what it was asked to wait for and returns
    /// immediately. The schedule is seconds by design, and a suite that lives
    /// through it is a suite nobody runs.
    fn recording_sleeper() -> (
        Arc<std::sync::Mutex<Vec<Duration>>>,
        impl Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = log.clone();
        (log, move |d| {
            sink.lock().unwrap().push(d);
            Box::pin(std::future::ready(()))
        })
    }

    fn agent_for(provider: Arc<ScriptedProvider>, tools: Arc<dyn ToolExecutor>) -> Agent {
        Agent::new(
            provider,
            tools,
            "fake-model".to_string(),
            ToolContext::new(".", "test-session"),
        )
        .with_permission_policy(PermissionPolicy::Skip)
    }

    /// Runs one turn against throwaway channels and hands back everything the
    /// turn emitted, so a test can assert on the event stream as a whole.
    async fn run_collect(
        agent: &mut Agent,
        text: &str,
        cancel: CancellationToken,
    ) -> (bool, Vec<AgentEvent>) {
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();
        let completed = agent
            .run_turn(text.to_string(), events_tx, perm_tx, question_tx, cancel)
            .await;
        let events = std::iter::from_fn(|| events_rx.try_recv().ok()).collect();
        (completed, events)
    }

    fn retries(events: &[AgentEvent]) -> Vec<(u32, u64)> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ProviderRetry {
                    attempt, delay_ms, ..
                } => Some((*attempt, *delay_ms)),
                _ => None,
            })
            .collect()
    }

    fn errors(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Error(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    fn limits_hit(events: &[AgentEvent]) -> Vec<TurnLimitKind> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TurnLimitReached { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_rate_limited_request_is_retried_and_the_turn_then_succeeds() {
        let provider = Arc::new(ScriptedProvider::error_then_text(
            api_error(429, None),
            "recovered",
        ));
        let (delays, sleeper) = recording_sleeper();
        let mut agent = agent_for(provider.clone(), Arc::new(NoTools)).with_sleeper(sleeper);

        let (completed, events) = run_collect(&mut agent, "hi", CancellationToken::new()).await;

        assert!(completed, "the retry should have rescued the turn");
        assert_eq!(provider.request_count(), 2);
        assert_eq!(agent.history()[1].text(), "recovered");
        assert_eq!(delays.lock().unwrap().len(), 1, "one backoff, one sleep");
        // The user has to be told *before* the wait, or a backoff is
        // indistinguishable from a hang.
        assert_eq!(retries(&events).len(), 1);
        assert!(errors(&events).is_empty(), "a rescued turn is not an error");
    }

    /// Replaying a contract error can never succeed — it only spends quota and
    /// delays the one useful thing, telling the user what is wrong.
    #[tokio::test]
    async fn a_bad_request_is_not_retried() {
        let provider = Arc::new(ScriptedProvider::new([ScriptedResponse::Fail(api_error(
            400, None,
        ))]));
        let (delays, sleeper) = recording_sleeper();
        let mut agent = agent_for(provider.clone(), Arc::new(NoTools)).with_sleeper(sleeper);

        let (completed, events) = run_collect(&mut agent, "hi", CancellationToken::new()).await;

        assert!(!completed);
        assert_eq!(provider.request_count(), 1, "400 must be sent exactly once");
        assert!(delays.lock().unwrap().is_empty());
        assert!(retries(&events).is_empty());
        assert!(errors(&events)[0].contains("400"));
    }

    #[tokio::test]
    async fn retrying_stops_at_the_attempt_cap_and_surfaces_the_error() {
        let policy = RetryPolicy::default();
        // Exactly the budget: the fixture panics on an extra request, so
        // over-retrying fails this test loudly rather than silently.
        let provider = Arc::new(ScriptedProvider::new(
            (0..policy.max_attempts).map(|_| ScriptedResponse::Fail(api_error(503, None))),
        ));
        let (delays, sleeper) = recording_sleeper();
        let mut agent = agent_for(provider.clone(), Arc::new(NoTools))
            .with_retry_policy(policy)
            .with_sleeper(sleeper);

        let (completed, events) = run_collect(&mut agent, "hi", CancellationToken::new()).await;

        assert!(!completed);
        assert_eq!(provider.request_count(), policy.max_attempts as usize);
        assert_eq!(
            delays.lock().unwrap().len(),
            policy.max_attempts as usize - 1
        );
        assert_eq!(retries(&events).len(), policy.max_attempts as usize - 1);
        assert!(errors(&events)[0].contains("503"));
    }

    #[tokio::test]
    async fn retry_after_from_the_server_replaces_the_computed_backoff() {
        let server_delay = Duration::from_secs(7);
        let provider = Arc::new(ScriptedProvider::error_then_text(
            api_error(429, Some(server_delay)),
            "recovered",
        ));
        let (delays, sleeper) = recording_sleeper();
        let mut agent = agent_for(provider.clone(), Arc::new(NoTools)).with_sleeper(sleeper);

        let (completed, events) = run_collect(&mut agent, "hi", CancellationToken::new()).await;

        assert!(completed);
        // Not the ~0.5s the formula would have chosen: the server is the only
        // party that knows when its window actually reopens.
        assert_eq!(*delays.lock().unwrap(), vec![server_delay]);
        assert_eq!(retries(&events), vec![(1, 7000)]);
    }

    /// A provider asking for five minutes is not describing a blip. Sleeping
    /// on it would hold the agent lock and look exactly like a crash, so the
    /// turn fails immediately with the number in the message and the user
    /// decides what to do about it.
    #[tokio::test]
    async fn a_retry_after_beyond_the_cap_fails_fast_instead_of_waiting() {
        let policy = RetryPolicy::default();
        let too_long = policy.max_retry_after + Duration::from_secs(1);
        let provider = Arc::new(ScriptedProvider::new([ScriptedResponse::Fail(api_error(
            429,
            Some(too_long),
        ))]));
        let (delays, sleeper) = recording_sleeper();
        let mut agent = agent_for(provider.clone(), Arc::new(NoTools)).with_sleeper(sleeper);

        let (completed, events) = run_collect(&mut agent, "hi", CancellationToken::new()).await;

        assert!(!completed);
        assert_eq!(provider.request_count(), 1);
        assert!(delays.lock().unwrap().is_empty());
        assert!(errors(&events)[0].contains("retry after 31s"));
    }

    /// Esc during a backoff must take effect now. This one uses the *real*
    /// sleeper on purpose — an injected one could never catch a select! that
    /// waits for the timer before noticing the token.
    #[tokio::test]
    async fn cancelling_during_a_backoff_does_not_wait_the_sleep_out() {
        let provider = Arc::new(ScriptedProvider::new([ScriptedResponse::Fail(api_error(
            429,
            Some(Duration::from_secs(25)),
        ))]));
        let mut agent = agent_for(provider.clone(), Arc::new(NoTools));

        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });

        let started = Instant::now();
        let (completed, _events) = run_collect(&mut agent, "hi", cancel).await;

        assert!(!completed);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} — cancellation lost the race with a 25s sleep",
            started.elapsed()
        );
        assert_eq!(provider.request_count(), 1);
    }

    /// Counts its calls, and optionally takes a while — enough to stand in for
    /// both a runaway loop and a slow command.
    struct CountingTools {
        calls: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl CountingTools {
        fn new(delay: Duration) -> (Arc<Self>, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    calls: calls.clone(),
                    delay,
                }),
                calls,
            )
        }
    }

    #[async_trait]
    impl ToolExecutor for CountingTools {
        fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
            vec![crate::message::ToolDefinition {
                name: "slow_tool".into(),
                description: "test tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
            Some(PermissionClass::ReadOnly)
        }

        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            ToolResult::ok("ok")
        }
    }

    /// The runaway case: a model that asks for a tool every single round. The
    /// cap has to stop it *and* leave history usable, or the next request is
    /// rejected for dangling `tool_use` blocks and the session is dead.
    #[tokio::test]
    async fn the_round_cap_stops_a_model_that_never_stops_calling_tools() {
        const MAX_ROUNDS: u32 = 3;
        let provider =
            Arc::new(ScriptedProvider::streams((0..MAX_ROUNDS).map(|i| {
                tool_call_reply(&format!("call_{i}"), "slow_tool", json_empty())
            })));
        let (tools, calls) = CountingTools::new(Duration::ZERO);
        let mut agent = agent_for(provider.clone(), tools).with_max_turns(MAX_ROUNDS);

        let (completed, events) = run_collect(&mut agent, "go", CancellationToken::new()).await;

        assert!(!completed, "a capped turn is not a normal completion");
        assert_eq!(provider.request_count(), MAX_ROUNDS as usize);
        assert_eq!(provider.remaining(), 0, "no request beyond the cap");
        assert_eq!(calls.load(Ordering::SeqCst), MAX_ROUNDS as usize);
        assert_eq!(limits_hit(&events), vec![TurnLimitKind::Rounds]);

        // The invariant the whole exit path exists to protect.
        assert_eq!(
            collect_ids(agent.history(), true),
            collect_ids(agent.history(), false),
            "every tool_use must have a matching tool_result"
        );
        // And the model is told why it stopped, in the same message.
        assert!(agent
            .history()
            .last()
            .unwrap()
            .text()
            .contains("stopped automatically"));
    }

    /// Rounds and calls diverge the moment a model batches calls, so the call
    /// budget is the only one that can bite mid-round — and the calls it
    /// refuses still have to be answered.
    #[tokio::test]
    async fn the_tool_call_budget_refuses_the_rest_of_the_round_and_answers_them() {
        let provider = Arc::new(ScriptedProvider::streams([tool_calls_reply(&[
            ("call_1", "slow_tool", json_empty()),
            ("call_2", "slow_tool", json_empty()),
        ])]));
        let (tools, calls) = CountingTools::new(Duration::ZERO);
        let mut agent = agent_for(provider, tools).with_max_tool_calls_per_turn(1);

        let (completed, events) = run_collect(&mut agent, "go", CancellationToken::new()).await;

        assert!(!completed);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "budget was one call");
        assert_eq!(limits_hit(&events), vec![TurnLimitKind::ToolCalls]);
        assert_eq!(
            collect_ids(agent.history(), true),
            collect_ids(agent.history(), false)
        );
        // The refused call must say it was refused, not that the user cancelled.
        let refused = tool_result_for(agent.history(), "call_2");
        assert!(refused.contains("tool-call budget"), "got: {refused}");
    }

    #[tokio::test]
    async fn the_wall_clock_cap_stops_a_turn_made_of_slow_tools() {
        let provider = Arc::new(ScriptedProvider::streams([tool_call_reply(
            "call_1",
            "slow_tool",
            json_empty(),
        )]));
        let (tools, calls) = CountingTools::new(Duration::from_millis(20));
        let mut agent =
            agent_for(provider.clone(), tools).with_max_wall_clock(Duration::from_millis(5));

        let (completed, events) = run_collect(&mut agent, "go", CancellationToken::new()).await;

        assert!(!completed);
        assert_eq!(limits_hit(&events), vec![TurnLimitKind::WallClock]);
        // The cap bounds further rounds; it never abandons a tool already
        // running, and never prevents the turn from doing anything at all.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.request_count(), 1);
        assert_eq!(
            collect_ids(agent.history(), true),
            collect_ids(agent.history(), false)
        );
    }

    fn json_empty() -> serde_json::Value {
        serde_json::json!({})
    }

    fn tool_result_for(history: &[Message], id: &str) -> String {
        history
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == id => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{id} was never answered"))
    }
}
