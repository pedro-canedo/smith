//! Snapshotting files before a mutating tool runs.

use super::*;

// ---- checkpointing ------------------------------------------------------
//
// The store itself is tested in `smith_tools::checkpoint`; what belongs
// here is the *hook* — that it fires at the right moment, that it never
// takes a turn down with it, and that a call the gate refuses leaves no
// trace behind.

/// Records what the agent asked it to do, and can be told to fail every
/// request — the interesting case, because a checkpoint failure has to be
/// invisible to the tool call.
#[derive(Default)]
struct SpyCheckpointer {
    failing: bool,
    calls: std::sync::Mutex<Vec<String>>,
}

impl SpyCheckpointer {
    fn failing() -> Self {
        Self {
            failing: true,
            ..Self::default()
        }
    }

    fn log(&self, entry: String) -> Result<(), String> {
        self.calls.lock().unwrap().push(entry);
        if self.failing {
            Err("the .smith directory is read-only".into())
        } else {
            Ok(())
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl crate::checkpoint::Checkpointer for SpyCheckpointer {
    async fn begin_turn(&self, _session_id: &str) -> u64 {
        self.calls.lock().unwrap().push("begin_turn".into());
        1
    }
    async fn snapshot_before(
        &self,
        _session_id: &str,
        _turn: u64,
        path: &std::path::Path,
    ) -> Result<(), String> {
        self.log(format!("before:{}", path.display()))
    }
    async fn snapshot_after(
        &self,
        _session_id: &str,
        _turn: u64,
        path: &std::path::Path,
    ) -> Result<(), String> {
        self.log(format!("after:{}", path.display()))
    }
    async fn note_uncovered(
        &self,
        _session_id: &str,
        _turn: u64,
        tool: &str,
    ) -> Result<(), String> {
        self.log(format!("uncovered:{tool}"))
    }
}

/// Stands in for `write_file` (declares its path) or `run_bash` (does
/// not), at whichever permission class the test needs.
struct PathDeclaringTools {
    class: PermissionClass,
    declares: bool,
    executed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ToolExecutor for PathDeclaringTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
    }
    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        Some(self.class)
    }
    fn snapshot_paths(
        &self,
        _name: &str,
        _input: &serde_json::Value,
        _ctx: &ToolContext,
    ) -> Vec<std::path::PathBuf> {
        if self.declares {
            vec![std::path::PathBuf::from("/proj/src/main.rs")]
        } else {
            Vec::new()
        }
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

fn checkpointed_agent(checkpointer: Arc<SpyCheckpointer>, tools: Arc<PathDeclaringTools>) -> Agent {
    Agent::new(
        Arc::new(write_file_then_done()),
        tools,
        "fake-model".to_string(),
        ToolContext::new("/proj", "test-session"),
    )
    .with_permission_policy(PermissionPolicy::Skip)
    .with_checkpointer(checkpointer)
}

/// The requirement that outranks the feature: losing the ability to undo a
/// write is bad; refusing to do the work because we could not prepare to
/// undo it is worse.
#[tokio::test]
async fn a_snapshot_failure_does_not_fail_the_tool_call() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let checkpointer = Arc::new(SpyCheckpointer::failing());
    let mut agent = checkpointed_agent(
        checkpointer.clone(),
        Arc::new(PathDeclaringTools {
            class: PermissionClass::Mutating,
            declares: true,
            executed: executed.clone(),
        }),
    );

    let (completed, events) = run_collect(&mut agent, "write it", CancellationToken::new()).await;

    assert!(completed, "the turn should have run to a normal completion");
    assert!(
        executed.load(std::sync::atomic::Ordering::SeqCst),
        "the tool was skipped because its checkpoint could not be written"
    );
    let failed = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolCallResult { is_error: true, .. }));
    assert!(!failed, "the tool call was reported as an error");
    // Not silent either — the warning rides the advisory progress channel,
    // which cannot fail a turn the way an `Error` event would.
    let warned = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolProgress { line, .. } if line.contains("/rewind")));
    assert!(warned, "the user was never told the write is not undoable");
}

/// The hook sits after the gates, so a refused call never leaves an object
/// behind and never snapshots a file that was not written.
#[tokio::test]
async fn a_tool_the_plan_gate_refuses_is_never_snapshotted() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let checkpointer = Arc::new(SpyCheckpointer::default());
    let mut agent = checkpointed_agent(
        checkpointer.clone(),
        Arc::new(PathDeclaringTools {
            class: PermissionClass::Mutating,
            declares: true,
            executed: executed.clone(),
        }),
    );
    agent.set_plan_gated(true);

    run_collect(&mut agent, "write it", CancellationToken::new()).await;

    assert!(!executed.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(checkpointer.calls(), vec!["begin_turn".to_string()]);
}

#[tokio::test]
async fn a_declared_path_is_snapshotted_on_both_sides_of_the_call() {
    let checkpointer = Arc::new(SpyCheckpointer::default());
    let mut agent = checkpointed_agent(
        checkpointer.clone(),
        Arc::new(PathDeclaringTools {
            class: PermissionClass::Mutating,
            declares: true,
            executed: Default::default(),
        }),
    );

    run_collect(&mut agent, "write it", CancellationToken::new()).await;

    assert_eq!(
        checkpointer.calls(),
        vec![
            "begin_turn".to_string(),
            "before:/proj/src/main.rs".to_string(),
            "after:/proj/src/main.rs".to_string(),
        ]
    );
}

/// `run_bash` and every MCP tool land here: they can change anything and
/// will not say what. Recording the call is the only reason `/rewind` can
/// admit the gap instead of implying it covered the whole turn.
#[tokio::test]
async fn a_mutating_tool_that_declares_no_paths_is_recorded_as_uncovered() {
    let checkpointer = Arc::new(SpyCheckpointer::default());
    let mut agent = checkpointed_agent(
        checkpointer.clone(),
        Arc::new(PathDeclaringTools {
            class: PermissionClass::Dangerous,
            declares: false,
            executed: Default::default(),
        }),
    );

    run_collect(&mut agent, "run it", CancellationToken::new()).await;

    assert!(checkpointer
        .calls()
        .contains(&"uncovered:write_file".to_string()));
}

/// A read-only tool declaring no paths is not a gap — it is a tool that
/// wrote nothing, and reporting it would drown the real warning.
#[tokio::test]
async fn a_read_only_tool_is_not_recorded_as_uncovered() {
    let checkpointer = Arc::new(SpyCheckpointer::default());
    let mut agent = checkpointed_agent(
        checkpointer.clone(),
        Arc::new(PathDeclaringTools {
            class: PermissionClass::ReadOnly,
            declares: false,
            executed: Default::default(),
        }),
    );

    run_collect(&mut agent, "read it", CancellationToken::new()).await;

    assert_eq!(checkpointer.calls(), vec!["begin_turn".to_string()]);
}

/// After a rewind the model still believes it wrote those files. The note
/// rides the next user message rather than becoming a message of its own,
/// which would leave two user messages in a row.
#[tokio::test]
async fn a_queued_note_rides_the_next_user_message_instead_of_becoming_one() {
    let provider = Arc::new(ScriptedProvider::streams([
        text_reply("ok"),
        text_reply("ok again"),
    ]));
    let mut agent = Agent::new(
        provider.clone(),
        Arc::new(NoTools),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    );
    agent.note_to_model("[smith] src/main.rs was restored.");

    run_collect(&mut agent, "carry on", CancellationToken::new()).await;

    let sent = provider.last_request().unwrap();
    let first = sent.messages[0].text();
    assert!(first.contains("src/main.rs was restored"), "{first}");
    assert!(first.contains("carry on"), "{first}");
    assert_eq!(
        sent.messages
            .iter()
            .filter(|m| m.role == Role::User)
            .count(),
        1,
        "the note became a message of its own"
    );

    // Delivered once, not re-sent on every later turn.
    run_collect(&mut agent, "again", CancellationToken::new()).await;
    assert!(!agent.history()[2].text().contains("restored"));
}
