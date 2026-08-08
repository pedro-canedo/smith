//! Everything that has to exist before the first action can be handled.
//!
//! Split out of `run_orchestrator` because it and the action loop share
//! nothing but the handful of values named in `Wiring`: the setup half is a
//! straight line of I/O run once, the loop half is a `match` run forever.

//! Owns "which provider/model" and the `Action` → `Agent` dispatch loop:
//! `run_orchestrator` is the async task that receives every `Action` the TUI
//! sends and drives the shared `Agent` accordingly (spawning one task per
//! action so a long-running turn never blocks e.g. `CancelGeneration` from
//! being handled). `main.rs` only builds the initial state and hands off to
//! this loop; prompt text/parsing lives in `prompts.rs`.

use std::sync::Arc;

use smith_config::{Config, MemoryCache};
use smith_core::{Action, Agent, AgentEvent, ToolContext};
use smith_tools::{CheckpointStore, ToolRegistry};
use tokio::sync::{mpsc, Mutex};

use crate::prompts::{context_provider, system_prompt_with};

use super::{
    build_provider_stack, hook_set, new_session_id, register_mcp_tools, retry_policy_for_chain,
    secret_redactor, start_mcp_connections, web_search_settings, OrchestratorOptions,
    OrchestratorState, ProviderKind,
};

/// What the action loop needs from the setup it did not do.
pub(super) struct Wiring {
    pub(super) state: Arc<Mutex<OrchestratorState>>,
    pub(super) interjections: Arc<std::sync::Mutex<std::collections::VecDeque<String>>>,
    pub(super) mcp: Arc<smith_mcp::McpRegistry>,
    pub(super) config: Config,
    pub(super) provider_kind: ProviderKind,
}

/// Builds the agent and everything under it.
///
/// `None` means the session cannot start — today only a provider that will not
/// build. That path still drains actions until `Quit`, because the frontend is
/// already drawing and a receiver that just disappears would hang it.
pub(super) async fn wire(
    opts: OrchestratorOptions,
    event_tx: &mpsc::UnboundedSender<AgentEvent>,
    action_rx: &mut mpsc::UnboundedReceiver<Action>,
) -> Option<Wiring> {
    let OrchestratorOptions {
        provider_kind,
        model,
        config,
        initial_messages,
        initial_tasks,
        mut persistence,
        permission_policy,
        initial_goal,
        limits,
        provider: provider_override,
        persona,
        unattended,
    } = opts;

    // Started before anything else in this function, so the process spawns and
    // network handshakes overlap with everything below instead of preceding it.
    let mcp_connecting = start_mcp_connections(&config);

    // The 9router gateway, if this session can reach for it (as primary or as
    // a fallback entry), is brought up concurrently with everything else —
    // same reasoning as the MCP connections above. A failure is an Error
    // event *before* the first turn: visible, and the session still starts,
    // because a fallback entry that stays dead simply strikes out and the
    // chain moves past it.
    let uses_ninerouter = provider_kind == ProviderKind::NineRouter
        || config.fallback.providers.iter().any(|p| p == "9router");
    let ninerouter_starting = uses_ninerouter.then(|| {
        let config = config.clone();
        tokio::spawn(async move {
            crate::node_runtime::ensure_ninerouter_running(&config).await?;
            // Running is not ready. A gateway with no upstreams answers every
            // health probe and then 404s on the first message — the one place
            // a user cannot do anything about it. Saying so before the turn
            // costs one request and turns a mid-conversation failure into a
            // sentence at startup.
            let base_url = config
                .nine_router
                .base_url
                .clone()
                .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());
            match crate::node_runtime::ninerouter_upstreams(&base_url).await {
                Ok(models) if models.is_empty() => Err(format!(
                    "the 9router gateway on {base_url} is running but routes to nothing \
                     — open http://localhost:20128 and add a provider under `Providers`, \
                     or every request will fail with `No active credentials`"
                )),
                // A list smith could not read is not evidence of a problem;
                // the gateway is up, and the turn will report its own failure.
                _ => Ok(()),
            }
        })
    });

    let provider = match provider_override {
        Some(p) => p,
        None => match build_provider_stack(provider_kind, &config, &model) {
            Ok(p) => p,
            Err(err) => {
                let _ = event_tx.send(AgentEvent::Error(err));
                while let Some(action) = action_rx.recv().await {
                    if matches!(action, Action::Quit) {
                        break;
                    }
                }
                return None;
            }
        },
    };

    let mut tools = ToolRegistry::with_builtin_tools();
    tools.replace(Arc::new(
        smith_tools::web_search::WebSearchTool::with_settings(web_search_settings(&config)),
    ));
    // Skills, and the `skill` tool that discloses them. The catalogue always
    // holds at least the builtin set, so the tool is registered in practice
    // on every session; the `is_empty` guard below is the fallback for a
    // build with the builtins compiled out. A broken SKILL.md costs its own
    // skill and is reported, exactly like a broken subagent definition.
    let project_dir = std::env::current_dir().unwrap_or_default();
    let skills = smith_config::SkillCatalog::discover(&project_dir);
    for problem in &skills.problems {
        let _ = event_tx.send(AgentEvent::Error(format!("skill {problem}")));
    }
    if !skills.is_empty() {
        let entries = skills
            .skills()
            .iter()
            .map(|s| smith_tools::skill::SkillEntry {
                name: s.name.clone(),
                description: s.description.clone(),
                body: s.rendered(),
            })
            .collect();
        tools.register(Arc::new(smith_tools::skill::SkillTool::new(entries)));
    }

    // Allocate a stable session id up front for staging (and reuse a resumed
    // id). The DB row is still created lazily on first persist via
    // ensure_session — which is why this must be a real id and not a
    // placeholder: `ensure_session` files the session under whatever it is
    // handed, so this *is* the session's permanent name.
    let session_id = persistence
        .as_ref()
        .and_then(|p| p.session_id.clone())
        .unwrap_or_else(new_session_id);
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

    // Rooted at the same directory as the tool jail, so every path a tool can
    // legally write is inside the checkpoint store's world.
    let checkpoints = Arc::new(CheckpointStore::new(tool_ctx.cwd.clone()));
    {
        // Old objects are reclaimed once per process, off the critical path.
        // A sweep that made startup wait — or that could fail startup — would
        // be a worse bug than the disk it saves.
        let checkpoints = checkpoints.clone();
        tokio::spawn(async move {
            checkpoints
                .sweep(smith_tools::checkpoint::DEFAULT_TTL)
                .await;
        });
    }
    {
        // Stale sessions' scratch directories, same lifecycle as checkpoint
        // objects: once per process, best-effort, never on the critical path.
        // The running session's own directory is exempt by id, not by age —
        // `--resume` legitimately reopens a session older than any TTL.
        let project_root = tool_ctx.cwd.clone();
        let keep = tool_ctx.session_id.clone();
        tokio::spawn(async move {
            smith_tools::scratch::sweep(&project_root, smith_tools::scratch::DEFAULT_TTL, &keep)
                .await;
        });
    }

    // A definition that will not parse costs its own subagent and nothing
    // else — but it is said out loud, because a `task` call naming a subagent
    // that quietly failed to load is otherwise just "no subagent named x".
    let (subagents, subagent_problems) = crate::subagents::load();
    for problem in subagent_problems {
        let _ = event_tx.send(AgentEvent::Error(format!("subagent {problem}")));
    }

    // The latest point the MCP registry can be joined: everything above ran
    // while the servers were connecting, and `Agent::new` needs the finished
    // tool registry. A panicked connect task costs MCP and nothing else.
    if let Some(starting) = ninerouter_starting {
        if let Ok(Err(e)) = starting.await {
            let _ = event_tx.send(AgentEvent::Error(format!("9router: {e}")));
        }
    }
    let mcp = Arc::new(mcp_connecting.await.unwrap_or_default());
    register_mcp_tools(&mcp, &mut tools, event_tx);
    let tools = Arc::new(tools);

    // Composed once, here, and held on the state: read-once is what keeps the
    // static prompt byte-identical for the whole session.
    let system = system_prompt_with(persona.as_ref());

    // Before the first request, so the context gauge and the compaction
    // threshold are built on what the model actually has rather than on a
    // guess. Best-effort: a provider that cannot answer keeps its defaults.
    provider.warm_capabilities(&model).await;

    let scratch_dir = tool_ctx.scratch_dir();
    let mut agent = Agent::new(provider, tools.clone(), model, tool_ctx)
        .with_checkpointer(checkpoints.clone())
        .with_system(system.clone())
        .with_context_provider(context_provider(memory.clone(), scratch_dir))
        .with_subagent_definitions(subagents)
        // Set explicitly (rather than left to `Agent`'s own default) so
        // `switch_model` has something to carry over and so `--max-turns` has
        // exactly one place to plug into.
        .with_limits(limits)
        .with_retry_policy(retry_policy_for_chain(config.fallback.providers.len()))
        .with_redactor(secret_redactor(&config))
        .with_hooks(hook_set(&config))
        .with_permission_policy(permission_policy)
        // Headless. Turns off the two prompt exemptions whose justification is
        // "do not interrupt the user" — there is no user to interrupt, and
        // `--allowed-tools` is the only gate the run has.
        .with_unattended(unattended);
    agent.set_goal(initial_goal);
    // The stamped snapshot when the frontend loaded one; the legacy history
    // scan otherwise (pre-snapshot sessions, callers that set nothing).
    let seeded_tasks = if initial_tasks.is_empty() {
        crate::startup::last_write_tasks_call(&initial_messages)
    } else {
        initial_tasks
    };
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
        agent.seed_session_totals(totals.usage, totals.cost_usd, totals.unpriced_turns);
        // Announced now rather than at the first turn: a resumed session shows
        // what it already spent from the moment it opens, which is the half of
        // acceptance criterion #4 a frontend can get wrong by simply staying
        // quiet until something new happens.
        if totals.turns > 0 {
            let _ = event_tx.send(AgentEvent::SessionCost {
                usd: totals.cost_usd,
                unpriced_turns: totals.unpriced_turns,
            });
        }
    }
    let state = Arc::new(Mutex::new(OrchestratorState {
        agent,
        persistence,
        provider_kind,
        tools,
        memory,
        system,
        checkpoints,
    }));

    // Taken before the loop, because `state` is locked for the duration of
    // every turn and this has to be reachable while one is running.
    let interjections = {
        let guard = state.lock().await;
        guard.agent.interjection_queue()
    };

    Some(Wiring {
        state,
        interjections,
        mcp,
        config,
        provider_kind,
    })
}
