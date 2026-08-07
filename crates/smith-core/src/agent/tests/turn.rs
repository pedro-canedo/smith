//! The turn loop: empty turns, the plan gate, scratch scope, `write_tasks`.

use super::*;

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

/// Like `RecordingTools`, but vouches that every call is confined to the
/// session's scratch directory — the executor-side half of
/// `Tool::scratch_scoped`.
struct ScratchScopedTools {
    executed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ToolExecutor for ScratchScopedTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
    }

    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        Some(PermissionClass::Mutating)
    }

    fn scratch_scoped(&self, _name: &str, _input: &serde_json::Value, _ctx: &ToolContext) -> bool {
        true
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
        ToolResult::ok("wrote scratch")
    }
}

#[tokio::test]
async fn a_scratch_scoped_call_skips_the_permission_prompt_under_ask() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(write_file_then_done());
    let tools = Arc::new(ScratchScopedTools {
        executed: executed.clone(),
    });
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
        .with_permission_policy(PermissionPolicy::Ask); // would normally prompt for Mutating

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    // Dropped up front: a prompt attempt now fails the call with
    // "permission channel closed" instead of hanging the test, so a
    // regression shows up as `executed == false`, not as a timeout.
    drop(permission_rx);

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
        "a scratch-confined Mutating call must run without a prompt"
    );
}

/// The three intercepted tools never reach `ToolExecutor::execute`, which
/// is where every dispatched call is checked against its published schema.
/// They are checked before the interception instead, so "a tool call is
/// validated against the schema the model was shown" holds on every path
/// rather than on most of them.
#[tokio::test]
async fn the_intercepted_tools_are_checked_against_their_schema_too() {
    for tool in INTERCEPTED_TOOLS {
        let provider = Arc::new(
            ScriptedProvider::tool_call_then_text(
                "call_1",
                tool,
                // Missing every required property, whatever they are.
                serde_json::json!({}),
                "done",
            )
            .with_id("anthropic"),
        );
        let tool_ctx = ToolContext::new(".", "test-session");
        let mut agent = Agent::new(
            provider,
            Arc::new(RejectingSchemaTools),
            "fake-model".to_string(),
            tool_ctx,
        );

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        agent
            .run_turn(
                "go".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;

        let mut rejected = false;
        while let Ok(event) = events_rx.try_recv() {
            if matches!(&event, AgentEvent::ToolCallResult { output, is_error, .. }
                    if *is_error && output.contains("schema says no"))
            {
                rejected = true;
            }
        }
        assert!(rejected, "{tool} ran without its arguments being checked");
    }
}

/// Refuses every argument object, so a call that was validated at all is
/// distinguishable from one that was not.
struct RejectingSchemaTools;

#[async_trait]
impl ToolExecutor for RejectingSchemaTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
    }

    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        Some(PermissionClass::ReadOnly)
    }

    fn validate_input(&self, _name: &str, _input: &serde_json::Value) -> Result<(), String> {
        Err("schema says no".to_string())
    }

    async fn execute(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        ToolResult::ok("should never run")
    }
}

/// `task` is classed `ReadOnly` because a child's own tools are, and it
/// therefore never reached the permission channel — the only place
/// `--allowed-tools` can see a call. Unattended, that left "spawn a whole
/// agent and spend the user's money" available to a job that named no
/// tools at all.
#[tokio::test]
async fn task_must_be_named_when_nobody_is_watching() {
    let provider = Arc::new(
        ScriptedProvider::tool_call_then_text(
            "call_1",
            subagent::TASK_TOOL,
            serde_json::json!({"description": "look", "prompt": "read the repo"}),
            "done",
        )
        .with_id("anthropic"),
    );
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(
        provider,
        Arc::new(NoTools),
        "fake-model".to_string(),
        tool_ctx,
    )
    .with_permission_policy(PermissionPolicy::Ask)
    .with_unattended(true);

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (permission_tx, mut permission_rx) = mpsc::unbounded_channel::<PermissionAsk>();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    // Answer the way `--allowed-tools` does when the tool is not listed.
    tokio::spawn(async move {
        while let Some(ask) = permission_rx.recv().await {
            let _ = ask.respond_to.send(PermissionDecision::Deny);
        }
    });

    agent
        .run_turn(
            "delegate it".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    let asked = events.iter().any(|e| {
            matches!(e, AgentEvent::PermissionPromptNeeded(r) if r.tool_name == subagent::TASK_TOOL)
        });
    assert!(asked, "task never reached the gate: {events:?}");
    // A refused call returns its error to the model through the history
    // rather than a `ToolCallResult` event, so the thing to assert is that
    // no child was ever spawned: `run_task` announces itself with a
    // "<name>: started" progress line before anything else.
    let spawned = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolProgress { line, .. } if line.contains("started")));
    assert!(!spawned, "a child agent was spawned anyway: {events:?}");

    // And the model is told, so it can react rather than silently retry.
    let refusal = agent.history().iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { content, is_error, .. }
                    if *is_error && content.contains("denied permission"))
        })
    });
    assert!(
        refusal,
        "the model was never told why: {:?}",
        agent.history()
    );
}

/// Interactively it stays ungated: the user is watching, the child can
/// only read, and a prompt per delegation would be pure friction.
#[tokio::test]
async fn task_is_not_gated_with_a_user_present() {
    let provider = Arc::new(
        ScriptedProvider::tool_call_then_text(
            "call_1",
            subagent::TASK_TOOL,
            serde_json::json!({"description": "look", "prompt": "read the repo"}),
            "done",
        )
        .with_id("anthropic"),
    );
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(
        provider,
        Arc::new(NoTools),
        "fake-model".to_string(),
        tool_ctx,
    )
    .with_permission_policy(PermissionPolicy::Ask)
    .with_unattended(false);

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    agent
        .run_turn(
            "delegate it".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    let mut asked = false;
    while let Ok(event) = events_rx.try_recv() {
        if matches!(&event, AgentEvent::PermissionPromptNeeded(r) if r.tool_name == subagent::TASK_TOOL)
        {
            asked = true;
        }
    }
    assert!(!asked, "a delegation prompted with the user right there");
}

/// The scratch exemption is a friction argument, and unattended there is
/// no friction to spare. It was the one case where a Mutating tool ran in
/// a headless job that named no tools at all: `--allowed-tools` is
/// answered on the permission channel, and this call never reached it.
#[tokio::test]
async fn a_scratch_scoped_call_is_still_gated_when_nobody_is_watching() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(write_file_then_done());
    let tools = Arc::new(ScratchScopedTools {
        executed: executed.clone(),
    });
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
        .with_permission_policy(PermissionPolicy::Ask)
        .with_unattended(true);

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    // Same trick as the test above: with the receiver gone, reaching the
    // channel fails the call rather than hanging, so "it asked" and "it
    // ran anyway" are distinguishable.
    drop(permission_rx);

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
        "a scratch write ran unattended without ever reaching the gate"
    );
}

/// …and the interactive behaviour is unchanged, which is the whole reason
/// the flag exists rather than the exemption simply being deleted.
#[tokio::test]
async fn the_scratch_exemption_still_applies_with_a_user_present() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(write_file_then_done());
    let tools = Arc::new(ScratchScopedTools {
        executed: executed.clone(),
    });
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
        .with_permission_policy(PermissionPolicy::Ask)
        .with_unattended(false);

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    drop(permission_rx);

    agent
        .run_turn(
            "do it".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(executed.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn the_plan_gate_still_blocks_scratch_scoped_calls() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(write_file_then_done());
    let tools = Arc::new(ScratchScopedTools {
        executed: executed.clone(),
    });
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
        .with_permission_policy(PermissionPolicy::Ask);
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
        "the scratch exemption is about friction, not authority — an \
             unapproved plan still blocks it"
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
