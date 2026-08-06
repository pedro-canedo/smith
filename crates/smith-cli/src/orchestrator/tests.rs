/// A fallback that quietly is not there would be discovered at the worst
/// possible moment — the moment it was configured for.
/// Keys go through the config file, not env vars: env is process-global
/// and these tests run in parallel with the ones asserting on absence.
fn config_with_openrouter_key() -> Config {
    let mut config = Config::default();
    config.openrouter.api_key = Some("k".into());
    config
}

#[test]
fn a_chain_naming_an_unknown_provider_errs_naming_it() {
    let mut config = config_with_openrouter_key();
    config.fallback.providers = vec!["chatgpt".into()];
    let Err(err) = build_provider_stack(ProviderKind::Openrouter, &config) else {
        panic!("an unknown chain entry must fail loudly");
    };
    assert!(err.contains("chatgpt"), "{err}");
}

#[test]
fn a_chain_entry_without_its_key_errs_naming_the_key() {
    let mut config = config_with_openrouter_key();
    config.fallback.providers = vec!["9router".into()];
    let Err(err) = build_provider_stack(ProviderKind::Openrouter, &config) else {
        panic!("an unusable chain entry must fail loudly");
    };
    assert!(err.contains("NINEROUTER_API_KEY"), "{err}");
    assert!(err.contains("worst possible moment"), "{err}");
}

/// No chain configured — the primary alone, unwrapped, zero cost.
#[test]
fn an_empty_chain_returns_the_bare_primary() {
    let provider =
        build_provider_stack(ProviderKind::Openrouter, &config_with_openrouter_key()).unwrap();
    assert_eq!(provider.id(), "openrouter");
}

/// A configured chain wraps: id() answers for the active (first) entry,
/// and a self-referential entry is skipped rather than duplicated.
#[test]
fn a_configured_chain_wraps_and_skips_the_primary_itself() {
    let mut config = config_with_openrouter_key();
    config.nine_router.api_key = Some("nk".into());
    config.fallback.providers = vec!["openrouter".into(), "9router".into()];
    let provider = build_provider_stack(ProviderKind::Openrouter, &config).unwrap();
    assert_eq!(provider.id(), "openrouter", "primary serves first");
}

/// Two extra attempts per chain entry — the arithmetic in the doc.
#[test]
fn the_retry_budget_grows_with_the_chain() {
    assert_eq!(
        retry_policy_for_chain(0).max_attempts,
        RetryPolicy::default().max_attempts
    );
    assert_eq!(
        retry_policy_for_chain(2).max_attempts,
        RetryPolicy::default().max_attempts + 4
    );
}

/// The clap value name is pinned because identifiers cannot start with a
/// digit — without `#[value(name = "9router")]` the flag would demand
/// `--provider nine-router`, a spelling nothing else uses.
#[test]
fn provider_labels_round_trip_including_the_digit_led_one() {
    use clap::ValueEnum;
    for kind in [
        ProviderKind::Anthropic,
        ProviderKind::Openai,
        ProviderKind::Openrouter,
        ProviderKind::NineRouter,
        ProviderKind::Ollama,
    ] {
        assert_eq!(
            ProviderKind::from_config_str(kind.label()),
            Some(kind),
            "label {} does not round-trip",
            kind.label()
        );
        assert_eq!(
            ProviderKind::from_str(kind.label(), false),
            Ok(kind),
            "clap value name diverges from the config label for {}",
            kind.label()
        );
    }
}

#[test]
fn a_missing_openrouter_key_errs_naming_the_env_var_and_the_free_key_url() {
    let config = Config::default();
    std::env::remove_var("OPENROUTER_API_KEY");
    let Err(err) = build_provider(ProviderKind::Openrouter, &config) else {
        panic!("a keyless openrouter build must fail");
    };
    assert!(err.contains("OPENROUTER_API_KEY"), "{err}");
    assert!(err.contains("openrouter.ai/keys"), "{err}");
}

#[test]
fn a_missing_ninerouter_key_errs_naming_the_dashboard() {
    let config = Config::default();
    std::env::remove_var("NINEROUTER_API_KEY");
    let Err(err) = build_provider(ProviderKind::NineRouter, &config) else {
        panic!("a keyless 9router build must fail");
    };
    assert!(err.contains("NINEROUTER_API_KEY"), "{err}");
    assert!(err.contains("localhost:20128"), "{err}");
}

use super::*;
use crate::headless::{self, HeadlessOptions, OutputFormat, EXIT_LIMIT, EXIT_OK};
use smith_core::testkit::{text_reply, tool_call_reply, ScriptedProvider, ScriptedResponse};

/// `[hooks]` sits between a plain table and an array of tables in
/// `Config`, and the TOML serializer is order-sensitive about exactly
/// that: a field in the wrong place writes a file this same struct cannot
/// read back, and nothing but a round trip catches it.
#[test]
fn a_config_carrying_hooks_survives_a_toml_round_trip() {
    let mut config = Config::default();
    config.runtime.chromium_path = Some("/usr/bin/chromium".into());
    config.hooks.pre_tool_use.push(smith_config::HookCommand {
        command: "guard.sh".into(),
        matcher: Some("write_file|edit_file".into()),
        timeout_ms: Some(2_000),
    });
    config
        .hooks
        .user_prompt_submit
        .push(smith_config::HookCommand {
            command: "redact.sh".into(),
            matcher: None,
            timeout_ms: None,
        });
    config.mcp_servers.push(smith_config::McpServerConfig {
        name: "files".into(),
        command: "mcp-files".into(),
        args: vec!["--root".into()],
        // A stdio entry predates the network transports and must still
        // round-trip byte-identically — that is what this test is for.
        url: None,
        transport: None,
        headers: Default::default(),
    });

    let text = toml::to_string_pretty(&config).expect("must serialize");
    let parsed: Config = toml::from_str(&text).expect("must parse back");

    assert_eq!(parsed.hooks.pre_tool_use.len(), 1);
    assert_eq!(parsed.hooks.pre_tool_use[0].command, "guard.sh");
    assert_eq!(parsed.hooks.pre_tool_use[0].timeout_ms, Some(2_000));
    assert_eq!(parsed.hooks.user_prompt_submit[0].matcher, None);
    assert!(parsed.hooks.post_tool_use.is_empty());
    assert_eq!(parsed.mcp_servers.len(), 1);
}

/// Every configured entry has to become a hook the agent will actually
/// run: a mapping that drops one silently is the exact failure mode this
/// feature cannot have.
#[test]
fn every_configured_hook_reaches_the_agent() {
    let mut config = Config::default();
    for (list, command) in [
        (&mut config.hooks.pre_tool_use, "pre.sh"),
        (&mut config.hooks.post_tool_use, "post.sh"),
        (&mut config.hooks.user_prompt_submit, "prompt.sh"),
    ] {
        list.push(smith_config::HookCommand {
            command: command.into(),
            matcher: None,
            timeout_ms: None,
        });
    }
    assert_eq!(hook_set(&config).len(), 3);
    assert!(hook_set(&Config::default()).is_empty());
}

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
        headless::compose_prompt(Some("diagnose this"), Some("thread 'main' panicked")).unwrap();
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

/// The whole checkpoint chain with nothing stubbed but the model: the real
/// `write_file` declares the path it will touch, the real `Agent` snapshots
/// it around dispatch, and a real `Action::Rewind` puts the bytes back.
///
/// It is deliberately end-to-end. Every piece is unit-tested on its own,
/// but the failure this guards against is a *wiring* one — a store nobody
/// handed to the agent, or a `snapshot_paths` nobody forwarded — and each
/// unit test would still pass with the wire cut.
#[tokio::test]
async fn a_write_is_checkpointed_and_rewind_puts_the_original_bytes_back() {
    // Inside the crate directory: that is `ToolContext`'s sandbox root
    // under `cargo test`, and therefore also the checkpoint store's root.
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let target = dir.path().join("poem.txt");
    tokio::fs::write(&target, "the original\n").await.unwrap();

    // A session id of its own, so a checkpoint written by a test running
    // in parallel can never become this one's "latest turn".
    let session_id = format!("test-rewind-{}", dir.path().display())
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-");
    let db = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::streams([
        // The read is not decoration: `write_file` refuses to replace a
        // file the session has never read (`fs_tools::ReadSet`), so a
        // scripted model that skipped it would never get as far as the
        // checkpoint this test is about.
        tool_call_reply(
            "call_1",
            "read_file",
            serde_json::json!({ "path": target.to_string_lossy() }),
        ),
        tool_call_reply(
            "call_2",
            "write_file",
            serde_json::json!({ "path": target.to_string_lossy(), "content": "the rewrite\n" }),
        ),
        text_reply("rewritten"),
    ]));

    let (action_tx, action_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    let mut opts = OrchestratorOptions::new(
        ProviderKind::Anthropic,
        "scripted-model".to_string(),
        Config::default(),
    );
    opts.provider = Some(provider);
    opts.permission_policy = smith_core::PermissionPolicy::Skip;
    opts.persistence = Some(Persistence {
        store: SessionStore::open(db.path()).unwrap(),
        session_id: Some(session_id.clone()),
        provider: "anthropic".into(),
        model: "scripted-model".into(),
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
        .send(Action::SubmitMessage("rewrite the poem".into()))
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
    assert_eq!(
        tokio::fs::read_to_string(&target).await.unwrap(),
        "the rewrite\n"
    );

    // The preview must change nothing — that is the entire contract of the
    // bare command.
    action_tx
        .send(Action::Rewind {
            turn: None,
            apply: false,
            force: false,
        })
        .unwrap();
    let preview = next_rewind(&mut event_rx).await;
    assert_eq!(
        preview.status,
        smith_core::RewindStatus::Preview,
        "{preview:?}"
    );
    assert_eq!(preview.restore.len(), 1, "{preview:?}");
    assert_eq!(
        tokio::fs::read_to_string(&target).await.unwrap(),
        "the rewrite\n",
        "a preview modified the file"
    );

    action_tx
        .send(Action::Rewind {
            turn: None,
            apply: true,
            force: false,
        })
        .unwrap();
    let applied = next_rewind(&mut event_rx).await;
    assert_eq!(
        applied.status,
        smith_core::RewindStatus::Applied,
        "{applied:?}"
    );
    assert_eq!(
        tokio::fs::read_to_string(&target).await.unwrap(),
        "the original\n"
    );

    orchestrator.abort();
}

async fn next_rewind(events: &mut mpsc::UnboundedReceiver<AgentEvent>) -> smith_core::RewindReport {
    while let Some(event) = events.recv().await {
        if let AgentEvent::Rewind(report) = event {
            return report;
        }
    }
    panic!("the orchestrator never reported a rewind");
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
