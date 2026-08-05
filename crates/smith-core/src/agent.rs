use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::event::{
    AgentEvent, AgentPhase, PermissionDecision, PermissionRequest, Task, TaskStatus, UserQuestion,
};
use crate::message::{CompletionRequest, ContentBlock, Message, Role, StopReason, StreamEvent};
use crate::permission_detail::format_permission_detail;
use crate::provider::LlmProvider;
use crate::redact::Redactor;
use crate::tool::{PermissionClass, PermissionPolicy, ToolContext, ToolResult};

/// Stand-in result recorded for a tool call the turn never got to run. The
/// model reads it, so it says plainly that nothing happened — an empty or
/// vague result would invite it to assume the call succeeded.
const NOT_EXECUTED_CANCELLED: &str = "not executed — the turn was cancelled by the user";

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

/// Asks the TUI to resolve an `ask_user` question. The oneshot carries the
/// final answer text (one of the three options, or custom input).
pub struct QuestionAsk {
    pub question: UserQuestion,
    pub respond_to: oneshot::Sender<String>,
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
        }
    }

    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
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

    /// If `message` is a single, otherwise-final text reply that's actually
    /// a `{"name": ..., "arguments": {...}}` tool call in disguise (a known
    /// tool name, specifically — not just JSON-shaped text), rebuild it as a
    /// real `ToolUse` so the normal tool-execution path picks it up. `None`
    /// for anything that doesn't match, which is the overwhelming majority
    /// of replies — this only exists for providers/models that fall back to
    /// printing the call instead of using the structured channel.
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

            let stream = match self
                .provider
                .stream_completion(request, cancel.clone())
                .await
            {
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

            self.messages.push(Message {
                role: Role::User,
                content: results,
            });

            if cancelled {
                let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Idle));
                let _ = events.send(AgentEvent::Error("cancelled".into()));
                return false;
            }

            let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Thinking));
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
        let mut result = self
            .tools
            .execute(name, input, &self.tool_ctx, cancel)
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
            answer = rx => answer.unwrap_or_else(|_| "User dismissed the question.".into()),
        };

        let result = ToolResult::ok(format!("User answered: {answer}"));
        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: false,
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

/// Scans `text` left to right for the first `{...}` JSON object shaped like
/// `{"name": "<a known tool>", "arguments": {...}}`, returning its name,
/// arguments, and the (trimmed) text before/after it. Uses a streaming
/// JSON parser to find the object's end rather than hand-rolled brace
/// counting, so it doesn't get confused by braces inside string values.
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
        let name = value.get("name").and_then(|v| v.as_str());
        let arguments = value.get("arguments").filter(|v| v.is_object());
        if let (Some(name), Some(arguments)) = (name, arguments) {
            if known_tools.contains(name) {
                let end = i + de.byte_offset();
                return Some((
                    name.to_string(),
                    arguments.clone(),
                    text[..i].trim().to_string(),
                    text[end..].trim().to_string(),
                ));
            }
        }
    }
    None
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Always replies with a completely empty turn (no text, no tool use) —
    /// exercises the "provider returned nothing" edge case.
    struct EmptyReplyProvider;

    #[async_trait]
    impl LlmProvider for EmptyReplyProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<StreamEvent, crate::provider::ProviderError>,
            >,
            crate::provider::ProviderError,
        > {
            let events = vec![Ok(StreamEvent::MessageComplete {
                stop_reason: StopReason::EndTurn,
                usage: crate::message::Usage::default(),
            })];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn empty_assistant_turn_is_not_pushed_to_history() {
        let provider = Arc::new(EmptyReplyProvider);
        let tools = Arc::new(NoTools);
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
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

    /// Replies with an empty turn twice, then text on the third attempt —
    /// exercises the auto-retry path for providers that stall right after a
    /// tool round instead of writing up the results.
    #[derive(Default)]
    struct FlakyThenTextProvider {
        attempts: std::sync::atomic::AtomicU32,
    }

    #[async_trait]
    impl LlmProvider for FlakyThenTextProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<StreamEvent, crate::provider::ProviderError>,
            >,
            crate::provider::ProviderError,
        > {
            let n = self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if n < 2 {
                vec![Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::EndTurn,
                    usage: crate::message::Usage::default(),
                })]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta("finally".to_string())),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                        usage: crate::message::Usage::default(),
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn empty_turns_are_retried_before_giving_up() {
        let provider = Arc::new(FlakyThenTextProvider::default());
        let tools = Arc::new(NoTools);
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
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

        assert_eq!(
            provider.attempts.load(std::sync::atomic::Ordering::SeqCst),
            3
        );
        assert_eq!(agent.history().len(), 2);
        assert_eq!(agent.history()[1].role, Role::Assistant);
        assert_eq!(agent.history()[1].text(), "finally");
    }

    /// Proposes calling `write_file` (a Mutating tool) on the first request,
    /// then ends the turn with plain text on the next — stateful so
    /// `run_turn`'s tool-use loop actually terminates instead of looping
    /// forever asking to call the same tool again.
    #[derive(Default)]
    struct SingleToolCallProvider {
        called: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl LlmProvider for SingleToolCallProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<StreamEvent, crate::provider::ProviderError>,
            >,
            crate::provider::ProviderError,
        > {
            let events = if self.called.swap(true, std::sync::atomic::Ordering::SeqCst) {
                vec![
                    Ok(StreamEvent::TextDelta("done".to_string())),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                        usage: crate::message::Usage::default(),
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "call_1".to_string(),
                        name: "write_file".to_string(),
                    }),
                    Ok(StreamEvent::ToolUseInputDelta {
                        id: "call_1".to_string(),
                        partial_json: "{}".to_string(),
                    }),
                    Ok(StreamEvent::ToolUseComplete {
                        id: "call_1".to_string(),
                    }),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::ToolUse,
                        usage: crate::message::Usage::default(),
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
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
        let provider = Arc::new(SingleToolCallProvider::default());
        let tools = Arc::new(RecordingTools {
            executed: executed.clone(),
        });
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
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
        let provider = Arc::new(SingleToolCallProvider::default());
        let tools = Arc::new(RecordingTools {
            executed: executed.clone(),
        });
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
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

    /// Calls `write_tasks` with a fixed task list on the first request, then
    /// ends the turn with plain text on the next.
    #[derive(Default)]
    struct WriteTasksProvider {
        called: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl LlmProvider for WriteTasksProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<StreamEvent, crate::provider::ProviderError>,
            >,
            crate::provider::ProviderError,
        > {
            let events = if self.called.swap(true, std::sync::atomic::Ordering::SeqCst) {
                vec![
                    Ok(StreamEvent::TextDelta("done".to_string())),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                        usage: crate::message::Usage::default(),
                    }),
                ]
            } else {
                let json = serde_json::json!({
                    "tasks": [
                        {"content": "step one", "status": "in_progress"},
                        {"content": "step two", "status": "pending"},
                    ]
                })
                .to_string();
                vec![
                    Ok(StreamEvent::ToolUseStart {
                        id: "call_1".to_string(),
                        name: "write_tasks".to_string(),
                    }),
                    Ok(StreamEvent::ToolUseInputDelta {
                        id: "call_1".to_string(),
                        partial_json: json,
                    }),
                    Ok(StreamEvent::ToolUseComplete {
                        id: "call_1".to_string(),
                    }),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::ToolUse,
                        usage: crate::message::Usage::default(),
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn write_tasks_updates_agent_state_even_while_plan_gated() {
        let provider = Arc::new(WriteTasksProvider::default());
        let tools = Arc::new(NoTools);
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
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

    /// Emits the tool call as plain text content (no ToolUseStart at all) —
    /// simulates a local model that doesn't use the structured tool-calling
    /// channel, e.g. Ollama serving a model with weak function-calling.
    struct TextOnlyToolCallProvider {
        called: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl LlmProvider for TextOnlyToolCallProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<StreamEvent, crate::provider::ProviderError>,
            >,
            crate::provider::ProviderError,
        > {
            let events = if self.called.swap(true, std::sync::atomic::Ordering::SeqCst) {
                vec![
                    Ok(StreamEvent::TextDelta("done".to_string())),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                        usage: crate::message::Usage::default(),
                    }),
                ]
            } else {
                vec![
                    Ok(StreamEvent::TextDelta(
                        r#"{"name": "write_file", "arguments": {"path": "a.txt"}}"#.to_string(),
                    )),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                        usage: crate::message::Usage::default(),
                    }),
                ]
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn text_only_tool_call_is_recovered_and_actually_executed() {
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(TextOnlyToolCallProvider {
            called: std::sync::atomic::AtomicBool::new(false),
        });
        let tools = Arc::new(RecordingToolsNamed {
            name: "write_file".to_string(),
            executed: executed.clone(),
        });
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
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

    fn fake_agent() -> Agent {
        let provider = Arc::new(EmptyReplyProvider);
        let tools = Arc::new(NoTools);
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
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

    /// Asks for two tool calls in one turn, then ends the turn.
    struct TwoToolCallProvider {
        called: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl LlmProvider for TwoToolCallProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<StreamEvent, crate::provider::ProviderError>,
            >,
            crate::provider::ProviderError,
        > {
            let events = if self.called.swap(true, std::sync::atomic::Ordering::SeqCst) {
                vec![
                    Ok(StreamEvent::TextDelta("done".to_string())),
                    Ok(StreamEvent::MessageComplete {
                        stop_reason: StopReason::EndTurn,
                        usage: crate::message::Usage::default(),
                    }),
                ]
            } else {
                let mut events = Vec::new();
                for id in ["call_1", "call_2"] {
                    events.push(Ok(StreamEvent::ToolUseStart {
                        id: id.to_string(),
                        name: "slow_tool".to_string(),
                    }));
                    events.push(Ok(StreamEvent::ToolUseInputDelta {
                        id: id.to_string(),
                        partial_json: "{}".to_string(),
                    }));
                    events.push(Ok(StreamEvent::ToolUseComplete { id: id.to_string() }));
                }
                events.push(Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::ToolUse,
                    usage: crate::message::Usage::default(),
                }));
                events
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }
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
        let provider = Arc::new(TwoToolCallProvider {
            called: std::sync::atomic::AtomicBool::new(false),
        });
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
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

        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
        let mut agent = Agent::new(
            Arc::new(TwoToolCallProvider {
                called: std::sync::atomic::AtomicBool::new(false),
            }),
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

    /// Emits a partial tool call and then reports the stream as cancelled —
    /// what a mid-stream Esc looks like.
    struct CancelledMidStreamProvider;

    #[async_trait]
    impl LlmProvider for CancelledMidStreamProvider {
        fn id(&self) -> &'static str {
            "fake"
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<StreamEvent, crate::provider::ProviderError>,
            >,
            crate::provider::ProviderError,
        > {
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::TextDelta("let me check ".to_string())),
                Ok(StreamEvent::ToolUseStart {
                    id: "half_call".to_string(),
                    name: "slow_tool".to_string(),
                }),
                Ok(StreamEvent::ToolUseInputDelta {
                    id: "half_call".to_string(),
                    partial_json: "{\"pa".to_string(),
                }),
                Ok(StreamEvent::MessageComplete {
                    stop_reason: StopReason::Cancelled,
                    usage: crate::message::Usage::default(),
                }),
            ])))
        }
    }

    /// A tool call cut off mid-stream was never dispatched and its arguments
    /// may be truncated JSON — it must not reach history, or it becomes a
    /// dangling `tool_use` that breaks every later request.
    #[tokio::test]
    async fn cancelling_mid_stream_drops_the_half_built_tool_call() {
        let tool_ctx = ToolContext {
            cwd: std::path::PathBuf::from("."),
            session_id: "test-session".into(),
        };
        let mut agent = Agent::new(
            Arc::new(CancelledMidStreamProvider),
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
}
