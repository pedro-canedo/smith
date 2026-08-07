//! Turn limits, and the provider retry that shares their boundary.

use super::*;

// ---- turn limits and provider retry -------------------------------

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
