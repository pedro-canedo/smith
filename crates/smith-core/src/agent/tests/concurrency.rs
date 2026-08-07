//! Dispatching a round's `ReadOnly` calls at once.

use super::*;

// ---- concurrent ReadOnly tool calls ------------------------------------

/// Builds a round of `n` `read_file` calls, ids `call_0..call_n`, followed
/// by a plain text turn.
fn read_round(n: usize) -> Arc<ScriptedProvider> {
    let ids: Vec<String> = (0..n).map(|i| format!("call_{i}")).collect();
    let calls: Vec<(&str, &str, serde_json::Value)> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), "read_file", serde_json::json!({ "n": i })))
        .collect();
    Arc::new(ScriptedProvider::streams([
        tool_calls_reply(&calls),
        text_reply("done"),
    ]))
}

/// Every call rendezvouses at a barrier before returning, so the turn can
/// only finish if that many calls were inside `execute` *at the same
/// instant*. Serial execution deadlocks instead of merely being slower,
/// which is the point — "it finished" proves nothing on its own.
struct BarrierTools {
    barrier: Arc<tokio::sync::Barrier>,
    /// Once the barrier has opened, later calls sail past it. Without this
    /// a round longer than the barrier's width would hang on the second
    /// cycle. Only ever read by a call admitted *after* one of the first
    /// batch returned, so it is always already set by then.
    opened: Arc<std::sync::atomic::AtomicBool>,
    live: Arc<AtomicUsize>,
    /// High-water mark of `live` — the concurrency bound, observed.
    peak: Arc<AtomicUsize>,
}

impl BarrierTools {
    fn new(width: usize) -> Self {
        Self {
            barrier: Arc::new(tokio::sync::Barrier::new(width)),
            opened: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            live: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl ToolExecutor for BarrierTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
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
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        if !self.opened.load(Ordering::SeqCst) {
            self.barrier.wait().await;
            self.opened.store(true, Ordering::SeqCst);
        }
        // A call that has merely been *woken* has not yet freed its place.
        // Yielding once more here is what gives a round wider than the
        // bound the chance to admit its surplus — and so what lets `peak`
        // catch an unbounded implementation instead of silently agreeing
        // with a bounded one.
        tokio::task::yield_now().await;
        self.live.fetch_sub(1, Ordering::SeqCst);
        ToolResult::ok("read")
    }
}

#[tokio::test]
async fn three_readonly_calls_in_one_round_actually_overlap() {
    let tools = BarrierTools::new(3);
    let peak = tools.peak.clone();
    let mut agent = agent_for(read_round(3), Arc::new(tools));

    // A serial loop can never satisfy a three-way barrier, so it hangs —
    // the timeout is what turns that into a failure instead of a hung suite.
    let turn = run_collect(&mut agent, "explore", CancellationToken::new());
    let (completed, _) = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the three reads never ran at the same time");

    assert!(completed);
    assert_eq!(peak.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn no_more_than_the_bound_run_at_once() {
    // Wider than the bound: the extra calls have to queue behind the
    // first batch rather than pile on.
    const CALLS: usize = MAX_CONCURRENT_TOOLS + 4;
    let tools = BarrierTools::new(MAX_CONCURRENT_TOOLS);
    let peak = tools.peak.clone();
    let mut agent = agent_for(read_round(CALLS), Arc::new(tools));

    let turn = run_collect(&mut agent, "explore", CancellationToken::new());
    let (completed, _) = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("fewer than the bound ever ran at once");

    assert!(completed);
    // Exactly the bound: the barrier opening proves it reached it, and
    // this proves nothing beyond it was ever admitted.
    assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENT_TOOLS);
}

/// Three ReadOnly calls that finish in the exact reverse of the order the
/// model asked for them. The last call is released as soon as everything
/// has started, and each call opens its predecessor's gate on the way out.
struct ReverseOrderTools {
    started: Arc<tokio::sync::Barrier>,
    gates: std::sync::Mutex<Vec<Option<oneshot::Receiver<()>>>>,
    openers: std::sync::Mutex<Vec<Option<oneshot::Sender<()>>>>,
    finished: std::sync::Mutex<Vec<usize>>,
}

impl ReverseOrderTools {
    fn new(n: usize) -> Self {
        let mut gates = Vec::with_capacity(n);
        let mut openers = Vec::with_capacity(n);
        for _ in 0..n {
            let (tx, rx) = oneshot::channel();
            gates.push(Some(rx));
            openers.push(Some(tx));
        }
        // The last call needs no predecessor to let it through.
        if let Some(last) = openers.last_mut().and_then(Option::take) {
            let _ = last.send(());
        }
        Self {
            started: Arc::new(tokio::sync::Barrier::new(n)),
            gates: std::sync::Mutex::new(gates),
            openers: std::sync::Mutex::new(openers),
            finished: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ToolExecutor for ReverseOrderTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
    }

    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        Some(PermissionClass::ReadOnly)
    }

    async fn execute(
        &self,
        _name: &str,
        input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let n = input["n"].as_u64().unwrap() as usize;
        self.started.wait().await;
        let gate = self.gates.lock().unwrap()[n].take().unwrap();
        let _ = gate.await;
        self.finished.lock().unwrap().push(n);
        if n > 0 {
            if let Some(opener) = self.openers.lock().unwrap()[n - 1].take() {
                let _ = opener.send(());
            }
        }
        ToolResult::ok(format!("body of file {n}"))
    }
}

#[tokio::test]
async fn results_keep_the_models_order_however_the_calls_finish() {
    let tools = Arc::new(ReverseOrderTools::new(3));
    let finished = Arc::clone(&tools);
    let mut agent = agent_for(read_round(3), tools);

    let turn = run_collect(&mut agent, "explore", CancellationToken::new());
    let (completed, _) = tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .expect("the reads did not overlap, so nothing could finish out of order");
    assert!(completed);

    // The premise: they really did complete backwards.
    assert_eq!(*finished.finished.lock().unwrap(), vec![2, 1, 0]);

    // The guarantee: the model still sees them forwards, each result
    // attached to the call it belongs to.
    assert_eq!(
        collect_ids(agent.history(), false),
        vec!["call_0", "call_1", "call_2"]
    );
    for n in 0..3 {
        assert_eq!(
            tool_result_for(agent.history(), &format!("call_{n}")),
            format!("body of file {n}")
        );
    }
}

/// Logs `start:<id>` and `end:<id>` for every call, and yields once in
/// between so a call that is genuinely concurrent with another shows up as
/// two starts before either end.
struct LoggingTools {
    log: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl ToolExecutor for LoggingTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
    }

    fn permission_class(&self, name: &str) -> Option<PermissionClass> {
        Some(match name {
            "read_file" => PermissionClass::ReadOnly,
            _ => PermissionClass::Mutating,
        })
    }

    async fn execute(
        &self,
        _name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let id = ctx.tool_call_id().unwrap_or("?").to_string();
        let _ = input;
        self.log.lock().unwrap().push(format!("start:{id}"));
        tokio::task::yield_now().await;
        self.log.lock().unwrap().push(format!("end:{id}"));
        ToolResult::ok("ok")
    }
}

#[tokio::test]
async fn a_mutating_call_splits_the_round_and_runs_on_its_own() {
    let provider = Arc::new(ScriptedProvider::streams([
        tool_calls_reply(&[
            ("read_a", "read_file", json_empty()),
            ("read_b", "read_file", json_empty()),
            ("write_c", "write_file", json_empty()),
            ("read_d", "read_file", json_empty()),
        ]),
        text_reply("done"),
    ]));
    let tools = Arc::new(LoggingTools {
        log: std::sync::Mutex::new(Vec::new()),
    });
    // Skip, so the Mutating call is not serialised merely by its prompt.
    let mut agent = agent_for(provider, tools.clone());

    let (completed, _) = run_collect(&mut agent, "go", CancellationToken::new()).await;
    assert!(completed);

    let log = tools.log.lock().unwrap().clone();
    let at = |entry: &str| {
        log.iter()
            .position(|e| e == entry)
            .unwrap_or_else(|| panic!("{entry} missing from {log:?}"))
    };

    // The leading run of reads overlaps.
    assert!(at("start:read_b") < at("end:read_a"), "{log:?}");

    // The write does not overlap anything: its end is the very next entry.
    assert_eq!(log[at("start:write_c") + 1], "end:write_c", "{log:?}");

    // And nothing that follows the write starts before it is done — this
    // is what makes a read placed after a write in the same round still
    // see that write.
    assert!(at("start:read_d") > at("end:write_c"), "{log:?}");

    // The cost of splitting into contiguous runs rather than hoisting
    // every read to the front: `read_d` runs alone instead of joining the
    // other two. Asserted so the trade-off is visible, not incidental.
    assert!(at("start:read_d") > at("end:read_b"), "{log:?}");
}

/// Cancels the turn from inside the first call of a wide concurrent round.
struct CancelOnFirstReadTools {
    cancel: CancellationToken,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolExecutor for CancelOnFirstReadTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
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
        self.cancel.cancel();
        ToolResult::ok("read")
    }
}

/// The invariant a concurrent round is most likely to break: results are
/// no longer appended in completion order, so an early exit could leave a
/// gap. It cannot — the slots are pre-seeded and only ever overwritten.
#[tokio::test]
async fn cancelling_a_concurrent_round_still_answers_every_tool_use() {
    const CALLS: usize = MAX_CONCURRENT_TOOLS + 4;
    let cancel = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(CancelOnFirstReadTools {
        cancel: cancel.clone(),
        calls: calls.clone(),
    });
    let mut agent = agent_for(read_round(CALLS), tools);

    let (completed, _) = run_collect(&mut agent, "explore", cancel).await;

    assert!(!completed, "a cancelled turn is not a normal completion");
    let ran = calls.load(Ordering::SeqCst);
    assert!(ran < CALLS, "cancellation stopped nothing: {ran} calls ran");

    let uses = collect_ids(agent.history(), true);
    let answers = collect_ids(agent.history(), false);
    assert_eq!(uses.len(), CALLS);
    assert_eq!(uses, answers, "every tool_use must have a tool_result");

    // The calls that never started say so, rather than looking successful.
    let last = tool_result_for(agent.history(), &format!("call_{}", CALLS - 1));
    assert!(last.contains("cancelled"), "got: {last}");
}

#[test]
fn only_readonly_tools_are_ever_run_concurrently() {
    struct Classes;
    #[async_trait]
    impl ToolExecutor for Classes {
        fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
            Vec::new()
        }
        fn permission_class(&self, name: &str) -> Option<PermissionClass> {
            match name {
                "read_file" | "ask_user" | "write_tasks" => Some(PermissionClass::ReadOnly),
                "write_file" => Some(PermissionClass::Mutating),
                "run_bash" => Some(PermissionClass::Dangerous),
                _ => None,
            }
        }
        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            ToolResult::error("unused")
        }
    }

    let agent = Agent::new(
        Arc::new(ScriptedProvider::streams([])),
        Arc::new(Classes),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    );

    assert!(agent.is_concurrency_safe("read_file"));
    assert!(!agent.is_concurrency_safe("write_file"));
    assert!(!agent.is_concurrency_safe("run_bash"));
    // ReadOnly, but intercepted by name and needing `&mut self`.
    assert!(!agent.is_concurrency_safe("ask_user"));
    assert!(!agent.is_concurrency_safe("write_tasks"));
    // Delegation needs `&mut self` too — and two children at once would
    // bill two conversations in parallel.
    assert!(!agent.is_concurrency_safe(subagent::TASK_TOOL));
    // An unregistered name is treated as Dangerous everywhere else too.
    assert!(!agent.is_concurrency_safe("mystery_tool"));
}
