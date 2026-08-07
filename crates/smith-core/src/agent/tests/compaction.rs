//! Compacting history when it outgrows the window.

use super::*;

// ---- compaction --------------------------------------------------------

const OPEN_TODOS: &[&str] = &[
    "wire up the migration path",
    "persist the computed cost",
    "emit the context gauge",
    "write the compaction tests",
];

const DONE_TODO: &str = "read the existing session store";

fn write_tasks_call() -> Message {
    let mut tasks = vec![serde_json::json!({
        "content": DONE_TODO,
        "status": "completed",
    })];
    for (i, todo) in OPEN_TODOS.iter().enumerate() {
        tasks.push(serde_json::json!({
            "content": todo,
            "status": if i == 0 { "in_progress" } else { "pending" },
        }));
    }
    Message::assistant(vec![ContentBlock::ToolUse {
        id: "tasks_1".into(),
        name: "write_tasks".into(),
        input: serde_json::json!({ "tasks": tasks }),
    }])
}

/// A long, realistic session: the checklist is established in the third
/// message — deep inside the part compaction throws away — and the rest is
/// alternating work with tool calls in it.
fn long_history(total: usize) -> Vec<Message> {
    let mut messages = vec![
        Message::user_text("build the context accounting chain"),
        write_tasks_call(),
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tasks_1".into(),
                content: "tasks updated".into(),
                is_error: false,
            }],
        },
    ];
    let mut round = 0;
    while messages.len() < total {
        round += 1;
        messages.push(Message::user_text(format!("step {round}, please continue")));
        messages.push(Message::assistant(vec![ContentBlock::ToolUse {
            id: format!("call_{round}"),
            name: "read_file".into(),
            input: serde_json::json!({ "path": format!("src/file_{round}.rs") }),
        }]));
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: format!("call_{round}"),
                content: format!("contents of file {round}"),
                is_error: false,
            }],
        });
        messages.push(Message::assistant(vec![ContentBlock::Text {
            text: format!("read file {round}"),
        }]));
    }
    messages.truncate(total);
    messages
}

fn history_text(history: &[Message]) -> String {
    history
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n")
}

fn fingerprint(history: &[Message]) -> String {
    serde_json::to_string(history).unwrap()
}

/// The acceptance criterion, end to end: 200 messages, todos established
/// in the part that gets thrown away, and every open one still present
/// afterwards — because they are re-injected structurally, not because the
/// summary happened to mention them (the scripted summary deliberately
/// mentions none of them).
#[tokio::test]
async fn two_hundred_messages_compact_with_every_pending_todo_intact() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply(
        "The session refactored the session store and added a migration path.",
    )]));
    let mut agent = agent_for(provider.clone(), Arc::new(NoTools));
    agent.set_goal(Some("ship context accounting".into()));
    agent.seed_history(long_history(200));
    assert_eq!(agent.history().len(), 200);

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let outcome = agent
        .compact(&events_tx, &CancellationToken::new())
        .await
        .expect("compaction should succeed");

    assert_eq!(outcome.messages_before, 200);
    assert!(outcome.messages_after <= 12, "{outcome:?}");
    assert!(outcome.tokens_after < outcome.tokens_before, "{outcome:?}");
    // Exactly one provider request: the summary. Compaction is not an
    // excuse to start a conversation.
    assert_eq!(provider.request_count(), 1);
    // ...and it was asked with no tools, so it cannot go do work of its own.
    assert!(provider.last_request().unwrap().tools.is_empty());

    let text = history_text(agent.history());
    for todo in OPEN_TODOS {
        assert!(text.contains(todo), "compaction lost the todo {todo:?}");
    }
    assert!(
        !text.contains(DONE_TODO),
        "a completed todo was re-injected as open"
    );
    assert!(text.contains("ship context accounting"), "{text}");
    assert!(text.contains("refactored the session store"), "{text}");
    assert!(text.contains("src/file_1.rs"), "files touched were lost");

    // The invariant that makes the *next* request legal at all.
    assert_eq!(
        collect_ids(agent.history(), true),
        collect_ids(agent.history(), false)
    );
    // Roles still alternate across the seam.
    assert_eq!(agent.history()[0].role, Role::User);
    assert_eq!(agent.history()[1].role, Role::Assistant);
    assert_eq!(agent.history()[2].role, Role::User);
}

/// The criterion end to end through `run_turn`, not through `compact`
/// directly: a 200-message session crosses the threshold, compacts itself
/// mid-turn, and answers — with every open todo still in the history the
/// model was actually sent.
#[tokio::test]
async fn a_long_turn_auto_compacts_and_still_answers() {
    let provider = Arc::new(
        ScriptedProvider::streams([
            text_reply("Earlier work established the store and its migrations."),
            text_reply_with_usage("here is the answer", prompt_usage(300)),
        ])
        .with_capabilities(window_of(1_000)),
    );
    let mut agent = agent_for(provider.clone(), Arc::new(NoTools));
    agent.seed_history(long_history(200));
    assert!(
        agent.should_compact(),
        "200 messages must not fit a 1000-token window"
    );

    let (completed, events) =
        run_collect(&mut agent, "so what now?", CancellationToken::new()).await;

    assert!(completed);
    // Two requests: the summary, then the turn itself.
    assert_eq!(provider.request_count(), 2);
    assert!(agent.history().len() < 20, "{}", agent.history().len());

    // What the model was *sent* on the real request is what matters.
    let sent = provider.requests()[1]
        .messages
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    for todo in OPEN_TODOS {
        assert!(
            sent.contains(todo),
            "auto-compaction lost the todo {todo:?}"
        );
    }
    assert!(sent.contains("so what now?"), "the user's message was lost");

    // The gauge came back down, and the frontend was told.
    assert!(!agent.should_compact());
    assert!(!context_events(&events).is_empty());
}

/// The second compaction is the dangerous one. By then the original
/// `write_tasks` call is gone — the first compaction replaced it with
/// prose — so anything reconstructing todos from history finds nothing.
/// The todos survive because the first compaction promoted them to the
/// agent's live checklist.
#[tokio::test]
async fn a_second_compaction_still_carries_the_todos() {
    let provider = Arc::new(ScriptedProvider::streams([
        text_reply("first summary"),
        text_reply("second summary"),
    ]));
    let mut agent = agent_for(provider, Arc::new(NoTools));
    agent.seed_history(long_history(200));

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    agent.compact(&events_tx, &cancel).await.unwrap();

    // There is no `write_tasks` call left anywhere to reconstruct from.
    assert!(
        !agent
            .history()
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, ContentBlock::ToolUse { name, .. } if name == "write_tasks")),
        "the fixture no longer tests what it claims to"
    );
    assert_eq!(agent.tasks().len(), OPEN_TODOS.len());

    agent.compact(&events_tx, &cancel).await.unwrap();

    let text = history_text(agent.history());
    for todo in OPEN_TODOS {
        assert!(text.contains(todo), "the second compaction lost {todo:?}");
    }
}

/// The failure path. A summarising request that dies must leave the
/// conversation byte-for-byte as it was — the alternative is destroying
/// history at precisely the moment the provider is unreliable.
#[tokio::test]
async fn a_failed_summarisation_leaves_history_intact() {
    // 401 rather than a 5xx: non-retryable, so the failure is the test's
    // subject rather than the backoff schedule.
    let provider = Arc::new(ScriptedProvider::new([ScriptedResponse::Fail(api_error(
        401, None,
    ))]));
    let mut agent = agent_for(provider.clone(), Arc::new(NoTools));
    let before = long_history(200);
    agent.seed_history(before.clone());

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    let error = agent
        .compact(&events_tx, &CancellationToken::new())
        .await
        .expect_err("the summarising request failed, so compaction must fail");

    assert!(error.contains("401"), "{error}");
    assert_eq!(agent.history().len(), before.len());
    assert_eq!(fingerprint(agent.history()), fingerprint(&before));
    assert_eq!(provider.request_count(), 1);
}

/// A summary that comes back empty is a failure too — replacing 200
/// messages with nothing is worse than not compacting.
#[tokio::test]
async fn an_empty_summary_is_treated_as_a_failure() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply("   ")]));
    let mut agent = agent_for(provider, Arc::new(NoTools));
    let before = long_history(60);
    agent.seed_history(before.clone());

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    assert!(agent
        .compact(&events_tx, &CancellationToken::new())
        .await
        .is_err());
    assert_eq!(fingerprint(agent.history()), fingerprint(&before));
}

/// A conversation too short to have a safe split point must fail cleanly
/// without spending a request on a summary of nothing.
#[tokio::test]
async fn a_short_history_is_not_compacted_and_costs_no_request() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply("unused")]));
    let mut agent = agent_for(provider.clone(), Arc::new(NoTools));
    agent.seed_history(vec![Message::user_text("hi")]);

    let (events_tx, _events_rx) = mpsc::unbounded_channel();
    assert!(agent
        .compact(&events_tx, &CancellationToken::new())
        .await
        .is_err());
    assert_eq!(provider.request_count(), 0);
}

/// The summary must never surface as something the assistant said — it is
/// housekeeping, not conversation. Its token cost, however, is the user's
/// and does surface.
#[tokio::test]
async fn the_summary_text_never_reaches_the_frontend_but_its_cost_does() {
    let provider = Arc::new(ScriptedProvider::streams([text_reply_with_usage(
        "a summary nobody should see in the chat pane",
        Usage {
            input_tokens: 900,
            output_tokens: 100,
            ..Usage::default()
        },
    )]));
    let mut agent = agent_for(provider, Arc::new(NoTools));
    agent.seed_history(long_history(60));

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    agent
        .compact(&events_tx, &CancellationToken::new())
        .await
        .unwrap();
    let events: Vec<AgentEvent> = std::iter::from_fn(|| events_rx.try_recv().ok()).collect();

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::AssistantTextDelta(_))),
        "the summary leaked into the transcript"
    );
    let reported: u32 = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TokenUsage(u) => Some(u.total_tokens()),
            _ => None,
        })
        .sum();
    assert_eq!(reported, 1_000);
    assert_eq!(agent.session_usage().output_tokens, 100);
}
