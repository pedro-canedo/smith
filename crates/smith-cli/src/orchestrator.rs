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
    Action, Agent, AgentEvent, LlmProvider, Message, PermissionAsk, QuestionAsk, Redactor,
    RetryPolicy, TurnLimits,
};
use smith_provider::{AnthropicProvider, FallbackEntry, FallbackProvider, OpenAiProvider};
use smith_store::{SessionStore, TurnRecord, TurnTotals};
use smith_tools::{CheckpointStore, ToolRegistry};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::prompts::{
    build_loop_task_prompt, context_provider, loop_turn_is_done, LOOP_CONTINUE_PROMPT,
};

/// Iteration cap applied when `/loop` is given no explicit count — a safety
/// net so a stuck task can't run away with the user's API budget.
const DEFAULT_LOOP_MAX_ITERATIONS: u32 = 25;
/// Ceiling an explicit `/loop <N>` count is clamped to, for the same reason.
const HARD_LOOP_MAX_ITERATIONS: u32 = 100;

/// The id a new session is filed under.
///
/// A v4 UUID, the same shape `SessionStore::create_session` mints, because
/// this — not that — is what a fresh session actually ends up called: the
/// orchestrator allocates an id before the first turn (the scratch directory
/// needs one) and hands it to `Persistence`, which then `ensure_session`s
/// that id rather than creating a new row.
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The id used when the store itself refuses to mint one.
///
/// Process-derived on purpose: at that point the database is unreachable, so
/// there is nothing to be unique *against* — the id only has to name this
/// process's scratch directory. It must never be the ordinary path, and was:
/// every session in the wild is called `local-<pid>`, and pids are recycled,
/// so two unrelated conversations could be filed under one row and have their
/// histories merged.
pub fn uuid_fallback() -> String {
    format!("local-{}", std::process::id())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderKind {
    Anthropic,
    Openai,
    /// OpenRouter (openrouter.ai) — cloud aggregator with `:free` models.
    Openrouter,
    /// 9Router — a local gateway (localhost:20128) that fans out to 40+
    /// upstream providers with its own internal fallback.
    ///
    /// The clap value name is pinned because identifiers cannot start with a
    /// digit: without it the flag would accept `--provider nine-router`, a
    /// spelling nothing else (config, docs, the gateway itself) uses.
    #[value(name = "9router")]
    NineRouter,
    Ollama,
}

impl ProviderKind {
    pub fn default_model(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "claude-sonnet-5",
            ProviderKind::Openai => "gpt-4.1",
            // Head of the curated free chain — see `smith_store::models`.
            ProviderKind::Openrouter => "nvidia/nemotron-3-ultra-550b-a55b:free",
            // There is no honest default. A gateway serves whatever its owner
            // configured, so any name smith invents is a guess — and the
            // guess used to be `auto`, which gateways resolve to something
            // local and then fail on. `build_provider_stack` refuses rather
            // than reaching this.
            ProviderKind::NineRouter => "",
            // Cloud, not a local weight. `llama3.2` was a model almost
            // nobody had pulled, so the last-resort default failed with "model
            // not found"; this one needs no download and no VRAM, and when it
            // fails it is because the daemon is signed out — which now says so
            // and names the free, cardless fix.
            ProviderKind::Ollama => "nemotron-3-super:cloud",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Openai => "openai",
            ProviderKind::Openrouter => "openrouter",
            ProviderKind::NineRouter => "9router",
            ProviderKind::Ollama => "ollama",
        }
    }

    pub fn from_config_str(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(ProviderKind::Anthropic),
            "openai" => Some(ProviderKind::Openai),
            "openrouter" => Some(ProviderKind::Openrouter),
            "9router" => Some(ProviderKind::NineRouter),
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
        ProviderKind::Openrouter => {
            let key = std::env::var("OPENROUTER_API_KEY")
                .ok()
                .or_else(|| config.openrouter.api_key.clone())
                .ok_or_else(|| {
                    "No OpenRouter API key found. Run `smith setup` (a free key takes a minute \
                     at https://openrouter.ai/keys), or export OPENROUTER_API_KEY."
                        .to_string()
                })?;
            let base_url = config
                .openrouter
                .base_url
                .clone()
                .unwrap_or_else(|| smith_config::DEFAULT_OPENROUTER_BASE_URL.to_string());
            Ok(Arc::new(
                OpenAiProvider::openrouter(key, base_url)
                    .with_fallback_models(config.openrouter.fallback_models.clone()),
            ))
        }
        ProviderKind::NineRouter => {
            // The dashboard key is required by the gateway — verified against
            // 9router@0.5.50: `/v1/models` answers keyless (which is what the
            // health check uses), but `/v1/chat/completions` is 401 "Missing
            // API key". So a missing key fails here, naming where the key
            // lives, rather than sending a placeholder that would bounce with
            // a confusing 401 on the first message.
            let key = std::env::var("NINEROUTER_API_KEY")
                .ok()
                .or_else(|| config.nine_router.api_key.clone())
                .ok_or_else(|| {
                    "No 9Router API key found. Run `smith setup`, or copy one from the local \
                     dashboard (http://localhost:20128) and export NINEROUTER_API_KEY."
                        .to_string()
                })?;
            let base_url = config
                .nine_router
                .base_url
                .clone()
                .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());
            Ok(Arc::new(OpenAiProvider::nine_router(key, base_url)))
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

/// The provider the session actually drives: the primary alone when no
/// `[fallback] providers` chain is configured (today's behavior, zero cost),
/// or a `FallbackProvider` wrapping the primary plus each configured entry.
///
/// Errors are the design: an unknown provider name in the chain, or a chain
/// entry whose key is missing, fails **naming the problem** instead of
/// silently skipping the entry — a fallback that quietly is not there is
/// discovered at the worst possible moment, which is the one moment it was
/// configured for.
/// `model` is the **resolved** model for this session — `--model`, then
/// `/model`, then `[general] model`, then the provider's default. It has to be
/// passed in rather than read from config here, because `FallbackProvider`
/// overwrites each request's model with the entry's
/// (`fallback.rs`: `request.model = entry.model.clone()`), so whatever this
/// function puts on the primary entry is what actually gets asked for.
///
/// It used to read `[general] model` and carried a comment claiming the
/// request drove the primary and this was "a placeholder overwritten per
/// request by the wrapper". The overwrite goes the other way. The effect was
/// that configuring any fallback chain silently disabled `--model` and
/// `/model`: a session asking for one model sent the name of another, and on
/// a machine whose `[general] model` named a gateway-specific id, that name
/// was sent to Ollama, which had never heard of it.
pub fn build_provider_stack(
    kind: ProviderKind,
    config: &Config,
    model: &str,
) -> Result<Arc<dyn LlmProvider>, String> {
    let primary = build_provider(kind, config)?;
    if config.fallback.providers.is_empty() {
        return Ok(primary);
    }

    let mut entries = vec![FallbackEntry {
        provider: primary,
        model: model.to_string(),
        label: kind.label().to_string(),
    }];

    for name in &config.fallback.providers {
        let fallback_kind = ProviderKind::from_config_str(name).ok_or_else(|| {
            format!(
                "[fallback] providers names `{name}`, which is not a provider \
                 (one of: anthropic, openai, openrouter, 9router, ollama)"
            )
        })?;
        if fallback_kind == kind {
            // The primary falling back to itself is a no-op; skip it rather
            // than erroring, because a shared global config listing every
            // provider someone has is a reasonable file to write.
            continue;
        }
        let provider = build_provider(fallback_kind, config).map_err(|e| {
            format!(
                "[fallback] providers names `{name}`, but it is not usable: {e} \
                 (a fallback that quietly is not there would be discovered at the \
                 worst possible moment — configure it or remove it from the list)"
            )
        })?;
        // A chain entry carries its own model, because `[general] model`
        // belongs to the primary — asking OpenRouter's model of Ollama is how
        // a fallback fails on arrival.
        let model = match fallback_kind {
            ProviderKind::NineRouter => config.nine_router.model.clone(),
            ProviderKind::Ollama => config.ollama.model.clone(),
            _ => None,
        }
        .unwrap_or_else(|| fallback_kind.default_model().to_string());

        // An entry that would ask for a model nobody named is the same class
        // of problem as one whose key is missing: it looks configured and
        // fails on arrival. 9Router is the only provider with no honest
        // default, because its catalogue belongs to whoever set up the
        // gateway.
        if model.is_empty() {
            return Err(format!(
                "[fallback] names `{name}`, but no model is set for it — add \
                 `[9router] model` (run `smith setup` to pick one the gateway \
                 actually lists), or remove it from the list"
            ));
        }
        entries.push(FallbackEntry {
            provider,
            model,
            label: fallback_kind.label().to_string(),
        });
    }

    if entries.len() == 1 {
        // Every configured entry was the primary itself.
        return Ok(entries.remove(0).provider);
    }
    Ok(Arc::new(FallbackProvider::new(
        entries,
        RetryPolicy::default().max_retry_after,
    )))
}

/// The retry budget, sized to the fallback chain.
///
/// The arithmetic: an advancement consumes one attempt (the handover error),
/// and a strike-based advancement consumes up to `STRIKE_LIMIT` (2). With the
/// default 4 attempts, a single dead entry could eat the whole budget before
/// the next entry ever answers — so each fallback entry adds two attempts.
/// Sleeps stay bounded: each is capped at `max_delay` (8s), and quota errors
/// advance instead of sleeping out their `Retry-After`.
pub fn retry_policy_for_chain(fallback_entries: usize) -> RetryPolicy {
    let default = RetryPolicy::default();
    RetryPolicy {
        max_attempts: default.max_attempts + 2 * (fallback_entries as u32),
        ..default
    }
}

/// Every credential this process has loaded, from either source, so tool
/// output can be scrubbed of them before it is shown, stored or sent onward.
///
/// Both the environment and the config file are read, because either can be
/// the live one for a given provider (`build_provider` prefers the env var)
/// and a stale value in the other is still a real secret worth hiding.
pub(crate) fn secret_redactor(config: &Config) -> Redactor {
    let from_env = [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "OPENROUTER_API_KEY",
        "NINEROUTER_API_KEY",
        "EXA_API_KEY",
        "TAVILY_API_KEY",
    ]
    .into_iter()
    .filter_map(|k| std::env::var(k).ok());

    let from_config = [
        config.anthropic.api_key.clone(),
        config.openai.api_key.clone(),
        config.openrouter.api_key.clone(),
        config.nine_router.api_key.clone(),
        config.exa.api_key.clone(),
        config.tavily.api_key.clone(),
    ]
    .into_iter()
    .flatten();

    Redactor::new(from_env.chain(from_config))
}

/// Turns the `[hooks]` config into the set the agent runs.
///
/// Shared by both places an `Agent` is built, so `/model` cannot silently drop
/// the user's hooks the way it once could have dropped their redactor: a
/// policy that stops being enforced because the model was switched is exactly
/// the invisible failure hooks exist to avoid.
fn hook_set(config: &Config) -> Arc<smith_core::HookSet> {
    use smith_core::hooks::{HookDefinition, HookEvent, DEFAULT_TIMEOUT};

    let build = |event: HookEvent, entries: &[smith_config::HookCommand]| {
        entries
            .iter()
            .map(|entry| {
                HookDefinition::new(event, entry.command.clone())
                    .with_matcher(entry.matcher.clone())
                    .with_timeout(
                        entry
                            .timeout_ms
                            .map(std::time::Duration::from_millis)
                            .unwrap_or(DEFAULT_TIMEOUT),
                    )
            })
            .collect::<Vec<_>>()
    };

    let mut hooks = build(HookEvent::PreToolUse, &config.hooks.pre_tool_use);
    hooks.extend(build(HookEvent::PostToolUse, &config.hooks.post_tool_use));
    hooks.extend(build(
        HookEvent::UserPromptSubmit,
        &config.hooks.user_prompt_submit,
    ));
    Arc::new(smith_core::HookSet::new(hooks))
}

/// Collects the config `web_search` reads, in one place so the orchestrator and
/// `doctor` cannot disagree about which backends are configured.
pub(crate) fn web_search_settings(config: &Config) -> smith_tools::web_search::SearchSettings {
    smith_tools::web_search::SearchSettings {
        backend: config.search.backend.clone(),
        exa_api_key: config.exa.api_key.clone(),
        tavily_api_key: config.tavily.api_key.clone(),
        searxng_url: config.search.searxng_url.clone(),
        market: config.search.market.clone(),
    }
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

    fn persist_tasks(&mut self, tasks: &[smith_core::Task]) {
        let session_id = self.ensure_session_id();
        let _ = self.store.save_tasks(&session_id, tasks);
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
    let tasks = state.agent.tasks().to_vec();
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
    // The *stamped* board — resume prefers this snapshot because the ids and
    // timestamps exist only here, not in the model's tool_use input that
    // history holds. Empty is skipped: most turns never touch the board, and
    // an empty write would erase a snapshot the turn had nothing to do with.
    if !tasks.is_empty() {
        persistence.persist_tasks(&tasks);
    }
}

/// Starts connecting to every configured MCP server, concurrently, and hands
/// back a join handle rather than a result.
///
/// Nothing here is awaited on the way in, and that is the point. Connecting is
/// process spawns and network handshakes — the slowest thing in startup — and
/// it used to sit in front of everything else in `run_orchestrator`, serially,
/// with no deadline. Now: N servers cost the slowest one instead of the sum,
/// they overlap with provider construction, memory discovery and subagent
/// loading, and `McpRegistry::connect_all` caps each server at
/// `smith_mcp::CONNECT_TIMEOUT`, so a wedged server is a bounded delay rather
/// than a hang. The remaining cost is that the *first turn* waits for the
/// registry; the UI has never waited, since `main` spawns the orchestrator and
/// renders the first frame in parallel with it.
fn start_mcp_connections(config: &Config) -> tokio::task::JoinHandle<smith_mcp::McpRegistry> {
    let specs = config.mcp_servers.clone();
    tokio::spawn(async move { smith_mcp::McpRegistry::connect_all(&specs).await })
}

/// Registers everything a connected registry publishes, under the namespaced
/// `mcp__{server}__{tool}` name so a remote server can't shadow a built-in or
/// another server. A server that failed is reported and skipped rather than
/// aborting startup — one bad server shouldn't take down the whole session;
/// an individual tool whose name is somehow still taken is skipped the same
/// way.
fn register_mcp_tools(
    registry: &smith_mcp::McpRegistry,
    tools: &mut ToolRegistry,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    for problem in registry.problems() {
        let _ = event_tx.send(AgentEvent::Error(problem));
    }
    for tool in registry.tools() {
        let name = tool.name().to_string();
        if let Err(e) = tools.try_register(tool) {
            let _ = event_tx.send(AgentEvent::Error(format!("mcp: skipping '{name}': {e}")));
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
    /// The composed static system prompt — built-in halves plus whatever
    /// persona was selected. Held rather than recomputed so `switch_model`
    /// rebuilds the agent with the *same bytes*: a persona re-read here would
    /// take effect on `/model` and nowhere else, and a recomposition that
    /// differed by a byte would silently cost the session its prefix cache.
    system: String,
    /// The undo buffer `/rewind` reads. Held here as the concrete type rather
    /// than the `Checkpointer` trait the agent uses, because planning and
    /// applying a rewind happen outside the turn loop and there is nothing
    /// there for the agent to abstract over.
    checkpoints: Arc<CheckpointStore>,
}

impl OrchestratorState {
    /// Rebuilds the agent against a (possibly new) provider/model, carrying
    /// the conversation history over so switching mid-chat doesn't lose
    /// context.
    async fn switch_model(
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
        let new_provider = build_provider_stack(new_kind, config, &model)?;

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
        let session_unpriced = self.agent.unpriced_turns();
        // Dropping this would silently disable `/rewind` for the rest of the
        // session — the least visible way to lose an undo buffer.
        let checkpointer = self.agent.checkpointer();
        // Read from disk once at startup; re-reading here would let a file
        // edited mid-session take effect on `/model` and nowhere else.
        let subagents = self.agent.subagent_definitions().to_vec();
        let unattended = self.agent.unattended();

        // Same reason as at startup: a switch to a model with a different
        // window must move the gauge and the compaction threshold with it.
        new_provider.warm_capabilities(&model).await;

        let scratch_dir = tool_ctx.scratch_dir();
        let mut agent = Agent::new(new_provider, self.tools.clone(), model.clone(), tool_ctx)
            .with_system(self.system.clone())
            .with_context_provider(context_provider(self.memory.clone(), scratch_dir))
            .with_subagent_definitions(subagents)
            .with_limits(limits)
            .with_retry_policy(retry_policy)
            .with_compaction(compaction)
            // Rebuilt from config rather than carried over, so a key added
            // since startup is covered too. Forgetting it here would silently
            // disable redaction for the rest of the session.
            .with_redactor(secret_redactor(config))
            // Rebuilt from config for the same reason as the redactor above,
            // and with the same failure mode if forgotten: a hook the user
            // believes is enforcing a policy, silently gone since `/model`.
            .with_hooks(hook_set(config))
            .with_permission_policy(permission_policy)
            // Carried over for the same reason the limits and the redactor are:
            // a `/model` switch that quietly re-enabled the exemptions would be
            // the least visible way to lose the only gate a headless run has.
            .with_unattended(unattended);
        if let Some(checkpointer) = checkpointer {
            agent = agent.with_checkpointer(checkpointer);
        }
        agent.set_plan_gated(plan_gated);
        agent.set_goal(goal);
        agent.seed_history(history);
        agent.seed_allowed_session_tools(allowed);
        agent.seed_tasks(tasks);
        agent.seed_session_totals(session_usage, session_cost, session_unpriced);

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
    /// The resumed board, already stamped. Empty means "scan history",
    /// which keeps pre-snapshot sessions working — see setup::wire.
    pub initial_tasks: Vec<smith_core::Task>,
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
    /// Output style for this session, already loaded. `None` is the built-in
    /// prompt. Resolved by the frontend (it owns `--persona`) and never
    /// re-read here — see `prompts::system_prompt_with`.
    pub persona: Option<smith_config::Persona>,
    /// True when nobody is at the terminal. Set by the headless frontend; it
    /// turns off the two prompt exemptions that only make sense when a human
    /// would otherwise be interrupted — see `Agent::with_unattended`.
    pub unattended: bool,
}

impl OrchestratorOptions {
    pub fn new(provider_kind: ProviderKind, model: String, config: Config) -> Self {
        Self {
            provider_kind,
            model,
            config,
            initial_messages: Vec::new(),
            initial_tasks: Vec::new(),
            persistence: None,
            permission_policy: smith_core::PermissionPolicy::default(),
            initial_goal: None,
            limits: TurnLimits::default(),
            provider: None,
            persona: None,
            unattended: false,
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

mod actions;
mod setup;

/// Wires the session up, then hands it to the action loop.
pub async fn run_orchestrator(opts: OrchestratorOptions, chans: OrchestratorChannels) {
    let OrchestratorChannels {
        mut action_rx,
        event_tx,
        permission_tx,
        question_tx,
    } = chans;

    let Some(wiring) = setup::wire(opts, &event_tx, &mut action_rx).await else {
        return;
    };
    actions::run(wiring, action_rx, event_tx, permission_tx, question_tx).await;
}

/// What a provider can run right now, for the `/model` picker.
///
/// Live where a live answer exists — an Ollama daemon lists what has been
/// pulled, a 9Router gateway lists what its owner configured — and the
/// curated list otherwise, because a catalogue call for the hosted providers
/// needs the key and offering the shortlist costs nothing.
///
/// An empty result is not an error here. The frontend says "could not read
/// the catalogue" and leaves the user able to type a name, which is strictly
/// better than a picker that refuses to open.
pub async fn live_models(kind: ProviderKind, config: &Config) -> Vec<smith_core::ModelChoice> {
    let choice = |id: String, detail: String, supports_tools: bool| smith_core::ModelChoice {
        id,
        detail,
        supports_tools,
    };

    match kind {
        ProviderKind::Ollama => {
            let base = config
                .ollama
                .base_url
                .clone()
                .unwrap_or_else(|| smith_config::DEFAULT_OLLAMA_BASE_URL.to_string());
            let live = smith_provider::ollama_tags(&base).await.unwrap_or_default();
            let mut out: Vec<smith_core::ModelChoice> = live
                .iter()
                .map(|m| {
                    let detail = m.summary().trim_start_matches(&m.name).trim().to_string();
                    choice(m.name.clone(), detail, m.supports_tools)
                })
                .collect();
            // The daemon lists only what is pulled, so a fresh install shows
            // almost nothing — and the free cloud models are exactly what is
            // worth offering there. Same merge the wizard and the web UI do.
            for name in smith_store::models::OLLAMA_FREE_CLOUD_MODELS {
                if live.iter().any(|m| m.name == *name) {
                    continue;
                }
                out.push(choice(
                    (*name).to_string(),
                    "cloud · free tier".to_string(),
                    true,
                ));
            }
            out
        }
        ProviderKind::NineRouter => {
            let base = config
                .nine_router
                .base_url
                .clone()
                .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());
            crate::node_runtime::ninerouter_upstreams(&base)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|id| choice(id, String::new(), true))
                .collect()
        }
        other => smith_store::models::known_models(other.label())
            .iter()
            .map(|id| choice((*id).to_string(), String::new(), true))
            .collect(),
    }
}

#[cfg(test)]
mod tests;
