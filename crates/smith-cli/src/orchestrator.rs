//! Owns "which provider/model" and the `Action` → `Agent` dispatch loop:
//! `run_orchestrator` is the async task that receives every `Action` the TUI
//! sends and drives the shared `Agent` accordingly (spawning one task per
//! action so a long-running turn never blocks e.g. `CancelGeneration` from
//! being handled). `main.rs` only builds the initial state and hands off to
//! this loop; prompt text/parsing lives in `prompts.rs`.

use std::sync::Arc;

use clap::ValueEnum;
use smith_core::{
    Action, Agent, AgentEvent, AgentPhase, LlmProvider, Message, PermissionAsk, QuestionAsk,
    Redactor, ToolContext,
};
use smith_provider::{AnthropicProvider, OpenAiProvider};
use smith_store::{Config, SessionStore};
use smith_tools::ToolRegistry;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::prompts::{
    build_approved_plan_prompt, build_loop_task_prompt, environment_now, last_assistant_plan_text,
    loop_turn_is_done, LOOP_CONTINUE_PROMPT, SYSTEM_PROMPT,
};

/// Iteration cap applied when `/loop` is given no explicit count — a safety
/// net so a stuck task can't run away with the user's API budget.
const DEFAULT_LOOP_MAX_ITERATIONS: u32 = 25;
/// Ceiling an explicit `/loop <N>` count is clamped to, for the same reason.
const HARD_LOOP_MAX_ITERATIONS: u32 = 100;

pub fn uuid_fallback() -> String {
    format!("local-{}", std::process::id())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderKind {
    Anthropic,
    Openai,
    Ollama,
}

impl ProviderKind {
    pub fn default_model(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "claude-sonnet-5",
            ProviderKind::Openai => "gpt-4.1",
            ProviderKind::Ollama => "llama3.2",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Openai => "openai",
            ProviderKind::Ollama => "ollama",
        }
    }

    pub fn from_config_str(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(ProviderKind::Anthropic),
            "openai" => Some(ProviderKind::Openai),
            "ollama" => Some(ProviderKind::Ollama),
            _ => None,
        }
    }
}

pub fn build_provider(kind: ProviderKind, config: &Config) -> Result<Arc<dyn LlmProvider>, String> {
    match kind {
        ProviderKind::Anthropic => {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .or_else(|| config.anthropic.api_key.clone())
                .ok_or_else(|| {
                    "No Anthropic API key found. Run `smith setup`, or export ANTHROPIC_API_KEY."
                        .to_string()
                })?;
            Ok(Arc::new(AnthropicProvider::new(key)))
        }
        ProviderKind::Openai => {
            let key = std::env::var("OPENAI_API_KEY")
                .ok()
                .or_else(|| config.openai.api_key.clone())
                .ok_or_else(|| {
                    "No OpenAI API key found. Run `smith setup`, or export OPENAI_API_KEY."
                        .to_string()
                })?;
            Ok(Arc::new(OpenAiProvider::new(key)))
        }
        ProviderKind::Ollama => {
            let base_url = config
                .ollama
                .base_url
                .clone()
                .unwrap_or_else(|| smith_store::DEFAULT_OLLAMA_BASE_URL.to_string());
            Ok(Arc::new(OpenAiProvider::ollama(base_url)))
        }
    }
}

/// Every credential this process has loaded, from either source, so tool
/// output can be scrubbed of them before it is shown, stored or sent onward.
///
/// Both the environment and the config file are read, because either can be
/// the live one for a given provider (`build_provider` prefers the env var)
/// and a stale value in the other is still a real secret worth hiding.
fn secret_redactor(config: &Config) -> Redactor {
    let from_env = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "EXA_API_KEY"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok());

    let from_config = [
        config.anthropic.api_key.clone(),
        config.openai.api_key.clone(),
        config.exa.api_key.clone(),
    ]
    .into_iter()
    .flatten();

    Redactor::new(from_env.chain(from_config))
}

pub struct Persistence {
    pub store: SessionStore,
    /// None until the first message is actually sent — see `resolve_session`.
    pub session_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub cwd: String,
    pub persisted: usize,
}

impl Persistence {
    /// The session row this process is writing to, creating it lazily on
    /// first use (a fresh start doesn't create one just for being launched).
    fn ensure_session_id(&mut self) -> String {
        match &self.session_id {
            Some(id) => {
                let _ = self
                    .store
                    .ensure_session(id, &self.provider, &self.model, &self.cwd);
                id.clone()
            }
            None => {
                let id = self
                    .store
                    .create_session(&self.provider, &self.model, &self.cwd)
                    .unwrap_or_else(|_| uuid_fallback());
                self.session_id = Some(id.clone());
                id
            }
        }
    }

    fn persist_new_messages(&mut self, history: &[Message]) {
        if self.persisted >= history.len() {
            return;
        }
        let session_id = self.ensure_session_id();
        for msg in &history[self.persisted..] {
            let _ = self.store.append_message(&session_id, msg);
        }
        self.persisted = history.len();
    }

    /// Persists the goal against this specific session row — never the
    /// project as a whole, so it can't leak into an unrelated conversation.
    fn save_goal(&mut self, goal: Option<&str>) {
        let session_id = self.ensure_session_id();
        let _ = self.store.set_goal(&session_id, goal);
    }
}

/// Connects to every configured MCP server and registers its tools into
/// `tools`, each under the namespaced `mcp__{server}__{tool}` name so a remote
/// server can't shadow a built-in or another server. A server that fails to
/// connect or list tools is skipped (with an error surfaced in the UI) rather
/// than aborting startup — one bad server shouldn't take down the whole
/// session; an individual tool whose name is somehow still taken is skipped
/// the same way.
async fn connect_mcp_servers(
    config: &Config,
    tools: &mut ToolRegistry,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    for server in &config.mcp_servers {
        let client = match smith_mcp::McpClient::connect(
            &server.name,
            &server.command,
            &server.args,
        )
        .await
        {
            Ok(client) => Arc::new(client),
            Err(e) => {
                let _ = event_tx.send(AgentEvent::Error(format!(
                    "mcp server '{}': failed to connect: {e}",
                    server.name
                )));
                continue;
            }
        };

        match client.list_tools().await {
            Ok(defs) => {
                for def in defs {
                    let remote_name = def.name.clone();
                    let adapter = Arc::new(smith_mcp::McpToolAdapter::new(client.clone(), def));
                    if let Err(e) = tools.try_register(adapter) {
                        let _ = event_tx.send(AgentEvent::Error(format!(
                            "mcp server '{}': skipping tool '{remote_name}': {e}",
                            server.name
                        )));
                    }
                }
            }
            Err(e) => {
                let _ = event_tx.send(AgentEvent::Error(format!(
                    "mcp server '{}': failed to list tools: {e}",
                    server.name
                )));
            }
        }
    }
}

struct OrchestratorState {
    agent: Agent,
    persistence: Option<Persistence>,
    provider_kind: ProviderKind,
    tools: Arc<ToolRegistry>,
}

impl OrchestratorState {
    /// Rebuilds the agent against a (possibly new) provider/model, carrying
    /// the conversation history over so switching mid-chat doesn't lose
    /// context.
    fn switch_model(
        &mut self,
        provider: Option<String>,
        model: String,
        config: &Config,
    ) -> Result<(&'static str, String), String> {
        let new_kind = match provider.as_deref() {
            Some(p) => {
                ProviderKind::from_config_str(p).ok_or_else(|| format!("unknown provider: {p}"))?
            }
            None => self.provider_kind,
        };
        let new_provider = build_provider(new_kind, config)?;

        let history = self.agent.history().to_vec();
        let permission_policy = self.agent.permission_policy();
        let plan_gated = self.agent.plan_gated();
        let goal = self.agent.goal().map(str::to_string);
        let tool_ctx = self.agent.tool_ctx().clone();
        // Session state that isn't part of the conversation but is still the
        // user's: tools they approved with "allow always" (revoking those
        // silently would re-prompt for work they already signed off on) and
        // the task checklist (the TUI keeps its own copy, so dropping the
        // agent's made the two disagree).
        let allowed = self.agent.allowed_session_tools().clone();
        let tasks = self.agent.tasks().to_vec();

        let mut agent = Agent::new(new_provider, self.tools.clone(), model.clone(), tool_ctx)
            .with_system(SYSTEM_PROMPT)
            .with_context_provider(environment_now)
            // Rebuilt from config rather than carried over, so a key added
            // since startup is covered too. Forgetting it here would silently
            // disable redaction for the rest of the session.
            .with_redactor(secret_redactor(config))
            .with_permission_policy(permission_policy);
        agent.set_plan_gated(plan_gated);
        agent.set_goal(goal);
        agent.seed_history(history);
        agent.seed_allowed_session_tools(allowed);
        agent.seed_tasks(tasks);

        self.agent = agent;
        self.provider_kind = new_kind;
        Ok((new_kind.label(), model))
    }
}

/// Drives a `/loop` run: repeats turns against the shared agent until the
/// model self-reports done, the iteration cap is hit, or the turn is
/// cancelled/fails. Holds one `CancellationToken` across every iteration so
/// Esc cancels the whole loop, not just the turn in flight.
#[allow(clippy::too_many_arguments)]
async fn run_loop(
    state: Arc<Mutex<OrchestratorState>>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    permission_tx: mpsc::UnboundedSender<PermissionAsk>,
    question_tx: mpsc::UnboundedSender<QuestionAsk>,
    cancel: CancellationToken,
    prompt: String,
    max_iterations: Option<u32>,
) {
    let max_iterations = max_iterations
        .unwrap_or(DEFAULT_LOOP_MAX_ITERATIONS)
        .clamp(1, HARD_LOOP_MAX_ITERATIONS);

    let mut iterations = 0u32;
    let mut reason = smith_core::LoopStopReason::MaxIterations;

    while iterations < max_iterations {
        iterations += 1;
        let _ = event_tx.send(AgentEvent::LoopIterationStarted {
            iteration: iterations,
            max_iterations,
        });

        let turn_prompt = if iterations == 1 {
            build_loop_task_prompt(&prompt)
        } else {
            LOOP_CONTINUE_PROMPT.to_string()
        };

        let mut guard = state.lock().await;
        let ok = guard
            .agent
            .run_turn(
                turn_prompt,
                event_tx.clone(),
                permission_tx.clone(),
                question_tx.clone(),
                cancel.clone(),
            )
            .await;
        let history = guard.agent.history().to_vec();
        if let Some(p) = guard.persistence.as_mut() {
            p.persist_new_messages(&history);
        }
        drop(guard);

        if cancel.is_cancelled() {
            reason = smith_core::LoopStopReason::Cancelled;
            break;
        }
        if !ok {
            reason = smith_core::LoopStopReason::Failed;
            break;
        }
        if loop_turn_is_done(&history) {
            reason = smith_core::LoopStopReason::Done;
            break;
        }
    }

    let _ = event_tx.send(AgentEvent::LoopFinished { reason, iterations });
}

#[allow(clippy::too_many_arguments)]
pub async fn run_orchestrator(
    provider_kind: ProviderKind,
    model: String,
    config: Config,
    initial_messages: Vec<Message>,
    mut persistence: Option<Persistence>,
    permission_policy: smith_core::PermissionPolicy,
    initial_goal: Option<String>,
    mut action_rx: mpsc::UnboundedReceiver<Action>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    permission_tx: mpsc::UnboundedSender<PermissionAsk>,
    question_tx: mpsc::UnboundedSender<QuestionAsk>,
) {
    let provider = match build_provider(provider_kind, &config) {
        Ok(p) => p,
        Err(err) => {
            let _ = event_tx.send(AgentEvent::Error(err));
            while let Some(action) = action_rx.recv().await {
                if matches!(action, Action::Quit) {
                    break;
                }
            }
            return;
        }
    };

    let mut tools = ToolRegistry::with_builtin_tools();
    if let Some(key) = config.exa.api_key.clone() {
        tools.replace(Arc::new(smith_tools::web_search::WebSearchTool::new(Some(
            key,
        ))));
    }
    connect_mcp_servers(&config, &mut tools, &event_tx).await;
    let tools = Arc::new(tools);

    // Allocate a stable session id up front for staging (and reuse a resumed
    // id). The DB row is still created lazily on first persist via ensure_session.
    let session_id = persistence
        .as_ref()
        .and_then(|p| p.session_id.clone())
        .unwrap_or_else(uuid_fallback);
    if let Some(p) = persistence.as_mut() {
        if p.session_id.is_none() {
            p.session_id = Some(session_id.clone());
        }
    }

    let tool_ctx = ToolContext::new(std::env::current_dir().unwrap_or_default(), session_id);
    let mut agent = Agent::new(provider, tools.clone(), model, tool_ctx)
        .with_system(SYSTEM_PROMPT)
        .with_context_provider(environment_now)
        .with_redactor(secret_redactor(&config))
        .with_permission_policy(permission_policy);
    agent.set_goal(initial_goal);
    let seeded_tasks = crate::last_write_tasks_call(&initial_messages);
    if !initial_messages.is_empty() {
        agent.seed_history(initial_messages);
    }
    if !seeded_tasks.is_empty() {
        agent.seed_tasks(seeded_tasks);
    }
    let state = Arc::new(Mutex::new(OrchestratorState {
        agent,
        persistence,
        provider_kind,
        tools,
    }));

    let mut current_cancel: Option<CancellationToken> = None;

    while let Some(action) = action_rx.recv().await {
        match action {
            Action::SubmitMessage(text) => {
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                // Spawned so CancelGeneration keeps flowing through this loop
                // while the turn is in flight, instead of blocking behind it.
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard
                        .agent
                        .run_turn(text, event_tx, permission_tx, question_tx, cancel)
                        .await;

                    let history = guard.agent.history().to_vec();
                    if let Some(p) = guard.persistence.as_mut() {
                        p.persist_new_messages(&history);
                    }
                });
            }
            Action::CancelGeneration => {
                if let Some(cancel) = current_cancel.take() {
                    cancel.cancel();
                }
            }
            Action::SwitchModel {
                provider,
                model,
                save,
            } => {
                // Spawned (like SubmitMessage) so it doesn't block this loop
                // — e.g. behind a turn that's still holding the agent lock.
                let state = state.clone();
                let event_tx = event_tx.clone();
                let config = config.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    match guard.switch_model(provider, model, &config) {
                        Ok((provider_label, model_label)) => {
                            let saved = if save {
                                let mut to_save = config.clone();
                                to_save.general.provider = Some(provider_label.to_string());
                                to_save.general.model = Some(model_label.clone());
                                to_save.save().is_ok()
                            } else {
                                false
                            };
                            let _ = event_tx.send(AgentEvent::ModelChanged {
                                provider: provider_label.to_string(),
                                model: model_label,
                                saved,
                            });
                        }
                        Err(err) => {
                            let _ = event_tx.send(AgentEvent::Error(err));
                        }
                    }
                });
            }
            Action::SetPermissionPolicy { policy, save } => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                let config = config.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard.agent.set_permission_policy(policy);

                    let saved = if save {
                        let mut to_save = config.clone();
                        to_save.general.permission_policy = Some(policy.as_str().to_string());
                        to_save.save().is_ok()
                    } else {
                        false
                    };
                    let _ = event_tx.send(AgentEvent::PermissionPolicyChanged { policy, saved });
                });
            }
            Action::StartPlan(description) => {
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                tokio::spawn(async move {
                    let prompt = format!(
                        "Planning request. Produce a structured plan (numbered steps, risks, and affected files) for the following task. Do not write, edit, or execute anything yet — only use read-only tools if you need to inspect the codebase first, and end your reply with the plan as plain text.\n\nTask: {description}"
                    );

                    let mut guard = state.lock().await;
                    guard.agent.set_plan_gated(true);
                    let _ = event_tx.send(AgentEvent::PlanGateChanged { gated: true });
                    let _ = event_tx.send(AgentEvent::PhaseChanged(AgentPhase::Planning));
                    guard
                        .agent
                        .run_turn(prompt, event_tx.clone(), permission_tx, question_tx, cancel)
                        .await;

                    let history = guard.agent.history().to_vec();
                    if let Some(p) = guard.persistence.as_mut() {
                        p.persist_new_messages(&history);
                    }
                });
            }
            Action::ApprovePlan => {
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard.agent.set_plan_gated(false);
                    let _ = event_tx.send(AgentEvent::PlanGateChanged { gated: false });
                    let _ = event_tx.send(AgentEvent::PhaseChanged(AgentPhase::Building));
                    let plan_text = last_assistant_plan_text(guard.agent.history());
                    let prompt = build_approved_plan_prompt(&plan_text);
                    guard
                        .agent
                        .run_turn(prompt, event_tx.clone(), permission_tx, question_tx, cancel)
                        .await;
                    let history = guard.agent.history().to_vec();
                    if let Some(p) = guard.persistence.as_mut() {
                        p.persist_new_messages(&history);
                    }
                });
            }
            Action::RejectPlan => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard.agent.set_plan_gated(false);
                    let _ = event_tx.send(AgentEvent::PlanGateChanged { gated: false });
                });
            }
            Action::SetGoal(goal) => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard.agent.set_goal(goal.clone());
                    if let Some(p) = guard.persistence.as_mut() {
                        p.save_goal(goal.as_deref());
                    }
                    let _ = event_tx.send(AgentEvent::GoalChanged(goal));
                });
            }
            Action::StartLoop {
                prompt,
                max_iterations,
            } => {
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                tokio::spawn(async move {
                    run_loop(
                        state,
                        event_tx,
                        permission_tx,
                        question_tx,
                        cancel,
                        prompt,
                        max_iterations,
                    )
                    .await;
                });
            }
            Action::Quit => break,
            Action::PermissionResponse(_) | Action::QuestionResponse(_) => {
                // Resolved directly by smith-tui via the oneshot channels.
            }
        }
    }
}
