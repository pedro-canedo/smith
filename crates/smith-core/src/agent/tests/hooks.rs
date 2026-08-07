//! `PreToolUse`/`PostToolUse`/`UserPromptSubmit` around the tool path.

use super::*;

// ---- hooks --------------------------------------------------------
//
// These cover the *wiring*: that a hook reaches the tool path at the
// documented rung, that what it decides actually changes what runs, and
// that what it says reaches the model in a form the model can act on.
// `hooks::tests` covers the contract itself (parsing, timeouts, quoting).

/// A hook runner that answers every invocation with the same canned
/// stdout, and records what it was asked.
#[derive(Debug)]
struct CannedHook {
    stdout: String,
    code: i32,
    calls: std::sync::Mutex<Vec<String>>,
}

impl CannedHook {
    fn new(stdout: &str) -> Arc<Self> {
        Arc::new(Self {
            stdout: stdout.to_string(),
            code: 0,
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn tool_names(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|payload| {
                serde_json::from_str::<serde_json::Value>(payload).ok()?["tool_name"]
                    .as_str()
                    .map(str::to_string)
            })
            .collect()
    }
}

#[async_trait]
impl crate::hooks::HookInvoker for CannedHook {
    async fn invoke(
        &self,
        _def: &crate::hooks::HookDefinition,
        payload: String,
        _cancel: &CancellationToken,
    ) -> crate::hooks::HookOutcome {
        self.calls.lock().unwrap().push(payload);
        crate::hooks::HookOutcome::Completed {
            stdout: self.stdout.clone(),
            stderr: String::new(),
            code: self.code,
        }
    }
}

fn hook_set(
    event: crate::hooks::HookEvent,
    invoker: Arc<CannedHook>,
) -> Arc<crate::hooks::HookSet> {
    Arc::new(crate::hooks::HookSet::with_invoker(
        vec![crate::hooks::HookDefinition::new(event, "policy.sh")],
        invoker,
    ))
}

/// Records the arguments each call arrived with, and can be told to reject
/// arguments that do not carry a `path` — standing in for a real schema.
struct ArgumentRecordingTools {
    seen: std::sync::Mutex<Vec<serde_json::Value>>,
    require_path: bool,
}

impl ArgumentRecordingTools {
    fn new(require_path: bool) -> Arc<Self> {
        Arc::new(Self {
            seen: std::sync::Mutex::new(Vec::new()),
            require_path,
        })
    }
}

#[async_trait]
impl ToolExecutor for ArgumentRecordingTools {
    /// One real definition, because `subagent::resolve_tool_set`
    /// intersects a child's tools with what is actually *registered* — an
    /// executor that publishes nothing gives every child no tools at all.
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        vec![crate::message::ToolDefinition {
            name: "read_file".to_string(),
            description: "reads a file".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }]
    }

    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        Some(PermissionClass::ReadOnly)
    }

    fn validate_input(&self, _name: &str, input: &serde_json::Value) -> Result<(), String> {
        if self.require_path && input.get("path").is_none() {
            return Err("missing required property `path`".into());
        }
        Ok(())
    }

    async fn execute(
        &self,
        _name: &str,
        input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        self.seen.lock().unwrap().push(input);
        ToolResult::ok("ran")
    }
}

/// The denial has to land in *history*, not just on a card: a block the
/// model never sees is a block it will retry forever.
#[tokio::test]
async fn a_pre_tool_use_hook_denial_reaches_the_model_and_stops_the_tool() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let tools = Arc::new(RecordingTools {
        executed: executed.clone(),
    });
    let mut agent = Agent::new(
        Arc::new(write_file_then_done()),
        tools,
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_permission_policy(PermissionPolicy::Skip)
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PreToolUse,
        CannedHook::new(r#"{"decision":"deny","reason":"writes are frozen during a release"}"#),
    ));

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
        !executed.load(std::sync::atomic::Ordering::SeqCst),
        "a denied call must not run"
    );

    let denial = agent
        .history()
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                content,
                is_error: true,
                ..
            } => Some(content.clone()),
            _ => None,
        })
        .expect("the model must receive the denial as a tool result");
    assert!(denial.contains("Blocked by a PreToolUse hook"));
    assert!(denial.contains("> writes are frozen during a release"));
    assert!(
        denial.contains("Change your approach or ask the user"),
        "the model needs to be told what to do instead"
    );
}

#[tokio::test]
async fn a_pre_tool_use_hook_rewrites_the_arguments_the_tool_receives() {
    let tools = ArgumentRecordingTools::new(false);
    let mut agent = Agent::new(
        Arc::new(ScriptedProvider::tool_call_then_text(
            "call_1",
            "read_file",
            serde_json::json!({"path": "/etc/shadow"}),
            "done",
        )),
        tools.clone(),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PreToolUse,
        CannedHook::new(r#"{"tool_input":{"path":"README.md"}}"#),
    ));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    agent
        .run_turn(
            "read it".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    let seen = tools.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0], serde_json::json!({"path": "README.md"}));
}

#[tokio::test]
async fn a_hook_rewrite_that_changes_the_tool_is_refused_before_dispatch() {
    let tools = ArgumentRecordingTools::new(false);
    let mut agent = Agent::new(
        Arc::new(ScriptedProvider::tool_call_then_text(
            "call_1",
            "read_file",
            serde_json::json!({"path": "README.md"}),
            "done",
        )),
        tools.clone(),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PreToolUse,
        CannedHook::new(r#"{"tool_name":"run_bash","tool_input":{"command":"rm -rf ."}}"#),
    ));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    agent
        .run_turn(
            "read it".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(
        tools.seen.lock().unwrap().is_empty(),
        "a hook that tries to redirect the call must stop it, not run either tool"
    );
}

/// The rewrite lands *before* the schema check, not after — so a hook that
/// produces invalid arguments is caught, and caught by name.
#[tokio::test]
async fn a_hook_rewrite_the_schema_rejects_never_reaches_the_tool() {
    let tools = ArgumentRecordingTools::new(true);
    let mut agent = Agent::new(
        Arc::new(ScriptedProvider::tool_call_then_text(
            "call_1",
            "read_file",
            serde_json::json!({"path": "README.md"}),
            "done",
        )),
        tools.clone(),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PreToolUse,
        CannedHook::new(r#"{"tool_input":{"pathh":"README.md"}}"#),
    ));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    agent
        .run_turn(
            "read it".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(tools.seen.lock().unwrap().is_empty());
    let denial = agent
        .history()
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                content,
                is_error: true,
                ..
            } => Some(content.clone()),
            _ => None,
        })
        .expect("the model must be told the call was blocked");
    assert!(denial.contains("the tool's own schema rejects"));
    assert!(
        denial.contains("PreToolUse hook"),
        "the hook must be blamed, not the model"
    );
}

/// The plan gate is above the hook, so a plan-gated call never spends a
/// process on one — and the message the model gets is still the plan's.
#[tokio::test]
async fn the_plan_gate_is_decided_before_a_hook_is_consulted() {
    let invoker = CannedHook::new(r#"{"decision":"allow"}"#);
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut agent = Agent::new(
        Arc::new(write_file_then_done()),
        Arc::new(RecordingTools {
            executed: executed.clone(),
        }),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_permission_policy(PermissionPolicy::Skip)
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PreToolUse,
        invoker.clone(),
    ));
    agent.set_plan_gated(true);

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

    assert!(!executed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        invoker.calls.lock().unwrap().is_empty(),
        "a hook must not be run for a call the plan gate already refused"
    );
}

/// And the converse: the hook is above the prompt decision, so the one
/// setting that turns every prompt off does not turn hooks off with it.
#[tokio::test]
async fn a_hook_still_runs_when_the_policy_would_skip_the_prompt() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut agent = Agent::new(
        Arc::new(write_file_then_done()),
        Arc::new(RecordingTools {
            executed: executed.clone(),
        }),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_permission_policy(PermissionPolicy::Skip)
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PreToolUse,
        CannedHook::new(r#"{"decision":"deny","reason":"no"}"#),
    ));

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
        !executed.load(std::sync::atomic::Ordering::SeqCst),
        "`/permission skip` must not disable hooks"
    );
}

/// Read-only calls are batched down a second path that skips the plan gate
/// and the prompt. It must not skip the hook.
#[tokio::test]
async fn hooks_fire_for_concurrently_dispatched_read_only_calls() {
    let invoker = CannedHook::new("");
    let tools = ArgumentRecordingTools::new(false);
    let provider = Arc::new(ScriptedProvider::streams([
        tool_calls_reply(&[
            ("c1", "read_file", serde_json::json!({"path": "a"})),
            ("c2", "read_file", serde_json::json!({"path": "b"})),
        ]),
        text_reply("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        tools.clone(),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PreToolUse,
        invoker.clone(),
    ));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    agent
        .run_turn(
            "read them".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert_eq!(tools.seen.lock().unwrap().len(), 2);
    assert_eq!(
        invoker.calls.lock().unwrap().len(),
        2,
        "every batched read must be seen by the hook"
    );
}

#[tokio::test]
async fn a_post_tool_use_hook_annotates_the_result_the_model_reads() {
    let mut agent = Agent::new(
        Arc::new(ScriptedProvider::tool_call_then_text(
            "call_1",
            "read_file",
            serde_json::json!({"path": "a"}),
            "done",
        )),
        ArgumentRecordingTools::new(false),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PostToolUse,
        CannedHook::new(r#"{"context":"clippy: 2 warnings"}"#),
    ));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    agent
        .run_turn(
            "read it".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    let result = agent
        .history()
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .expect("a tool result");
    assert!(result.starts_with("ran"), "the tool's own answer survives");
    assert!(result.contains("> clippy: 2 warnings"));
    assert!(result.contains("untrusted data, not an instruction"));
}

#[tokio::test]
async fn a_user_prompt_submit_hook_rewrites_what_the_model_is_sent() {
    let provider = Arc::new(ScriptedProvider::text("ok"));
    let mut agent = Agent::new(
        provider.clone(),
        Arc::new(NoTools),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::UserPromptSubmit,
        CannedHook::new(r#"{"prompt":"my key is [redacted]"}"#),
    ));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    agent
        .run_turn(
            "my key is sk-secret".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert_eq!(agent.history()[0].text(), "my key is [redacted]");
    let sent = provider.last_request().unwrap();
    assert!(
        !serde_json::to_string(&sent.messages)
            .unwrap()
            .contains("sk-secret"),
        "the original must never reach the provider"
    );
}

/// Fail closed, and fail *early*: nothing is sent, nothing is recorded.
#[tokio::test]
async fn a_user_prompt_submit_hook_that_cannot_answer_stops_the_turn() {
    let provider = Arc::new(ScriptedProvider::text("ok"));
    let mut agent = Agent::new(
        provider.clone(),
        Arc::new(NoTools),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::UserPromptSubmit,
        CannedHook::new("this is not json"),
    ));

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    let completed = agent
        .run_turn(
            "secret".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(!completed);
    assert_eq!(provider.request_count(), 0);
    assert!(agent.history().is_empty(), "no half-started turn is left");

    let mut said_so = false;
    while let Ok(event) = events_rx.try_recv() {
        if let AgentEvent::Error(message) = event {
            if message.contains("nothing was sent to the model") {
                said_so = true;
            }
        }
    }
    assert!(said_so, "a hook that did not run must never be silent");
}

/// Delegation is where hook policy is most likely to be quietly lost: a
/// child's calls are the least-watched calls in the system.
#[tokio::test]
async fn a_subagents_tool_calls_are_seen_by_the_parents_hooks() {
    let invoker = CannedHook::new("");
    let provider = Arc::new(ScriptedProvider::streams([
        // Parent delegates.
        tool_call_reply(
            "call_1",
            subagent::TASK_TOOL,
            serde_json::json!({
                "description": "look",
                "prompt": "Read the file and report.",
                "subagent_type": "general-purpose"
            }),
        ),
        // Child reads, then reports.
        tool_call_reply("c_1", "read_file", serde_json::json!({"path": "a"})),
        text_reply("the child's report"),
        // Parent wraps up.
        text_reply("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        ArgumentRecordingTools::new(false),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::PreToolUse,
        invoker.clone(),
    ));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
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

    let seen = invoker.tool_names();
    assert!(
        seen.contains(&subagent::TASK_TOOL.to_string()),
        "the delegation itself is a tool call and must be hookable: {seen:?}"
    );
    assert!(
        seen.contains(&"read_file".to_string()),
        "the child's own calls must be hookable too: {seen:?}"
    );

    // And the child is labelled, so a hook that only wants the user's own
    // calls can filter — the reason inheriting hooks is safe to default on.
    let payloads = invoker.calls.lock().unwrap();
    let child = payloads
        .iter()
        .map(|p| serde_json::from_str::<serde_json::Value>(p).unwrap())
        .find(|p| p["tool_name"] == "read_file")
        .unwrap();
    assert_eq!(child["agent"], "subagent");
    assert_eq!(child["depth"], 1);
}

/// A child's "prompt" is written by the parent model. Firing an event
/// called `UserPromptSubmit` on it would misreport who said it.
#[tokio::test]
async fn a_subagents_prompt_does_not_fire_the_user_prompt_hook() {
    let invoker = CannedHook::new("");
    let provider = Arc::new(ScriptedProvider::streams([
        tool_call_reply(
            "call_1",
            subagent::TASK_TOOL,
            serde_json::json!({
                "description": "look",
                "prompt": "Read the file and report.",
                "subagent_type": "general-purpose"
            }),
        ),
        text_reply("the child's report"),
        text_reply("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        ArgumentRecordingTools::new(false),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_hooks(hook_set(
        crate::hooks::HookEvent::UserPromptSubmit,
        invoker.clone(),
    ));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
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

    assert_eq!(
        invoker.calls.lock().unwrap().len(),
        1,
        "exactly one prompt was submitted by a user"
    );
}

#[test]
fn parse_tasks_rejects_empty_list() {
    let err = parse_tasks(&serde_json::json!({"tasks": []})).unwrap_err();
    assert!(err.contains("non-empty"));
}

#[test]
fn parse_tasks_rejects_unknown_status() {
    let err = parse_tasks(&serde_json::json!({
        "tasks": [{"content": "x", "status": "done"}]
    }))
    .unwrap_err();
    assert!(err.contains("unknown task status"));
}

#[test]
fn parse_tasks_reads_content_and_status() {
    let tasks = parse_tasks(&serde_json::json!({
        "tasks": [
            {"content": "a", "status": "completed"},
            {"content": "b", "status": "pending"},
        ]
    }))
    .unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].status, TaskStatus::Completed);
    assert_eq!(tasks[1].status, TaskStatus::Pending);
}

#[test]
fn parse_tasks_accepts_blocked_with_a_reason_and_review() {
    let tasks = parse_tasks(&serde_json::json!({
        "tasks": [
            {"content": "a", "status": "blocked", "blocked_reason": "needs a key", "id": "t1"},
            {"content": "b", "status": "review"},
        ]
    }))
    .unwrap();
    assert_eq!(tasks[0].status, TaskStatus::Blocked);
    assert_eq!(tasks[0].blocked_reason.as_deref(), Some("needs a key"));
    assert_eq!(tasks[0].id.as_deref(), Some("t1"));
    assert_eq!(tasks[1].status, TaskStatus::Review);
    assert_eq!(
        tasks[1].id, None,
        "ids are stamped later, not invented here"
    );
}

/// The stamp is smith's, never the model's — a model has no clock worth
/// trusting, and a fabricated recency would order the board by fiction.
#[test]
fn parse_tasks_discards_a_model_supplied_updated_at() {
    let tasks = parse_tasks(&serde_json::json!({
        "tasks": [{"content": "a", "status": "pending", "updated_at": 12345}]
    }))
    .unwrap();
    assert_eq!(tasks[0].updated_at, None);
}

#[test]
fn stamping_assigns_positional_ids_only_where_the_model_sent_none() {
    let incoming = vec![
        Task {
            id: Some("keep-me".into()),
            ..Task::new("a", TaskStatus::Pending)
        },
        Task::new("b", TaskStatus::Pending),
    ];
    let stamped = super::super::interactive::stamp_tasks(&[], incoming, 1_000);
    assert_eq!(stamped[0].id.as_deref(), Some("keep-me"));
    assert_eq!(stamped[1].id.as_deref(), Some("t2"));
    assert_eq!(stamped[0].updated_at, Some(1_000));
}

/// Per-card recency: an unchanged card keeps its stamp across the full-list
/// replacement `write_tasks` always sends; a changed one is refreshed.
#[test]
fn an_unchanged_task_keeps_its_updated_at_across_snapshots() {
    let first = super::super::interactive::stamp_tasks(
        &[],
        vec![
            Task::new("unchanged", TaskStatus::Pending),
            Task::new("will change", TaskStatus::Pending),
        ],
        1_000,
    );
    let second = super::super::interactive::stamp_tasks(
        &first,
        vec![
            Task::new("unchanged", TaskStatus::Pending),
            Task::new("will change", TaskStatus::InProgress),
        ],
        2_000,
    );
    assert_eq!(second[0].updated_at, Some(1_000), "nothing changed");
    assert_eq!(second[1].updated_at, Some(2_000), "the status moved");
}

/// The compat pin: without the optional fields a task serializes exactly as
/// the original two-field shape, so stream-json consumers see nothing new
/// until the fields are actually used.
#[test]
fn a_task_without_new_fields_serializes_exactly_as_before() {
    let json = serde_json::to_string(&Task::new("ship it", TaskStatus::InProgress)).unwrap();
    assert_eq!(json, r#"{"content":"ship it","status":"in_progress"}"#);
}

#[test]
fn finds_fallback_tool_call_that_is_the_whole_message() {
    let known = defs(&["write_file"]);
    let text = r#"{"name": "write_file", "arguments": {"path": "a.txt", "content": "hi"}}"#;
    let (name, args, before, after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(name, "write_file");
    assert_eq!(args["path"], "a.txt");
    assert!(before.is_empty());
    assert!(after.is_empty());
}

#[test]
fn finds_fallback_tool_call_with_leading_prose() {
    let known = defs(&["write_file"]);
    let text = "Sure, I'll create that file now.\n\n{\"name\": \"write_file\", \"arguments\": {\"path\": \"a.txt\"}}";
    let (name, _args, before, after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(name, "write_file");
    assert_eq!(before, "Sure, I'll create that file now.");
    assert!(after.is_empty());
}

#[test]
fn ignores_json_naming_an_unregistered_tool() {
    let known = defs(&["write_file"]);
    let text = r#"{"name": "delete_everything", "arguments": {}}"#;
    assert!(find_fallback_tool_call(text, &known).is_none());
}

#[test]
fn ignores_plain_text_with_no_json() {
    let known = defs(&["write_file"]);
    assert!(find_fallback_tool_call("just a normal reply", &known).is_none());
}

/// The flat envelope the system prompt asks for when a model has no
/// structured tool channel: the remaining top-level fields are the
/// arguments.
#[test]
fn finds_the_flat_action_envelope() {
    let known = defs(&["web_search"]);
    let text = r#"{"action": "web_search", "query": "rust 2024 edition"}"#;
    let (name, args, before, after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(name, "web_search");
    assert_eq!(args, serde_json::json!({"query": "rust 2024 edition"}));
    assert!(before.is_empty());
    assert!(after.is_empty());
}

#[test]
fn action_envelope_keeps_every_field_but_the_action_itself() {
    let known = defs(&["web_search"]);
    let text = r#"{"action": "web_search", "query": "rust", "num_results": 5}"#;
    let (_name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(args, serde_json::json!({"query": "rust", "num_results": 5}));
}

/// The two envelopes crossed. A model writing this means the inner object,
/// not a literal `arguments` argument.
#[test]
fn action_envelope_unwraps_a_nested_arguments_object() {
    let known = defs(&["web_search"]);
    let text = r#"{"action": "web_search", "arguments": {"query": "rust"}}"#;
    let (_name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(args, serde_json::json!({"query": "rust"}));
}

/// The registered-tool check is the whole safety property: an `action`
/// field is common enough in ordinary JSON that dispatching on it blindly
/// would turn quoted data into tool calls.
#[test]
fn ignores_an_action_naming_an_unregistered_tool() {
    let known = defs(&["web_search"]);
    let text = r#"{"action": "delete_everything", "path": "/"}"#;
    assert!(find_fallback_tool_call(text, &known).is_none());
}

#[test]
fn finds_the_action_envelope_after_prose() {
    let known = defs(&["web_search"]);
    let text = "I need to look this up.\n\n{\"action\": \"web_search\", \"query\": \"rust\"}";
    let (name, _args, before, after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(name, "web_search");
    assert_eq!(before, "I need to look this up.");
    assert!(after.is_empty());
}
