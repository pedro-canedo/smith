use super::*;
use smith_core::{ContentBlock, Message, PermissionRequest, Role, UserQuestion};
use tokio::sync::oneshot;

fn opts(format: OutputFormat, allowed: &[&str]) -> HeadlessOptions {
    HeadlessOptions {
        prompt: "do the thing".into(),
        format,
        allowed_tools: allowed.iter().map(|s| s.to_string()).collect(),
        color: false,
        provider: "scripted".into(),
        model: "test-model".into(),
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

struct Harness {
    action_rx: mpsc::UnboundedReceiver<Action>,
    out: Vec<u8>,
    err: Vec<u8>,
    code: u8,
}

impl Harness {
    fn out(&self) -> String {
        String::from_utf8(self.out.clone()).unwrap()
    }

    fn err(&self) -> String {
        String::from_utf8(self.err.clone()).unwrap()
    }
}

/// Runs the frontend against a canned event script, with the channels
/// wired exactly as the orchestrator wires them.
///
/// The script is handed the senders by value and they are dropped when it
/// returns, so a script that never sends a terminal event closes the
/// channels instead of hanging the test — which is also the one honest way
/// to simulate an orchestrator that died mid-turn.
async fn drive(
    options: &HeadlessOptions,
    script: impl FnOnce(
        mpsc::UnboundedSender<AgentEvent>,
        mpsc::UnboundedSender<PermissionAsk>,
        mpsc::UnboundedSender<QuestionAsk>,
    ),
) -> Harness {
    let (action_tx, action_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (permission_tx, permission_rx) = mpsc::unbounded_channel();
    let (question_tx, question_rx) = mpsc::unbounded_channel();

    script(event_tx, permission_tx, question_tx);

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(
        options,
        action_tx,
        event_rx,
        permission_rx,
        question_rx,
        &mut out,
        &mut err,
    )
    .await;

    Harness {
        action_rx,
        out,
        err,
        code,
    }
}

fn started() -> AgentEvent {
    AgentEvent::PhaseChanged(smith_core::AgentPhase::Thinking)
}

fn done(text: &str) -> AgentEvent {
    AgentEvent::AssistantTurnComplete {
        message: assistant(text),
        stop_reason: StopReason::EndTurn,
    }
}

/// A permission ask the way the agent sends one.
fn permission_ask(tool_name: &str) -> (PermissionAsk, oneshot::Receiver<PermissionDecision>) {
    let (tx, rx) = oneshot::channel();
    (
        PermissionAsk {
            request: PermissionRequest {
                tool_call_id: "call_1".into(),
                tool_name: tool_name.into(),
                detail: format!("{tool_name} would do something"),
            },
            respond_to: tx,
        },
        rx,
    )
}

#[tokio::test]
async fn a_successful_turn_prints_prose_on_stdout_and_exits_zero() {
    let options = opts(OutputFormat::Text, &[]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events
            .send(AgentEvent::AssistantTextDelta("all ".into()))
            .unwrap();
        events
            .send(AgentEvent::AssistantTextDelta("done".into()))
            .unwrap();
        events.send(done("all done")).unwrap();
    })
    .await;

    assert_eq!(h.code, EXIT_OK);
    assert_eq!(h.out(), "all done\n");
}

/// A model that ends on a newline already gets no second one — a stray
/// blank line is as much a bug as a missing one.
#[tokio::test]
async fn a_reply_that_already_ends_in_a_newline_is_not_given_another() {
    let options = opts(OutputFormat::Text, &[]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events
            .send(AgentEvent::AssistantTextDelta("done\n".into()))
            .unwrap();
        events.send(done("done\n")).unwrap();
    })
    .await;

    assert_eq!(h.out(), "done\n");
}

#[tokio::test]
async fn the_prompt_is_submitted_as_the_turns_only_action() {
    let options = opts(OutputFormat::Text, &[]);
    let mut h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events.send(done("ok")).unwrap();
    })
    .await;

    let action = h.action_rx.try_recv().unwrap();
    assert!(matches!(action, Action::SubmitMessage(text) if text == "do the thing"));
    assert!(h.action_rx.try_recv().is_err());
}

#[tokio::test]
async fn a_failed_turn_exits_non_zero_and_keeps_stdout_clean() {
    let options = opts(OutputFormat::Text, &[]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events
            .send(AgentEvent::Error("provider exploded".into()))
            .unwrap();
    })
    .await;

    assert_eq!(h.code, EXIT_TURN_FAILED);
    assert!(h.out.is_empty());
    assert!(h.err().contains("provider exploded"));
}

/// A broken MCP server reports itself on the same channel as a turn
/// failure, before the turn starts. Treating it as fatal would make one
/// misconfigured server abort every headless run in the project.
#[tokio::test]
async fn an_error_before_the_turn_starts_is_reported_but_not_fatal() {
    let options = opts(OutputFormat::Text, &[]);
    let h = drive(&options, |events, _, _| {
        events
            .send(AgentEvent::Error(
                "mcp server 'x': failed to connect".into(),
            ))
            .unwrap();
        events.send(started()).unwrap();
        events.send(done("carried on")).unwrap();
    })
    .await;

    assert_eq!(h.code, EXIT_OK);
    assert!(h.err().contains("failed to connect"));
}

#[tokio::test]
async fn a_capped_turn_exits_with_the_limit_code_not_the_failure_one() {
    let options = opts(OutputFormat::Text, &[]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events
            .send(AgentEvent::TurnLimitReached {
                kind: smith_core::TurnLimitKind::Rounds,
                detail: "reached the limit of 2 tool-call rounds in one turn".into(),
            })
            .unwrap();
    })
    .await;

    assert_eq!(h.code, EXIT_LIMIT);
    assert!(h.err().contains("2 tool-call rounds"));
}

#[tokio::test]
async fn a_cancelled_turn_is_a_failure_even_though_it_arrives_as_a_completion() {
    let options = opts(OutputFormat::Text, &[]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events
            .send(AgentEvent::AssistantTurnComplete {
                message: assistant("partial"),
                stop_reason: StopReason::Cancelled,
            })
            .unwrap();
    })
    .await;

    assert_eq!(h.code, EXIT_TURN_FAILED);
}

/// If the orchestrator dies the frontend must not wait forever for a
/// completion event that can no longer be sent.
#[tokio::test]
async fn a_closed_event_channel_ends_the_run_as_a_failure() {
    let options = opts(OutputFormat::Text, &[]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
    })
    .await;

    assert_eq!(h.code, EXIT_TURN_FAILED);
    assert!(h.err().contains("without finishing"));
}

#[tokio::test]
async fn json_output_is_a_single_parseable_object() {
    let options = opts(OutputFormat::Json, &[]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events
            .send(AgentEvent::AssistantTextDelta("ignored in json".into()))
            .unwrap();
        events
            .send(AgentEvent::TokenUsage(Usage {
                input_tokens: 10,
                output_tokens: 4,
                ..Usage::default()
            }))
            .unwrap();
        events
            .send(AgentEvent::TokenUsage(Usage {
                input_tokens: 1,
                output_tokens: 2,
                ..Usage::default()
            }))
            .unwrap();
        events.send(done("the answer")).unwrap();
    })
    .await;

    assert_eq!(h.code, EXIT_OK);
    let value: serde_json::Value = serde_json::from_slice(&h.out).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["exit_code"], 0);
    assert_eq!(value["result"], "the answer");
    assert_eq!(value["num_turns"], 1);
    assert_eq!(value["usage"]["input_tokens"], 11);
    assert_eq!(value["usage"]["output_tokens"], 6);
    assert_eq!(value["error"], serde_json::Value::Null);
}

#[tokio::test]
async fn json_output_reports_the_failure_and_the_exit_code_it_caused() {
    let options = opts(OutputFormat::Json, &[]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events.send(AgentEvent::Error("boom".into())).unwrap();
    })
    .await;

    assert_eq!(h.code, EXIT_TURN_FAILED);
    let value: serde_json::Value = serde_json::from_slice(&h.out).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["exit_code"], 1);
    assert_eq!(value["error"], "boom");
}

/// The property that matters for JSONL: every line parses on its own.
/// A pretty-printed object would satisfy "valid JSON" and still break
/// every consumer.
#[tokio::test]
async fn stream_json_emits_exactly_one_complete_json_value_per_line() {
    let options = opts(OutputFormat::StreamJson, &["write_file"]);
    let h = drive(&options, |events, permissions, _| {
        events.send(started()).unwrap();
        events
            .send(AgentEvent::AssistantTextDelta("multi\nline\ntext".into()))
            .unwrap();
        let (ask, rx) = permission_ask("run_bash");
        permissions.send(ask).unwrap();
        tokio::spawn(async move {
            let _ = rx.await;
            let _ = events.send(AgentEvent::ToolCallStarted {
                id: "call_1".into(),
                tool_name: "write_file".into(),
                input: serde_json::json!({ "path": "a.txt", "body": "x\ny" }),
            });
            let _ = events.send(done("done"));
        });
    })
    .await;

    let text = h.out();
    assert!(text.ends_with('\n'));
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= 5, "expected several lines, got {lines:?}");
    for line in &lines {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line is not standalone JSON: {line:?}: {e}"));
        assert!(value.get("type").is_some(), "no type tag on {line:?}");
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(lines.last().unwrap()).unwrap()["type"],
        "result"
    );
    // The denial line has to be synthesized — the agent emits no tool
    // events at all for a refused call.
    assert!(text.contains("permission_decision"));
}

#[tokio::test]
async fn a_tool_in_allowed_tools_is_allowed_for_the_rest_of_the_session() {
    let options = opts(OutputFormat::Text, &["write_file"]);
    let (decided_tx, decided_rx) = oneshot::channel();
    let h = drive(&options, |events, permissions, _| {
        events.send(started()).unwrap();
        let (ask, rx) = permission_ask("write_file");
        permissions.send(ask).unwrap();
        // The turn can only finish *after* the answer — that is the shape
        // the agent imposes, since it is parked on the oneshot until then.
        tokio::spawn(async move {
            let _ = decided_tx.send(rx.await.unwrap());
            let _ = events.send(done("wrote it"));
        });
    })
    .await;

    // `AllowSession`, not `AllowOnce`: the answer comes from a flag, so
    // asking the same question twice could only ever get the same reply.
    assert_eq!(decided_rx.await.unwrap(), PermissionDecision::AllowSession);
    assert_eq!(h.code, EXIT_OK);
    assert!(!h.err().contains("denied"));
}

#[tokio::test]
async fn the_denial_reason_names_the_flag_that_would_have_allowed_it() {
    let options = opts(OutputFormat::Text, &[]);
    let h = drive(&options, |events, permissions, _| {
        events.send(started()).unwrap();
        let (ask, rx) = permission_ask("edit_file");
        permissions.send(ask).unwrap();
        tokio::spawn(async move {
            let _ = rx.await;
            let _ = events.send(done("blocked"));
        });
    })
    .await;

    let err = h.err();
    assert!(err.contains("denied edit_file"), "{err}");
    assert!(err.contains("--allowed-tools"), "{err}");
}

/// Deny-by-default, and loudly. A run that silently did nothing because
/// every write was refused is indistinguishable from a model that chose
/// to do nothing, so the reason has to be on stderr and in the JSON.
#[tokio::test]
async fn a_tool_outside_allowed_tools_is_denied_and_said_so() {
    let options = opts(OutputFormat::Json, &["read_file"]);
    let (decided_tx, decided_rx) = oneshot::channel();
    let h = drive(&options, |events, permissions, _| {
        events.send(started()).unwrap();
        let (ask, rx) = permission_ask("run_bash");
        permissions.send(ask).unwrap();
        tokio::spawn(async move {
            let _ = decided_tx.send(rx.await.unwrap());
            let _ = events.send(done("could not"));
        });
    })
    .await;

    assert_eq!(decided_rx.await.unwrap(), PermissionDecision::Deny);
    assert!(h.err().contains("denied run_bash"), "{}", h.err());
    let value: serde_json::Value = serde_json::from_slice(&h.out).unwrap();
    assert_eq!(value["denied_tools"][0], "run_bash");
    // Denial is not itself a run failure: the model gets the refusal as a
    // tool error and may still finish the turn sensibly.
    assert_eq!(h.code, EXIT_OK);
}

#[tokio::test]
async fn ask_user_is_answered_with_an_instruction_to_proceed_not_a_guess() {
    let options = opts(OutputFormat::Text, &[]);
    let (answered_tx, answered_rx) = oneshot::channel();
    let h = drive(&options, |events, _, questions| {
        events.send(started()).unwrap();
        let (tx, rx) = oneshot::channel();
        questions
            .send(QuestionAsk {
                question: UserQuestion {
                    id: "q1".into(),
                    prompt: "Which database?".into(),
                    options: ["postgres".into(), "sqlite".into(), "mysql".into()],
                },
                respond_to: tx,
            })
            .unwrap();
        tokio::spawn(async move {
            let _ = answered_tx.send(rx.await.unwrap());
            let _ = events.send(done("picked one"));
        });
    })
    .await;

    // A refusal, not an answer: headless has no user, and inventing one
    // is worse than telling the model plainly that nobody is there.
    let reason = answered_rx
        .await
        .unwrap()
        .expect_err("headless must refuse the question, not answer it");
    assert!(reason.contains("headless"));
    for option in ["postgres", "sqlite", "mysql"] {
        assert!(
            !reason.contains(option),
            "refusal leaked a suggestion: {reason}"
        );
    }
    assert_eq!(h.code, EXIT_OK);
}

#[tokio::test]
async fn tool_calls_are_logged_to_stderr_in_text_mode_leaving_stdout_pure() {
    let options = opts(OutputFormat::Text, &["write_file"]);
    let h = drive(&options, |events, _, _| {
        events.send(started()).unwrap();
        events
            .send(AgentEvent::ToolCallStarted {
                id: "call_1".into(),
                tool_name: "write_file".into(),
                input: serde_json::json!({ "path": "a.txt" }),
            })
            .unwrap();
        events
            .send(AgentEvent::ToolCallResult {
                id: "call_1".into(),
                output: "wrote 3 bytes".into(),
                is_error: false,
            })
            .unwrap();
        events
            .send(AgentEvent::AssistantTextDelta("finished".into()))
            .unwrap();
        events.send(done("finished")).unwrap();
    })
    .await;

    assert_eq!(h.out(), "finished\n");
    let err = h.err();
    assert!(err.contains("write_file"));
    assert!(err.contains("path=a.txt"));
}

#[tokio::test]
async fn color_is_off_when_asked_and_on_otherwise() {
    let plain = Paint(false);
    assert_eq!(plain.red("x"), "x");
    assert_eq!(plain.dim("x"), "x");
    let painted = Paint(true);
    assert!(painted.red("x").starts_with("\x1b["));
    assert!(painted.dim("x").ends_with(RESET));
}

#[test]
fn a_prompt_and_stdin_combine_with_the_instruction_first() {
    let composed = compose_prompt(Some("diagnose this"), Some("panic at line 4")).unwrap();
    assert!(composed.starts_with("diagnose this"));
    assert!(composed.contains("<stdin>\npanic at line 4\n</stdin>"));
}

#[test]
fn stdin_alone_is_the_prompt() {
    assert_eq!(
        compose_prompt(None, Some("  explain this repo  ")).unwrap(),
        "explain this repo"
    );
}

#[test]
fn a_prompt_alone_needs_no_delimiters() {
    assert_eq!(compose_prompt(Some("hi"), None).unwrap(), "hi");
    // An empty pipe is the same as no pipe — `smith -p x < /dev/null`
    // must not hand the model a blank <stdin> block to reason about.
    assert_eq!(compose_prompt(Some("hi"), Some("   \n ")).unwrap(), "hi");
}

#[test]
fn no_prompt_at_all_is_a_usage_error() {
    assert!(compose_prompt(None, None).is_err());
    assert!(compose_prompt(Some("  "), Some("")).is_err());
}

#[test]
fn summaries_stay_on_one_line_and_never_split_a_codepoint() {
    let summary = summarize(&serde_json::json!({ "command": "echo a\nb" }));
    assert_eq!(summary, "command=echo a b");
    let long = summarize(&serde_json::json!({ "body": "é".repeat(400) }));
    assert_eq!(long.chars().count(), SUMMARY_LIMIT);
    assert!(long.ends_with('…'));
}
