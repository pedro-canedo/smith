//! Context-window accounting and per-turn cost.

use super::*;

// ---- context accounting ------------------------------------------------

/// The provider hands back an exact prompt count with every response, so
/// the gauge should be that number verbatim — not an estimate of it —
/// right up until something else is appended to history.
#[tokio::test]
async fn the_context_gauge_uses_the_providers_own_prompt_count() {
    let provider = Arc::new(
        ScriptedProvider::streams([text_reply_with_usage(
            "ok",
            Usage {
                input_tokens: 5_000,
                output_tokens: 120,
                ..Usage::default()
            },
        )])
        .with_capabilities(window_of(20_000)),
    );
    let mut agent = agent_for(provider, Arc::new(NoTools));
    let (_, events) = run_collect(&mut agent, "hello", CancellationToken::new()).await;

    let context = agent.context_usage();
    assert_eq!(context.used, 5_120);
    assert_eq!(context.window, 20_000);
    assert!(
        !context.estimated,
        "nothing was appended after the response"
    );
    assert!(
        (context.ratio() - 0.256).abs() < 1e-6,
        "{}",
        context.ratio()
    );

    // And the frontend was told, with the same numbers.
    assert!(
        context_events(&events).contains(&(5_120, 20_000, false)),
        "{:?}",
        context_events(&events)
    );
}

/// Anthropic reports `input_tokens` *excluding* cached tokens, so a gauge
/// that reads only that field shows an all-but-empty context on the exact
/// sessions that are closest to full.
#[tokio::test]
async fn cached_prompt_tokens_count_toward_the_context() {
    let provider = Arc::new(
        ScriptedProvider::streams([text_reply_with_usage(
            "ok",
            Usage {
                input_tokens: 100,
                output_tokens: 0,
                cache_read: 9_000,
                cache_write: 500,
            },
        )])
        .with_capabilities(window_of(20_000)),
    );
    let mut agent = agent_for(provider, Arc::new(NoTools));
    run_collect(&mut agent, "hello", CancellationToken::new()).await;

    assert_eq!(agent.context_usage().used, 9_600);
}

/// A model with no entry in any capability table must be assumed small.
/// `ScriptedProvider` reports `ProviderCapabilities::default()` for exactly
/// this reason.
#[tokio::test]
async fn an_unknown_model_is_measured_against_the_conservative_window() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply_with_usage(
        "ok",
        prompt_usage(4_096),
    )]));
    let mut agent = agent_for(provider, Arc::new(NoTools));
    run_collect(&mut agent, "hello", CancellationToken::new()).await;

    let context = agent.context_usage();
    assert_eq!(context.window, 8_192);
    assert!((context.ratio() - 0.5).abs() < 1e-6, "{}", context.ratio());
    // The same 4096 tokens against a 200k model would be 2% — being wrong
    // in this direction is what keeps a turn from blowing the window.
    assert!(context.ratio() > 0.4);
}

/// Before the first response there is nothing but estimate, and the system
/// prompt and tool schemas are a real, sizeable part of it.
#[tokio::test]
async fn the_first_request_is_estimated_and_includes_the_prompt_overhead() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply("ok")]));
    let agent = agent_for(provider, Arc::new(NoTools)).with_system("x".repeat(4_000));

    let context = agent.context_usage();
    assert!(context.estimated);
    // 4000 chars of system prompt is ~1000 tokens before the margin.
    assert!(context.used >= 1_000, "{}", context.used);
}

#[tokio::test]
async fn the_compaction_trigger_fires_at_the_threshold_and_not_before() {
    // 0.80 of a 1000-token window is 800 exactly. Both sides of that line
    // are checked, because "fires eventually" is not the requirement.
    for (input_tokens, expected) in [(799u32, false), (800u32, true)] {
        let provider = Arc::new(
            ScriptedProvider::streams([text_reply_with_usage("ok", prompt_usage(input_tokens))])
                .with_capabilities(window_of(1_000)),
        );
        let mut agent = agent_for(provider, Arc::new(NoTools));
        run_collect(&mut agent, "hi", CancellationToken::new()).await;

        assert_eq!(agent.context_usage().used, input_tokens);
        assert_eq!(
            agent.should_compact(),
            expected,
            "{input_tokens} tokens of a 1000-token window"
        );
    }
}

#[tokio::test]
async fn compaction_can_be_switched_off_entirely() {
    let provider = Arc::new(
        ScriptedProvider::streams([text_reply_with_usage("ok", prompt_usage(999))])
            .with_capabilities(window_of(1_000)),
    );
    let mut agent = agent_for(provider, Arc::new(NoTools)).with_compaction(CompactionConfig {
        enabled: false,
        ..CompactionConfig::default()
    });
    run_collect(&mut agent, "hi", CancellationToken::new()).await;

    assert!(agent.context_usage().ratio() > 0.9);
    assert!(!agent.should_compact());
}

// ---- cost --------------------------------------------------------------

fn priced_agent(provider: Arc<ScriptedProvider>, model: &str) -> Agent {
    Agent::new(
        provider,
        Arc::new(NoTools),
        model.to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_permission_policy(PermissionPolicy::Skip)
}

/// Cost is computed once, here, while the turn is running — which is the
/// number the session store then persists.
#[tokio::test]
async fn a_turn_carries_the_cost_computed_when_it_ran() {
    let provider = Arc::new(
        ScriptedProvider::streams([text_reply_with_usage(
            "ok",
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                ..Usage::default()
            },
        )])
        .with_id("anthropic"),
    );
    let mut agent = priced_agent(provider, "claude-sonnet-5");
    run_collect(&mut agent, "hi", CancellationToken::new()).await;

    let turn = agent.last_turn().expect("a turn ran");
    assert_eq!(turn.provider, "anthropic");
    assert_eq!(turn.model, "claude-sonnet-5");
    assert_eq!(turn.usage.output_tokens, 1_000_000);
    assert!((turn.cost_usd.unwrap() - 18.0).abs() < 1e-9, "{turn:?}");
    assert!((agent.session_cost_usd() - 18.0).abs() < 1e-9);
}

/// An unpriced model still gets its tokens recorded; the cost is `None`,
/// never a zero pretending the turn was free.
#[tokio::test]
async fn an_unpriced_model_records_tokens_without_inventing_a_cost() {
    let provider = Arc::new(
        ScriptedProvider::streams([text_reply_with_usage("ok", prompt_usage(1_000))])
            .with_id("ollama"),
    );
    let mut agent = priced_agent(provider, "qwen2.5");
    run_collect(&mut agent, "hi", CancellationToken::new()).await;

    let turn = agent.last_turn().unwrap();
    assert_eq!(turn.usage.input_tokens, 1_000);
    assert_eq!(turn.cost_usd, None);
    assert_eq!(agent.session_cost_usd(), 0.0);
}

/// Each turn's accounting stands alone, so a caller persisting one row per
/// turn never double-counts the previous one.
#[tokio::test]
async fn turn_accounting_resets_between_turns_while_the_session_total_grows() {
    let provider = Arc::new(
        ScriptedProvider::streams([
            text_reply_with_usage("one", prompt_usage(1_000_000)),
            text_reply_with_usage("two", prompt_usage(1_000_000)),
        ])
        .with_id("anthropic"),
    );
    let mut agent = priced_agent(provider, "claude-sonnet-5");

    run_collect(&mut agent, "first", CancellationToken::new()).await;
    assert!((agent.last_turn().unwrap().cost_usd.unwrap() - 3.0).abs() < 1e-9);

    run_collect(&mut agent, "second", CancellationToken::new()).await;
    assert!((agent.last_turn().unwrap().cost_usd.unwrap() - 3.0).abs() < 1e-9);
    assert!((agent.session_cost_usd() - 6.0).abs() < 1e-9);
    assert_eq!(agent.session_usage().input_tokens, 2_000_000);
}

/// What `--resume` does: the restored total is whatever the store recorded,
/// and this turn's freshly computed cost accumulates on top of it.
/// A message typed mid-turn reaches the model *during* that turn, at the
/// next round boundary — not after it, by which point the work it was
/// meant to redirect is already done.
#[tokio::test]
async fn an_interjection_joins_the_turn_it_was_typed_into() {
    let provider = Arc::new(
        ScriptedProvider::tool_call_then_text(
            "call_1",
            "read_file",
            serde_json::json!({"path": "a.txt"}),
            "done",
        )
        .with_id("anthropic"),
    );
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(
        provider.clone(),
        Arc::new(NoTools),
        "fake-model".to_string(),
        tool_ctx,
    )
    .with_permission_policy(PermissionPolicy::Skip);

    // Queued before the turn starts, which is the same thing the driver
    // does mid-turn — the point is that `run_turn` reads it per round.
    agent
        .interjection_queue()
        .lock()
        .unwrap()
        .push_back("also handle the errors".to_string());

    run_collect(&mut agent, "fix the parser", CancellationToken::new()).await;

    let sent = provider.last_request().unwrap();
    assert!(
        sent.messages
            .iter()
            .any(|m| m.text() == "also handle the errors"),
        "the interjection never reached the model: {:?}",
        sent.messages.iter().map(|m| m.text()).collect::<Vec<_>>()
    );
    // As a plain user message, with nothing wrapped around it telling the
    // model what to conclude: whether it redirects the work or adds to it
    // is the judgement the model is there to make.
    assert!(agent
        .history()
        .iter()
        .any(|m| m.role == Role::User && m.text() == "also handle the errors"));
}

/// It is announced, so a frontend can show *when* it landed rather than
/// leaving the user wondering whether it was seen.
#[tokio::test]
async fn an_interjection_is_announced_when_it_lands() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply("ok")]).with_id("anthropic"));
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(
        provider,
        Arc::new(NoTools),
        "fake-model".to_string(),
        tool_ctx,
    );
    agent
        .interjection_queue()
        .lock()
        .unwrap()
        .push_back("one more thing".to_string());

    let (_, events) = run_collect(&mut agent, "go", CancellationToken::new()).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::UserInterjected(t) if t == "one more thing")),
        "no announcement: {events:?}"
    );
}

/// The queue drains completely: two messages typed in quick succession
/// must not leave one behind for a turn that may never come.
#[tokio::test]
async fn every_pending_interjection_lands_in_the_same_round() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply("ok")]).with_id("anthropic"));
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(
        provider.clone(),
        Arc::new(NoTools),
        "fake-model".to_string(),
        tool_ctx,
    );
    {
        let queue = agent.interjection_queue();
        let mut queue = queue.lock().unwrap();
        queue.push_back("first".to_string());
        queue.push_back("second".to_string());
    }

    run_collect(&mut agent, "go", CancellationToken::new()).await;

    let sent = provider.last_request().unwrap();
    let texts: Vec<String> = sent.messages.iter().map(|m| m.text()).collect();
    assert!(texts.contains(&"first".to_string()), "{texts:?}");
    assert!(texts.contains(&"second".to_string()), "{texts:?}");
}

#[tokio::test]
async fn a_resumed_session_keeps_accumulating_from_its_restored_total() {
    let provider = Arc::new(
        ScriptedProvider::streams([text_reply_with_usage("ok", prompt_usage(1_000_000))])
            .with_id("anthropic"),
    );
    let mut agent = priced_agent(provider, "claude-sonnet-5");
    agent.seed_session_totals(
        Usage {
            input_tokens: 500,
            output_tokens: 250,
            ..Usage::default()
        },
        41.5,
        0,
    );

    run_collect(&mut agent, "hi", CancellationToken::new()).await;

    assert!((agent.session_cost_usd() - 44.5).abs() < 1e-9);
    assert_eq!(agent.session_usage().input_tokens, 1_000_500);
}
