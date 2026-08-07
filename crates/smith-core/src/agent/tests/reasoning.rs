//! Stripping `<think>` blocks, as a unit and through `run_turn`.

use super::*;

// --- reasoning tags in the text channel --------------------------------

/// Runs `chunks` through the filter as if they were streamed deltas,
/// returning the visible text and how many tags were removed.
fn strip_reasoning(chunks: &[&str]) -> (String, u32) {
    let mut filter = ReasoningFilter::new();
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&filter.push(chunk));
    }
    out.push_str(&filter.finish());
    (out, filter.stripped)
}

#[test]
fn a_think_block_is_removed_from_the_text_channel() {
    let (out, stripped) = strip_reasoning(&["<think>let me work this out</think>The answer."]);
    assert_eq!(out, "The answer.");
    assert_eq!(stripped, 2);
}

#[test]
fn every_reasoning_tag_spelling_is_recognised() {
    for tag in ["think", "thinking", "reasoning"] {
        let (out, _) = strip_reasoning(&[&format!("<{tag}>hidden</{tag}>shown")]);
        assert_eq!(out, "shown", "<{tag}> should have been stripped");
    }
    // Casing is the model's whim, not a signal.
    let (out, _) = strip_reasoning(&["<Think>hidden</THINK>shown"]);
    assert_eq!(out, "shown");
}

/// A nested tag must not close the outer block early and leak the rest of
/// the reasoning.
#[test]
fn nested_blocks_close_in_order() {
    let (out, _) = strip_reasoning(&["<think>a<think>b</think>c</think>visible"]);
    assert_eq!(out, "visible");
}

/// Exactly what the failing session produced: a closing tag whose opener
/// never arrived. Only the tag goes.
#[test]
fn a_stray_closing_tag_is_removed_without_eating_the_text_around_it() {
    let (out, stripped) = strip_reasoning(&["first thought\n</think>\nthe actual answer"]);
    assert_eq!(out, "first thought\n\nthe actual answer");
    assert_eq!(stripped, 1);
    // And a later, properly-opened block still works — the stray close
    // must not have left the depth counter underwater.
    let (out, _) = strip_reasoning(&["a</think>b<think>c</think>d"]);
    assert_eq!(out, "abd");
}

/// The model opened a block and the stream ended inside it. That text is
/// reasoning by the model's own marking, so it is dropped rather than
/// handed to the user as a truncated thought.
#[test]
fn an_unclosed_block_swallows_the_rest_of_the_stream() {
    let (out, stripped) = strip_reasoning(&["visible <think>still musing about"]);
    assert_eq!(out, "visible ");
    assert_eq!(stripped, 1);
}

/// Deltas break wherever the transport happens to flush, so a tag can
/// arrive in pieces — including one character at a time.
#[test]
fn a_tag_split_across_deltas_is_still_recognised() {
    let (out, _) = strip_reasoning(&["Answer: <thi", "nk>hidden</thin", "k>forty-two"]);
    assert_eq!(out, "Answer: forty-two");

    let mut filter = ReasoningFilter::new();
    let mut out = String::new();
    for ch in "ok<think>no</think>yes".chars() {
        out.push_str(&filter.push(&ch.to_string()));
    }
    out.push_str(&filter.finish());
    assert_eq!(out, "okyes");
}

/// Text that only talks *about* the tags — as this codebase's own docs and
/// commit messages now do — must survive intact.
#[test]
fn prose_mentioning_the_tags_is_left_alone() {
    let prose = "Reasoning models emit `<think>` and `</think>` in the text channel.";
    let (out, stripped) = strip_reasoning(&[prose]);
    assert_eq!(out, prose);
    assert_eq!(stripped, 0);

    let fenced = "Example:\n\n```\n<think>\nmusing\n</think>\n```\n\nThat's the shape.";
    let (out, stripped) = strip_reasoning(&[fenced]);
    assert_eq!(out, fenced);
    assert_eq!(stripped, 0);
}

/// `<` is ordinary punctuation far more often than it is a reasoning tag.
#[test]
fn angle_brackets_that_are_not_reasoning_tags_are_untouched() {
    let text = "if 1 < 2 then <div> and <thinker> and </thoughts> stay put";
    let (out, stripped) = strip_reasoning(&[text]);
    assert_eq!(out, text);
    assert_eq!(stripped, 0);
}

/// Drives one turn to completion with permissions out of the way and hands
/// the agent back for assertions on history.
async fn run_one_turn(provider: Arc<ScriptedProvider>, tools: Arc<dyn ToolExecutor>) -> Agent {
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
        .with_permission_policy(PermissionPolicy::Skip);
    let (events_tx, _events_rx) = mpsc::unbounded_channel();
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
    agent
}

/// The point of doing this in `consume_stream`: reasoning is gone from the
/// message *and* therefore from history, so it is never re-sent.
#[tokio::test]
async fn a_think_block_never_reaches_history() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply(
        "<think>The user wants a number. 42 is fine.</think>The answer is 42.",
    )]));
    let agent = run_one_turn(provider, Arc::new(NoTools)).await;

    assert_eq!(agent.history().last().unwrap().text(), "The answer is 42.");
    for message in agent.history() {
        assert!(
            !message.text().contains("think"),
            "reasoning survived into history: {:?}",
            message.text()
        );
    }
    assert_eq!(agent.reasoning_tags_stripped(), 2);
}

/// The other half of the leak: the chat pane is painted from the deltas,
/// not from the final message, so a tag straddling two of them has to be
/// caught on the way *out* as well as on the way into history.
#[tokio::test]
async fn a_tag_split_across_deltas_never_reaches_the_event_stream() {
    let provider = Arc::new(ScriptedProvider::streams([vec![
        StreamEvent::TextDelta("The answer is <thi".into()),
        StreamEvent::TextDelta("nk>should I say 42?</thi".into()),
        StreamEvent::TextDelta("nk>42.".into()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]]));
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(
        provider,
        Arc::new(NoTools),
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

    let streamed: String = std::iter::from_fn(|| events_rx.try_recv().ok())
        .filter_map(|e| match e {
            AgentEvent::AssistantTextDelta(delta) => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, "The answer is 42.");
    assert_eq!(agent.history().last().unwrap().text(), "The answer is 42.");
}

/// The dangerous half of the leak: a model musing about a call must not
/// have that call executed.
#[tokio::test]
async fn an_envelope_inside_a_think_block_is_not_executed() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(ScriptedProvider::streams([text_reply(
        r#"<think>I could run {"action": "web_search", "query": "x"} here.</think>No need to search."#,
    )]));
    let tools = Arc::new(RecordingToolsNamed::new(
        defs(&["web_search"]),
        executed.clone(),
    ));
    let agent = run_one_turn(provider, tools.clone()).await;

    assert!(
        !executed.load(std::sync::atomic::Ordering::SeqCst),
        "a tool call the model was only thinking about must not run"
    );
    assert_eq!(agent.history().last().unwrap().text(), "No need to search.");
}

/// The verbatim failing session: a stray `</think>` between two copies of
/// an envelope naming `search` rather than `web_search`. Both defects at
/// once — the search must actually run.
#[tokio::test]
async fn the_observed_failure_now_runs_the_search() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let envelope = r#"{
  "action": "search",
  "query": "últimas notícias guerra na Ucrânia",
  "region": "pt-br",
  "max_results": 10
}"#;
    let provider = Arc::new(ScriptedProvider::streams([
        text_reply(&format!("{envelope}\n</think>\n{envelope}")),
        text_reply("Here are the headlines."),
    ]));
    let tools = Arc::new(RecordingToolsNamed::new(
        vec![
            def_with_properties(
                "web_search",
                serde_json::json!({"query": {}, "num_results": {}}),
            ),
            def_with_properties("run_bash", serde_json::json!({"command": {}})),
        ],
        executed.clone(),
    ));
    let agent = run_one_turn(provider, tools.clone()).await;

    let calls = tools.calls();
    assert_eq!(calls.len(), 1, "expected exactly one dispatch: {calls:?}");
    assert_eq!(calls[0].0, "web_search");
    assert_eq!(calls[0].1["query"], "últimas notícias guerra na Ucrânia");
    // Aliased onto the schema's own name...
    assert_eq!(calls[0].1["num_results"], 10);
    // ...while a key the schema knows nothing about is still handed over
    // for the tool to judge, not silently dropped here.
    assert_eq!(calls[0].1["region"], "pt-br");
    assert_eq!(
        agent.history().last().unwrap().text(),
        "Here are the headlines."
    );
}

/// Normalisation end to end, not just in the resolver.
#[tokio::test]
async fn a_differently_spelled_tool_name_is_recovered_and_executed() {
    for written in ["Web-Search", "WEB_SEARCH"] {
        let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(ScriptedProvider::streams([
            text_reply(&format!(r#"{{"action": "{written}", "query": "rust"}}"#)),
            text_reply("done"),
        ]));
        let tools = Arc::new(RecordingToolsNamed::new(
            defs(&["web_search"]),
            executed.clone(),
        ));
        run_one_turn(provider, tools.clone()).await;

        let calls = tools.calls();
        assert_eq!(calls.len(), 1, "{written} should have dispatched once");
        assert_eq!(calls[0].0, "web_search");
    }
}

/// The safety property, end to end: nothing runs, and the turn ends with
/// the JSON still sitting there as text rather than a guessed side effect.
#[tokio::test]
async fn an_action_naming_two_equally_plausible_tools_is_not_executed() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(ScriptedProvider::streams([text_reply(
        r#"{"action": "write", "path": "a.txt", "content": "hi"}"#,
    )]));
    let tools = Arc::new(RecordingToolsNamed::new(
        defs(&["write_file", "write_tasks"]),
        executed.clone(),
    ));
    let agent = run_one_turn(provider, tools.clone()).await;

    assert!(
        !executed.load(std::sync::atomic::Ordering::SeqCst),
        "an ambiguous name must not pick a tool"
    );
    assert!(agent.history().last().unwrap().text().contains("\"write\""));
}

#[test]
fn effective_system_is_none_without_system_or_goal() {
    assert!(fake_agent().effective_system().is_none());
}

#[test]
fn effective_system_uses_base_system_when_no_goal_set() {
    let agent = fake_agent().with_system("be concise");
    assert_eq!(agent.effective_system().as_deref(), Some("be concise"));
}

#[test]
fn effective_system_folds_goal_into_base_system() {
    let mut agent = fake_agent().with_system("be concise");
    agent.set_goal(Some("ship the login page".to_string()));
    let system = agent.effective_system().unwrap();
    assert!(system.contains("be concise"));
    assert!(system.contains("ship the login page"));
}

#[test]
fn effective_system_works_with_goal_but_no_base_system() {
    let mut agent = fake_agent();
    agent.set_goal(Some("ship the login page".to_string()));
    assert!(agent
        .effective_system()
        .unwrap()
        .contains("ship the login page"));
}

/// The goal segment points at the builtin `goal` skill — but only when a
/// `skill` tool is actually registered, so the prompt never names a tool
/// that does not exist (headless with a stripped registry, tests, forks
/// with the builtins compiled out).
#[test]
fn the_goal_names_the_goal_skill_only_when_the_skill_tool_exists() {
    // `fake_agent` runs on NoTools: no `skill` tool, no pointer.
    let mut agent = fake_agent();
    agent.set_goal(Some("ship the login page".to_string()));
    let system = agent.effective_system().unwrap();
    assert!(!system.contains("`goal` skill"), "{system}");

    struct SkillOnly;
    #[async_trait]
    impl ToolExecutor for SkillOnly {
        fn tool_defs(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        fn permission_class(&self, name: &str) -> Option<PermissionClass> {
            (name == "skill").then_some(PermissionClass::ReadOnly)
        }
        async fn execute(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            ToolResult::error("not under test")
        }
    }
    let mut agent = Agent::new(
        Arc::new(ScriptedProvider::streams([])),
        Arc::new(SkillOnly),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    );
    agent.set_goal(Some("ship the login page".to_string()));
    let system = agent.effective_system().unwrap();
    assert!(system.contains("`goal` skill"), "{system}");
    // The pointer belongs to the goal segment, behind the goal line itself.
    assert!(
        system.find("ship the login page").unwrap() < system.find("`goal` skill").unwrap(),
        "{system}"
    );
}

#[test]
fn clearing_goal_reverts_to_base_system() {
    let mut agent = fake_agent().with_system("be concise");
    agent.set_goal(Some("ship the login page".to_string()));
    agent.set_goal(None);
    assert_eq!(agent.effective_system().as_deref(), Some("be concise"));
}

#[test]
fn effective_system_appends_injected_context_after_the_base_prompt() {
    let mut agent = fake_agent()
        .with_system("be concise")
        .with_context_provider(|| "Current date: 2026-08-05".to_string());
    agent.set_goal(Some("ship the login page".to_string()));

    let system = agent.effective_system().unwrap();
    let base = system.find("be concise").unwrap();
    let date = system.find("Current date: 2026-08-05").unwrap();
    let goal = system.find("ship the login page").unwrap();
    // The static prompt must stay at the front so prefix-based prompt
    // caching isn't invalidated by the volatile segments behind it.
    assert!(base < date && date < goal, "unexpected order in: {system}");
}

#[test]
fn effective_system_recomputes_context_on_every_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let agent = fake_agent()
        .with_system("be concise")
        .with_context_provider(move || format!("call {}", counter.fetch_add(1, Ordering::SeqCst)));

    assert!(agent.effective_system().unwrap().contains("call 0"));
    assert!(agent.effective_system().unwrap().contains("call 1"));
}

#[test]
fn effective_system_skips_a_blank_context() {
    let agent = fake_agent()
        .with_system("be concise")
        .with_context_provider(|| "   ".to_string());
    assert_eq!(agent.effective_system().as_deref(), Some("be concise"));
}

#[test]
fn effective_system_works_with_context_but_no_base_system() {
    let agent = fake_agent().with_context_provider(|| "Current date: 2026-08-05".to_string());
    assert_eq!(
        agent.effective_system().as_deref(),
        Some("Current date: 2026-08-05")
    );
}

/// Asks for two `slow_tool` calls in one turn, then ends the turn.
fn two_tool_calls_then_done() -> ScriptedProvider {
    ScriptedProvider::streams([
        tool_calls_reply(&[
            ("call_1", "slow_tool", serde_json::json!({})),
            ("call_2", "slow_tool", serde_json::json!({})),
        ]),
        text_reply("done"),
    ])
}

/// Cancels the turn from inside the first tool call — the exact shape of
/// a user hitting Esc while a tool is running.
struct CancelOnFirstCallTools {
    cancel: CancellationToken,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl ToolExecutor for CancelOnFirstCallTools {
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
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.cancel.cancel();
        ToolResult::ok("first tool finished")
    }
}

/// Every `tool_use` block must be answered by a `tool_result`, even when
/// the turn is cancelled halfway through the round. Without this the next
/// request is rejected outright ("tool_use ids were found without
/// tool_result blocks") and the session is unusable — acceptance
/// criterion #1.
#[tokio::test]
async fn cancelling_mid_tool_round_still_answers_every_tool_use() {
    let cancel = CancellationToken::new();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools = Arc::new(CancelOnFirstCallTools {
        cancel: cancel.clone(),
        calls: calls.clone(),
    });
    let provider = Arc::new(two_tool_calls_then_done());
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
        .with_permission_policy(PermissionPolicy::Skip);

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    let completed = agent
        .run_turn("go".into(), events_tx, perm_tx, question_tx, cancel.clone())
        .await;

    assert!(!completed, "a cancelled turn is not a normal completion");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second tool must not run after cancellation"
    );

    let tool_use_ids = collect_ids(agent.history(), true);
    let tool_result_ids = collect_ids(agent.history(), false);
    assert_eq!(
        tool_use_ids, tool_result_ids,
        "every tool_use must have a matching tool_result"
    );
    assert_eq!(tool_use_ids.len(), 2);

    // The call that never ran must say so rather than look successful.
    let unanswered = agent
        .history()
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == "call_2" => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("call_2 must be answered");
    assert!(unanswered.1, "an unrun tool call is an error result");
    assert!(unanswered.0.contains("cancelled"), "got: {}", unanswered.0);
}

/// Reports progress mid-execution and records the call id it was handed,
/// standing in for a tool that streams output while it runs.
struct ProgressingTool {
    seen_call_id: std::sync::Mutex<Option<String>>,
}

#[async_trait]
impl ToolExecutor for ProgressingTool {
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
        ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        *self.seen_call_id.lock().unwrap() = ctx.tool_call_id().map(str::to_string);
        ctx.report_progress("line one");
        ctx.report_progress("line two");
        ToolResult::ok("finished")
    }
}

/// The channel a later task will use to stream `run_bash` output: a tool
/// must be able to emit lines *between* its start and result events, and
/// each line must carry the id of the call that produced it — otherwise a
/// frontend can't attach output to the right call when several tools run
/// in one round.
#[tokio::test]
async fn a_tool_can_report_progress_between_its_start_and_result_events() {
    let tools = Arc::new(ProgressingTool {
        seen_call_id: std::sync::Mutex::new(None),
    });
    let mut agent = Agent::new(
        Arc::new(two_tool_calls_then_done()),
        tools.clone(),
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_permission_policy(PermissionPolicy::Skip);

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    agent
        .run_turn(
            "go".into(),
            events_tx,
            perm_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    // The tool learned which call it was without being told explicitly.
    assert_eq!(
        tools.seen_call_id.lock().unwrap().as_deref(),
        Some("call_2"),
        "the context must carry the id of the call currently executing"
    );

    // Ordering is the whole reason progress rides the same channel.
    let sequence: Vec<String> = std::iter::from_fn(|| events_rx.try_recv().ok())
        .filter_map(|e| match e {
            AgentEvent::ToolCallStarted { id, .. } => Some(format!("start:{id}")),
            AgentEvent::ToolProgress { id, line } => Some(format!("progress:{id}:{line}")),
            AgentEvent::ToolCallResult { id, .. } => Some(format!("result:{id}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        sequence,
        vec![
            "start:call_1",
            "progress:call_1:line one",
            "progress:call_1:line two",
            "result:call_1",
            "start:call_2",
            "progress:call_2:line one",
            "progress:call_2:line two",
            "result:call_2",
        ]
    );
}

/// The agent's own context is session-long and must stay call-agnostic —
/// `/model` clones it into a rebuilt agent, so a stale call id or a
/// channel from a finished turn would outlive its call.
#[test]
fn the_agents_own_context_carries_no_call_id() {
    assert!(fake_agent().tool_ctx().tool_call_id().is_none());
}

/// A tool that echoes a secret back, standing in for `run_bash {"command":
/// "env"}` or `cat ~/.smith/config.toml`.
struct LeakySecretTool {
    secret: String,
}

#[async_trait]
impl ToolExecutor for LeakySecretTool {
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
        ToolResult::ok(format!("ANTHROPIC_API_KEY={}\nPATH=/usr/bin", self.secret))
    }
}

/// A leaked key must not survive into history: history is what gets
/// persisted to SQLite *and* what is sent to the provider on the next
/// request, so a secret landing there is handed to a third party.
#[tokio::test]
async fn a_secret_in_tool_output_never_reaches_history() {
    const SECRET: &str = "sk-ant-api03-supersecretvalue";

    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(
        Arc::new(two_tool_calls_then_done()),
        Arc::new(LeakySecretTool {
            secret: SECRET.to_string(),
        }),
        "fake-model".to_string(),
        tool_ctx,
    )
    .with_permission_policy(PermissionPolicy::Skip)
    .with_redactor(Redactor::new([SECRET.to_string()]));

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    agent
        .run_turn(
            "what's in the env?".into(),
            events_tx,
            perm_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    let history = format!("{:?}", agent.history());
    assert!(!history.contains(SECRET), "secret reached history");
    assert!(history.contains(crate::redact::REDACTED));
    // Everything else in the output has to survive, or redaction would be
    // destroying the tool result it's protecting.
    assert!(history.contains("PATH=/usr/bin"));

    // The transcript the user sees comes from these events, not history.
    let mut saw_result = false;
    while let Ok(event) = events_rx.try_recv() {
        if let AgentEvent::ToolCallResult { output, .. } = event {
            saw_result = true;
            assert!(!output.contains(SECRET), "secret reached the transcript");
        }
    }
    assert!(saw_result, "expected at least one tool result event");
}

/// A tool call cut off mid-stream was never dispatched and its arguments
/// may be truncated JSON — it must not reach history, or it becomes a
/// dangling `tool_use` that breaks every later request. The script is
/// spelled out rather than built from a helper: a half-emitted call with
/// truncated input and no `ToolUseComplete` is exactly what makes this
/// case interesting.
#[tokio::test]
async fn cancelling_mid_stream_drops_the_half_built_tool_call() {
    let provider = ScriptedProvider::streams([vec![
        StreamEvent::TextDelta("let me check ".to_string()),
        StreamEvent::ToolUseStart {
            id: "half_call".to_string(),
            name: "slow_tool".to_string(),
        },
        StreamEvent::ToolUseInputDelta {
            id: "half_call".to_string(),
            partial_json: "{\"pa".to_string(),
        },
        StreamEvent::MessageComplete {
            stop_reason: StopReason::Cancelled,
            usage: crate::message::Usage::default(),
        },
    ]]);
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(
        Arc::new(provider),
        Arc::new(NoTools),
        "fake-model".to_string(),
        tool_ctx,
    );

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    let completed = agent
        .run_turn(
            "go".into(),
            events_tx,
            perm_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(!completed, "cancelled is not a normal completion");
    assert!(
        collect_ids(agent.history(), true).is_empty(),
        "no dangling tool_use may survive a cancelled stream"
    );
    // The text the model did manage to produce is still worth keeping.
    assert!(agent
        .history()
        .iter()
        .any(|m| m.text().contains("let me check")));
}
