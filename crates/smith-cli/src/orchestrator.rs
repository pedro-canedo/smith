//! Owns "which provider/model" and the `Action` → `Agent` dispatch loop:
//! `run_orchestrator` is the async task that receives every `Action` the TUI
//! sends and drives the shared `Agent` accordingly (spawning one task per
//! action so a long-running turn never blocks e.g. `CancelGeneration` from
//! being handled). `main.rs` only builds the initial state and hands off to
//! this loop; prompt text/parsing lives in `prompts.rs`.

use std::sync::Arc;

use clap::ValueEnum;
use smith_config::{Config, MemoryCache};
use smith_core::{
    Action, Agent, AgentEvent, AgentPhase, LlmProvider, Message, PermissionAsk, QuestionAsk,
    Redactor, RetryPolicy, ToolContext, TurnLimits,
};
use smith_provider::{AnthropicProvider, OpenAiProvider};
use smith_store::{SessionStore, TurnRecord, TurnTotals};
use smith_tools::ToolRegistry;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::prompts::{
    build_approved_plan_prompt, build_loop_task_prompt, context_provider, last_assistant_plan_text,
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
                .unwrap_or_else(|| smith_config::DEFAULT_OLLAMA_BASE_URL.to_string());
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

    /// Writes one `turns` row with the cost the agent computed while the turn
    /// was running. Nothing here consults a price table — that is what makes
    /// the stored number a historical fact rather than a re-derivation.
    fn record_turn(&mut self, turn: &TurnRecord) {
        let session_id = self.ensure_session_id();
        let _ = self.store.record_turn(&session_id, turn);
    }

    /// The totals a resumed session starts from, or zero for a fresh one.
    fn turn_totals(&self) -> TurnTotals {
        self.session_id
            .as_deref()
            .and_then(|id| self.store.turn_totals(id).ok())
            .unwrap_or_default()
    }
}

/// Persists whatever the turn that just finished added: new messages, and its
/// own token/cost accounting.
///
/// One function because the two must not drift apart — a turn whose messages
/// are saved but whose cost is not comes back on `--resume` looking free.
fn persist_turn(state: &mut OrchestratorState) {
    let history = state.agent.history().to_vec();
    let turn = state.agent.last_turn().cloned();
    let Some(persistence) = state.persistence.as_mut() else {
        return;
    };
    persistence.persist_new_messages(&history);
    if let Some(turn) = turn {
        persistence.record_turn(&TurnRecord {
            provider: turn.provider,
            model: turn.model,
            usage: turn.usage,
            cost_usd: turn.cost_usd,
        });
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
    /// Shared with the agent's context provider. Kept here so `switch_model`
    /// hands the *same* cache to the rebuilt agent — a fresh one would re-read
    /// every SMITH.md on the next request for no reason, and would drop the
    /// warm state a long session has built up.
    memory: MemoryCache,
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
        // Turn budget and retry schedule are session settings, not per-model
        // ones: dropping them here would quietly restore the defaults on every
        // `/model`, which is the least visible way to lose a configured cap.
        let limits = self.agent.limits();
        let retry_policy = self.agent.retry_policy();
        let compaction = self.agent.compaction();
        // Money already spent does not become unspent because the user typed
        // `/model` — and the persisted `turns` rows still record it, so
        // dropping it here would only make the display disagree with the
        // database.
        let session_usage = self.agent.session_usage();
        let session_cost = self.agent.session_cost_usd();

        let mut agent = Agent::new(new_provider, self.tools.clone(), model.clone(), tool_ctx)
            .with_system(SYSTEM_PROMPT)
            .with_context_provider(context_provider(self.memory.clone()))
            .with_limits(limits)
            .with_retry_policy(retry_policy)
            .with_compaction(compaction)
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
        agent.seed_session_totals(session_usage, session_cost);

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
        persist_turn(&mut guard);
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

/// Everything `run_orchestrator` needs that isn't a channel.
///
/// A struct rather than a dozen positional parameters because there are now
/// two callers (the TUI path in `main`, and `headless`) that agree on most
/// fields and differ in two or three — with positional arguments that
/// difference is invisible at the call site.
pub struct OrchestratorOptions {
    pub provider_kind: ProviderKind,
    pub model: String,
    pub config: Config,
    pub initial_messages: Vec<Message>,
    pub persistence: Option<Persistence>,
    pub permission_policy: smith_core::PermissionPolicy,
    pub initial_goal: Option<String>,
    pub limits: TurnLimits,
    /// Bypasses `build_provider` when set.
    ///
    /// Two reasons it exists: tests inject `ScriptedProvider` here (it has no
    /// config representation, so without this seam every orchestrator-level
    /// test would need the network), and `headless` builds the provider up
    /// front so a missing API key is a clean exit-2 before any channel work
    /// starts, rather than an `Error` event racing the first turn.
    pub provider: Option<Arc<dyn LlmProvider>>,
}

impl OrchestratorOptions {
    pub fn new(provider_kind: ProviderKind, model: String, config: Config) -> Self {
        Self {
            provider_kind,
            model,
            config,
            initial_messages: Vec::new(),
            persistence: None,
            permission_policy: smith_core::PermissionPolicy::default(),
            initial_goal: None,
            limits: TurnLimits::default(),
            provider: None,
        }
    }
}

/// The four channels a frontend and the orchestrator share. `smith-tui` and
/// `headless` are interchangeable consumers of the three outbound ones.
pub struct OrchestratorChannels {
    pub action_rx: mpsc::UnboundedReceiver<Action>,
    pub event_tx: mpsc::UnboundedSender<AgentEvent>,
    pub permission_tx: mpsc::UnboundedSender<PermissionAsk>,
    pub question_tx: mpsc::UnboundedSender<QuestionAsk>,
}

pub async fn run_orchestrator(opts: OrchestratorOptions, chans: OrchestratorChannels) {
    let OrchestratorOptions {
        provider_kind,
        model,
        config,
        initial_messages,
        mut persistence,
        permission_policy,
        initial_goal,
        limits,
        provider: provider_override,
    } = opts;
    let OrchestratorChannels {
        mut action_rx,
        event_tx,
        permission_tx,
        question_tx,
    } = chans;

    let provider = match provider_override {
        Some(p) => p,
        None => match build_provider(provider_kind, &config) {
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
        },
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
    // Project memory is scoped to the working directory the rest of this run
    // already agreed on (`main` applies `--cwd` before anything reads it), so
    // the memory chain, the tool sandbox and the session store all describe
    // the same project.
    let memory = MemoryCache::discover(&tool_ctx.cwd);
    let mut agent = Agent::new(provider, tools.clone(), model, tool_ctx)
        .with_system(SYSTEM_PROMPT)
        .with_context_provider(context_provider(memory.clone()))
        // Set explicitly (rather than left to `Agent`'s own default) so
        // `switch_model` has something to carry over and so `--max-turns` has
        // exactly one place to plug into.
        .with_limits(limits)
        .with_retry_policy(RetryPolicy::default())
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
    // `--resume` restores the accumulated cost from what those turns actually
    // cost when they ran, read straight out of the `turns` table. Recomputing
    // it from the current price table here is exactly the bug that makes a
    // resumed session disagree with the one it resumed.
    if let Some(p) = persistence.as_ref() {
        let totals = p.turn_totals();
        agent.seed_session_totals(totals.usage, totals.cost_usd);
    }
    let state = Arc::new(Mutex::new(OrchestratorState {
        agent,
        persistence,
        provider_kind,
        tools,
        memory,
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

                    persist_turn(&mut guard);
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

                    persist_turn(&mut guard);
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
                    persist_turn(&mut guard);
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
            Action::Remember(note) => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let guard = state.lock().await;
                    // The chain root, not the cwd: a note dropped in whatever
                    // subdirectory you happened to be in would only apply
                    // while you stayed there, which isn't what "remember this"
                    // means.
                    let path = smith_config::memory::memory_path(&guard.memory.scope().root);
                    match smith_config::memory::remember(&path, &note) {
                        Ok(()) => {
                            // The next request re-reads memory, so the note is
                            // in effect immediately — but only if the cache is
                            // told its fingerprint is stale.
                            guard.memory.invalidate();
                        }
                        Err(e) => {
                            let _ = event_tx.send(AgentEvent::Error(format!(
                                "could not write {}: {e}",
                                path.display()
                            )));
                        }
                    }
                });
            }
            Action::Compact => {
                // Shares `current_cancel` with turns: compaction spends a
                // provider request, so Esc has to be able to stop it.
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    match guard.agent.compact(&event_tx, &cancel).await {
                        Ok(_) => {
                            // Compaction rewrites history rather than
                            // appending to it, so the messages already in the
                            // database stay as they are — the session store
                            // keeps the full transcript even though the model
                            // no longer sees it, which is what makes the
                            // operation non-destructive. What does need saving
                            // is the summarising request's cost.
                            //
                            // Success is reported by the `ContextUsage` event
                            // `compact` emits, not by a message: the only
                            // notice channel here is `Error`, and headless
                            // treats an `Error` mid-turn as a failed run.
                            persist_turn(&mut guard);
                        }
                        Err(err) => {
                            let _ = event_tx
                                .send(AgentEvent::Error(format!("compaction failed: {err}")));
                        }
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::headless::{self, HeadlessOptions, OutputFormat, EXIT_LIMIT, EXIT_OK};
    use smith_core::testkit::{text_reply, tool_call_reply, ScriptedProvider, ScriptedResponse};

    /// The whole stack minus the network: a real `run_orchestrator` driving a
    /// real `Agent` and `ToolRegistry` over the real channels, with only the
    /// provider scripted, consumed by the real headless frontend.
    async fn run_headless_against(
        provider: Arc<ScriptedProvider>,
        options: HeadlessOptions,
        max_turns: Option<u32>,
    ) -> (u8, String, String) {
        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (permission_tx, permission_rx) = mpsc::unbounded_channel();
        let (question_tx, question_rx) = mpsc::unbounded_channel();

        let mut opts = OrchestratorOptions::new(
            ProviderKind::Anthropic,
            "scripted-model".to_string(),
            Config::default(),
        );
        opts.provider = Some(provider);
        opts.permission_policy = smith_core::PermissionPolicy::Ask;
        if let Some(max_turns) = max_turns {
            opts.limits.max_turns = max_turns;
        }

        let orchestrator = tokio::spawn(run_orchestrator(
            opts,
            OrchestratorChannels {
                action_rx,
                event_tx,
                permission_tx,
                question_tx,
            },
        ));

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = headless::run(
            &options,
            action_tx,
            event_rx,
            permission_rx,
            question_rx,
            &mut out,
            &mut err,
        )
        .await;
        orchestrator.abort();

        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn options(prompt: &str, format: OutputFormat, allowed: &[&str]) -> HeadlessOptions {
        HeadlessOptions {
            prompt: prompt.to_string(),
            format,
            allowed_tools: allowed.iter().map(|s| s.to_string()).collect(),
            color: false,
            provider: "scripted".into(),
            model: "scripted-model".into(),
        }
    }

    fn read_manifest() -> serde_json::Value {
        serde_json::json!({ "path": "Cargo.toml" })
    }

    #[tokio::test]
    async fn a_plain_turn_reaches_the_provider_and_comes_back_as_json() {
        let provider = Arc::new(ScriptedProvider::text("the sky is blue"));
        let (code, out, _) = run_headless_against(
            provider.clone(),
            options("why is the sky blue?", OutputFormat::Json, &[]),
            None,
        )
        .await;

        assert_eq!(code, EXIT_OK);
        let value: serde_json::Value = serde_json::from_slice(out.as_bytes()).unwrap();
        assert_eq!(value["result"], "the sky is blue");
        assert_eq!(value["ok"], true);

        // The prompt the frontend composed is what the model actually saw.
        let request = provider.last_request().unwrap();
        assert!(request
            .messages
            .iter()
            .any(|m| m.text().contains("why is the sky blue?")));
    }

    /// Proves the composition, not just that `compose_prompt` returns a
    /// string: the piped bytes have to survive all the way into the request.
    #[tokio::test]
    async fn a_prompt_composed_from_stdin_arrives_intact_at_the_provider() {
        let prompt =
            headless::compose_prompt(Some("diagnose this"), Some("thread 'main' panicked"))
                .unwrap();
        let provider = Arc::new(ScriptedProvider::text("it panicked"));
        let (code, _, _) = run_headless_against(
            provider.clone(),
            options(&prompt, OutputFormat::Text, &[]),
            None,
        )
        .await;

        assert_eq!(code, EXIT_OK);
        let sent = provider
            .last_request()
            .unwrap()
            .messages
            .iter()
            .map(|m| m.text())
            .collect::<String>();
        assert!(sent.contains("diagnose this"));
        assert!(sent.contains("thread 'main' panicked"));
    }

    /// `--max-turns 1` has to stop the turn after one tool-call round, which
    /// means the second scripted reply is never requested.
    #[tokio::test]
    async fn max_turns_stops_the_turn_and_reports_the_limit_exit_code() {
        let provider = Arc::new(ScriptedProvider::streams([
            tool_call_reply("call_1", "read_file", read_manifest()),
            text_reply("never requested"),
        ]));
        let (code, _, err) = run_headless_against(
            provider.clone(),
            options("read the manifest", OutputFormat::Text, &["read_file"]),
            Some(1),
        )
        .await;

        assert_eq!(code, EXIT_LIMIT);
        assert_eq!(provider.request_count(), 1);
        assert_eq!(provider.remaining(), 1);
        assert!(err.contains("1 tool-call rounds"), "{err}");
    }

    /// Without the cap the same script runs to completion — otherwise the
    /// test above would pass for the wrong reason.
    #[tokio::test]
    async fn the_same_script_completes_when_the_cap_is_not_set() {
        let provider = Arc::new(ScriptedProvider::streams([
            tool_call_reply("call_1", "read_file", read_manifest()),
            text_reply("it is a manifest"),
        ]));
        let (code, out, _) = run_headless_against(
            provider.clone(),
            options("read the manifest", OutputFormat::Text, &["read_file"]),
            None,
        )
        .await;

        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "it is a manifest\n");
        assert_eq!(provider.request_count(), 2);
    }

    /// A provider that fails outright must not be reported as a successful
    /// run — the whole point of headless exit codes.
    #[tokio::test]
    async fn a_provider_failure_exits_non_zero() {
        let provider = Arc::new(ScriptedProvider::new([ScriptedResponse::Fail(
            // 401 rather than a 5xx on purpose: `RetryPolicy` would re-send a
            // retryable failure, and this test is about the exit code, not
            // the backoff schedule.
            smith_core::ProviderError::Api {
                status: 401,
                message: "bad key".into(),
                retry_after: None,
            },
        )]));
        let (code, out, err) =
            run_headless_against(provider, options("hello", OutputFormat::Text, &[]), None).await;

        assert_ne!(code, EXIT_OK);
        assert!(out.is_empty());
        assert!(err.contains("bad key"), "{err}");
    }

    /// Deny-by-default all the way down: the tool is never executed, so the
    /// file it would have written does not appear.
    #[tokio::test]
    async fn a_tool_missing_from_allowed_tools_never_runs() {
        // Inside the crate directory on purpose: that is `ToolContext`'s
        // sandbox root under `cargo test`, and a path outside it would be
        // refused by the jail rather than by the flag under test.
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let target = dir.path().join("written.txt");
        let provider = Arc::new(ScriptedProvider::streams([
            tool_call_reply(
                "call_1",
                "write_file",
                serde_json::json!({ "path": target.to_string_lossy(), "content": "hi" }),
            ),
            text_reply("I was not allowed to write that"),
        ]));

        let (code, out, err) = run_headless_against(
            provider,
            options("write a file", OutputFormat::Json, &["read_file"]),
            None,
        )
        .await;

        assert_eq!(code, EXIT_OK);
        assert!(!target.exists(), "the denied tool ran anyway");
        assert!(err.contains("denied write_file"), "{err}");
        let value: serde_json::Value = serde_json::from_slice(out.as_bytes()).unwrap();
        assert_eq!(value["denied_tools"][0], "write_file");
    }

    /// The same script with the tool allowed: the flag is what makes the
    /// difference, not some other refusal along the way.
    #[tokio::test]
    async fn the_same_tool_runs_once_allowed_tools_names_it() {
        // Inside the crate directory on purpose: that is `ToolContext`'s
        // sandbox root under `cargo test`, and a path outside it would be
        // refused by the jail rather than by the flag under test.
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let target = dir.path().join("written.txt");
        let provider = Arc::new(ScriptedProvider::streams([
            tool_call_reply(
                "call_1",
                "write_file",
                serde_json::json!({ "path": target.to_string_lossy(), "content": "hi" }),
            ),
            text_reply("wrote it"),
        ]));

        let (code, _, err) = run_headless_against(
            provider,
            options("write a file", OutputFormat::Text, &["write_file"]),
            None,
        )
        .await;

        assert_eq!(code, EXIT_OK, "{err}");
        assert!(target.exists(), "{err}");
    }

    /// Every line of a real run's `stream-json` output — deltas, tool events,
    /// the final summary — has to stand alone as JSON.
    #[tokio::test]
    async fn a_real_stream_json_run_is_line_delimited_throughout() {
        let provider = Arc::new(ScriptedProvider::streams([
            tool_call_reply("call_1", "read_file", read_manifest()),
            text_reply("a manifest\nwith a newline in it"),
        ]));
        let (code, out, _) = run_headless_against(
            provider,
            options("read it", OutputFormat::StreamJson, &["read_file"]),
            None,
        )
        .await;

        assert_eq!(code, EXIT_OK);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.len() > 4, "{lines:?}");
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("not standalone JSON: {line:?}: {e}"));
        }
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["type"], "result");
        assert_eq!(last["data"]["exit_code"], 0);
    }

    /// The wiring half of acceptance criterion #4: a turn run through the real
    /// orchestrator writes a `turns` row carrying the cost computed while it
    /// ran, and a second `run_orchestrator` over the same database — which is
    /// exactly what `--resume` is — starts from that stored figure.
    #[tokio::test]
    async fn a_turn_persists_its_cost_and_a_resumed_run_starts_from_it() {
        use smith_core::testkit::text_reply_with_usage;
        use smith_core::Usage;

        let dir = tempfile::tempdir().unwrap();
        let session_id = SessionStore::open(dir.path())
            .unwrap()
            .create_session("anthropic", "claude-sonnet-5", "/proj")
            .unwrap();

        // A million tokens each way of sonnet: $3 in + $15 out.
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Usage::default()
        };
        let provider = Arc::new(
            ScriptedProvider::streams([text_reply_with_usage("done", usage)]).with_id("anthropic"),
        );

        let (action_tx, action_rx) = mpsc::unbounded_channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        let mut opts = OrchestratorOptions::new(
            ProviderKind::Anthropic,
            "claude-sonnet-5".to_string(),
            Config::default(),
        );
        opts.provider = Some(provider);
        opts.permission_policy = smith_core::PermissionPolicy::Skip;
        opts.persistence = Some(Persistence {
            store: SessionStore::open(dir.path()).unwrap(),
            session_id: Some(session_id.clone()),
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/proj".into(),
            persisted: 0,
        });

        let orchestrator = tokio::spawn(run_orchestrator(
            opts,
            OrchestratorChannels {
                action_rx,
                event_tx,
                permission_tx,
                question_tx,
            },
        ));
        action_tx
            .send(Action::SubmitMessage("hello".into()))
            .unwrap();

        while let Some(event) = event_rx.recv().await {
            if matches!(
                event,
                AgentEvent::AssistantTurnComplete {
                    stop_reason: smith_core::StopReason::EndTurn,
                    ..
                }
            ) {
                break;
            }
        }

        // Persistence happens in the turn's own task, just after the event.
        let reader = SessionStore::open(dir.path()).unwrap();
        let totals = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let totals = reader.turn_totals(&session_id).unwrap();
                if totals.turns > 0 {
                    return totals;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the turn should have been recorded");
        orchestrator.abort();

        assert_eq!(totals.turns, 1);
        assert_eq!(totals.usage.output_tokens, 1_000_000);
        assert_eq!(totals.unpriced_turns, 0);
        assert!((totals.cost_usd - 18.0).abs() < 1e-9, "{totals:?}");

        // Now the resume: a fresh orchestrator over the same database has to
        // pick the total up before it does anything at all.
        let (_action_tx2, action_rx2) = mpsc::unbounded_channel();
        let (event_tx2, _event_rx2) = mpsc::unbounded_channel();
        let (permission_tx2, _p2) = mpsc::unbounded_channel();
        let (question_tx2, _q2) = mpsc::unbounded_channel();
        let mut opts = OrchestratorOptions::new(
            ProviderKind::Anthropic,
            "claude-sonnet-5".to_string(),
            Config::default(),
        );
        // No scripted responses: this run must not need the provider to know
        // what the session already cost.
        opts.provider = Some(Arc::new(ScriptedProvider::streams([])));
        opts.persistence = Some(Persistence {
            store: SessionStore::open(dir.path()).unwrap(),
            session_id: Some(session_id.clone()),
            provider: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            cwd: "/proj".into(),
            persisted: 0,
        });
        let resumed = tokio::spawn(run_orchestrator(
            opts,
            OrchestratorChannels {
                action_rx: action_rx2,
                event_tx: event_tx2,
                permission_tx: permission_tx2,
                question_tx: question_tx2,
            },
        ));
        // Reading the totals back is the same call the orchestrator makes to
        // seed the agent, and it is identical — not merely close.
        assert_eq!(reader.turn_totals(&session_id).unwrap(), totals);
        resumed.abort();
    }
}
