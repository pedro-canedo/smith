//! `on_agent_event` turning each `AgentEvent` into UI state.

use super::*;

#[test]
fn model_changed_event_updates_labels_and_clears_stale_resources() {
    let mut app = test_app();
    app.resources = Some(ResourceStats::default());
    app.on_agent_event(AgentEvent::ModelChanged {
        provider: "ollama".to_string(),
        model: "qwen2.5".to_string(),
        saved: true,
    });
    assert_eq!(app.provider_label, "ollama");
    assert_eq!(app.model_label, "qwen2.5");
    assert!(app.resources.is_none());
    assert!(app
        .lines
        .iter()
        .any(|l| l.text.contains("switched to ollama/qwen2.5")
            && l.text.contains("saved as default")));
}

#[test]
fn token_usage_sets_tokens_per_sec_and_meta_includes_it() {
    let mut app = test_app();
    app.waiting_on_assistant = true;
    app.turn_started_at = Some(Instant::now() - std::time::Duration::from_secs(2));
    app.stream_started_at = Some(Instant::now() - std::time::Duration::from_secs(2));

    app.on_agent_event(AgentEvent::AssistantTextDelta("hello ".into()));
    assert!(app.live_tokens_per_sec.is_some());
    assert!(app.display_tokens_per_sec().is_some());

    app.on_agent_event(AgentEvent::TokenUsage(Usage {
        input_tokens: 10,
        output_tokens: 100,
        ..Usage::default()
    }));
    let rate = app.tokens_per_sec.expect("measured rate");
    assert!(rate > 0.0);

    app.on_agent_event(AgentEvent::AssistantTurnComplete {
        message: smith_core::Message::assistant(vec![smith_core::ContentBlock::Text {
            text: "hello world".into(),
        }]),
        stop_reason: StopReason::EndTurn,
    });
    assert!(app.live_tokens_per_sec.is_none());
    assert_eq!(app.display_tokens_per_sec(), Some(rate));
    let meta = app
        .lines
        .last()
        .and_then(|l| l.meta.as_deref())
        .unwrap_or("");
    assert!(meta.contains("tok/s"), "meta was: {meta}");
}

#[test]
fn permission_policy_changed_event_updates_state() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::PermissionPolicyChanged {
        policy: PermissionPolicy::Skip,
        saved: false,
    });
    assert_eq!(app.permission_policy, PermissionPolicy::Skip);
    assert!(app
        .lines
        .iter()
        .any(|l| l.text.contains("permission mode: skip")));
}

#[test]
fn the_mcp_status_event_opens_a_table_with_one_row_per_server() {
    use smith_core::{McpHealth, McpServerStatus, McpStatus};
    let mut app = test_app();
    app.on_agent_event(AgentEvent::McpStatus(McpStatus {
        servers: vec![
            McpServerStatus {
                name: "docs".into(),
                transport: "sse".into(),
                health: McpHealth::Connected,
                tools: 4,
                resources: 1,
                prompts: 0,
                detail: None,
            },
            McpServerStatus {
                name: "ghost".into(),
                transport: "-".into(),
                health: McpHealth::Failed,
                tools: 0,
                resources: 0,
                prompts: 0,
                detail: Some("not on PATH".into()),
            },
        ],
    }));
    // The transcript stays clean — the report is a panel, not history.
    assert!(app.lines.is_empty(), "{:?}", app.lines);
    let overlay = app.overlay.as_ref().expect("an overlay should be open");
    let OverlayBody::Table { rows, columns, .. } = &overlay.body else {
        panic!("expected a table, got {:?}", overlay.body);
    };
    assert_eq!(columns.len(), 5);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], "docs");
    assert_eq!(rows[0][2], "connected");
    assert_eq!(rows[0][3], "4/1/0");
    assert_eq!(rows[1][4], "not on PATH");
}

/// With nothing configured, a table of zero rows says less than the hint
/// does — so that case stays a transcript line.
#[test]
fn no_mcp_servers_stays_a_transcript_line_rather_than_an_empty_table() {
    use smith_core::McpStatus;
    let mut app = test_app();
    app.on_agent_event(AgentEvent::McpStatus(McpStatus {
        servers: Vec::new(),
    }));
    assert!(app.overlay.is_none());
    assert_eq!(app.lines.len(), 1);
    assert!(app.lines[0].text.contains("no MCP servers configured"));
}

/// The cost shown is the one the agent reported, **not** one recomputed
/// from `usage` and a local price table. That is the whole reason the TUI
/// no longer carries a price table: a resumed session's cost is a sum of
/// per-turn figures priced when those turns ran, and multiplying today's
/// price by the lifetime token count would silently disagree with it.
#[test]
fn usage_shows_the_cost_the_agent_reported_not_a_local_recomputation() {
    let mut app = test_app();
    // Tokens that any price table would turn into a large number…
    app.usage.input_tokens = 1_000_000;
    app.usage.output_tokens = 1_000_000;
    // …while the authoritative figure, from the `turns` table, is small.
    app.on_agent_event(AgentEvent::SessionCost {
        usd: 0.25,
        unpriced_turns: 0,
    });
    app.run_slash_command("usage");
    assert_eq!(usage_cell(&app, "cost (est.)"), "~$0.2500");
}

/// "$0.00" and "we have no price for that model" are different claims.
#[test]
fn unpriced_turns_are_reported_beside_the_total_not_folded_into_it() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::SessionCost {
        usd: 4.20,
        unpriced_turns: 3,
    });
    app.run_slash_command("usage");
    assert_eq!(usage_cell(&app, "cost (est.)"), "~$4.2000");
    let footer = &app.overlay.as_ref().unwrap().footer;
    assert!(footer.iter().any(|f| f.contains("3 turn(s)")), "{footer:?}");
}

/// The regression the roadmap called out: a permission prompt used to
/// redraw the whole frame ~8 times a second while the *user* was reading
/// it, with a spinner claiming work was happening.
#[test]
fn waiting_on_the_user_is_not_animation() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::PermissionPromptNeeded(
        smith_core::PermissionRequest {
            tool_call_id: "1".into(),
            tool_name: "run_bash".into(),
            detail: "ls".into(),
        },
    ));
    assert!(app.modal.is_some(), "the prompt is up");
    assert!(
        !app.is_animating(),
        "nothing is moving while the agent waits for a human"
    );

    app.on_agent_event(AgentEvent::UserQuestionNeeded(smith_core::UserQuestion {
        id: "q".into(),
        prompt: "which one?".into(),
        options: ["a".into(), "b".into(), "c".into()],
    }));
    assert!(!app.is_animating(), "same for a question");
}

/// …but a read-only tool running in parallel behind the prompt still is.
#[test]
fn a_tool_still_running_behind_a_prompt_keeps_the_frame_alive() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "read_file".into(),
        input: serde_json::json!({"path": "src/main.rs"}),
    });
    app.on_agent_event(AgentEvent::PermissionPromptNeeded(
        smith_core::PermissionRequest {
            tool_call_id: "2".into(),
            tool_name: "run_bash".into(),
            detail: "ls".into(),
        },
    ));
    assert!(
        app.is_animating(),
        "the running card's throbber has to keep ticking"
    );
}

/// A card that carries content of its own is never folded — a `read_file`
/// card can hold a diff or an error tail, and hiding those is the opposite
/// of the point.
#[test]
fn only_status_only_tools_are_grouped() {
    let mut app = test_app();
    for (i, path) in ["a.rs", "b.rs", "c.rs"].iter().enumerate() {
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: format!("r{i}"),
            tool_name: "read_file".into(),
            input: serde_json::json!({ "path": path }),
        });
    }
    let cards = app
        .lines
        .iter()
        .filter(|l| l.role == ChatRole::Tool)
        .count();
    assert_eq!(cards, 3, "reads must keep their own cards");
}

/// A permission prompt is the agent blocked on the user; answering it by
/// starting a different turn is not what the queue is for.
#[test]
fn the_queue_waits_for_a_modal_to_be_answered() {
    let mut app = test_app();
    submit(&mut app, "first");
    submit(&mut app, "second");
    app.waiting_on_assistant = false;
    app.on_agent_event(AgentEvent::PermissionPromptNeeded(
        smith_core::PermissionRequest {
            tool_call_id: "1".into(),
            tool_name: "run_bash".into(),
            detail: "rm -rf /".into(),
        },
    ));
    assert!(app.take_queued_prompt().is_none());
}

#[test]
fn plan_turn_complete_opens_confirm_modal() {
    let mut app = test_app();
    app.plan_turn_active = true;
    app.plan_gated = true;
    app.waiting_on_assistant = true;
    app.on_agent_event(AgentEvent::AssistantTurnComplete {
        message: smith_core::Message::assistant(vec![smith_core::ContentBlock::Text {
            text: "1. Do the thing\n2. Ship it".into(),
        }]),
        stop_reason: StopReason::EndTurn,
    });
    let modal = app.modal.plan().expect("plan modal");
    assert!(modal.text.contains("Do the thing"));
    assert!(!app.plan_turn_active);
    assert!(!app.waiting_on_assistant);
}

#[test]
fn empty_turn_reports_no_output_instead_of_going_silent() {
    let mut app = test_app();
    app.waiting_on_assistant = true;
    app.on_agent_event(AgentEvent::AssistantTurnComplete {
        message: smith_core::Message::assistant(vec![]),
        stop_reason: StopReason::EndTurn,
    });
    assert!(app
        .lines
        .iter()
        .any(|l| l.text.contains("no output for this turn")));
    assert!(!app.waiting_on_assistant);
}

#[test]
fn empty_turn_while_plan_gated_hints_at_plan_reject() {
    let mut app = test_app();
    app.plan_turn_active = true;
    app.plan_gated = true;
    app.waiting_on_assistant = true;
    app.on_agent_event(AgentEvent::AssistantTurnComplete {
        message: smith_core::Message::assistant(vec![]),
        stop_reason: StopReason::EndTurn,
    });
    assert!(!app.plan_turn_active);
    assert!(app.lines.iter().any(|l| l.text.contains("/plan reject")));
}

#[test]
fn tool_call_leaves_a_permanent_transcript_line_that_updates_on_result() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "read_file".into(),
        input: serde_json::json!({"path": "src/main.rs"}),
    });
    let line = app
        .lines
        .iter()
        .find(|l| l.tool_id.as_deref() == Some("call_1"))
        .expect("tool line pushed to transcript");
    assert_eq!(line.tool_status, Some(ActivityStatus::Running));
    assert!(line.text.contains("src/main.rs"));

    app.on_agent_event(AgentEvent::ToolCallResult {
        id: "call_1".into(),
        output: "ok".into(),
        is_error: false,
    });
    let line = app
        .lines
        .iter()
        .find(|l| l.tool_id.as_deref() == Some("call_1"))
        .unwrap();
    assert_eq!(line.tool_status, Some(ActivityStatus::Done));
    assert_eq!(line.tool_output.as_deref(), Some("ok"));
}

#[test]
fn failed_tool_call_appends_error_snippet_to_its_transcript_line() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "run_bash".into(),
        input: serde_json::json!({"command": "cargo test"}),
    });
    app.on_agent_event(AgentEvent::ToolCallResult {
        id: "call_1".into(),
        output: "permission denied".into(),
        is_error: true,
    });
    let line = app
        .lines
        .iter()
        .find(|l| l.tool_id.as_deref() == Some("call_1"))
        .unwrap();
    assert_eq!(line.tool_status, Some(ActivityStatus::Error));
    assert!(line
        .tool_output
        .as_deref()
        .unwrap()
        .contains("permission denied"));
}

/// A round of ReadOnly calls runs concurrently, so starts, progress lines
/// and results arrive interleaved rather than in start/result pairs. Every
/// one of those events carries the call's id and every lookup here is by
/// id, so the cards resolve independently — asserted rather than assumed,
/// because "matches by id" is only true as long as nothing starts matching
/// by position instead.
#[test]
fn three_concurrent_tool_calls_resolve_independently_when_events_interleave() {
    let mut app = test_app();
    for id in ["call_1", "call_2", "call_3"] {
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: id.into(),
            tool_name: "read_file".into(),
            input: serde_json::json!({ "path": format!("src/{id}.rs") }),
        });
    }
    // Three cards, all spinning, in the order the model asked for them.
    let running: Vec<&str> = app
        .lines
        .iter()
        .filter(|l| l.tool_status == Some(ActivityStatus::Running))
        .map(|l| l.tool_id.as_deref().unwrap())
        .collect();
    assert_eq!(running, vec!["call_1", "call_2", "call_3"]);

    // Results and progress arrive in whatever order the calls finish.
    app.on_agent_event(AgentEvent::ToolCallResult {
        id: "call_3".into(),
        output: "third".into(),
        is_error: false,
    });
    app.on_agent_event(AgentEvent::ToolProgress {
        id: "call_1".into(),
        line: "still reading".into(),
    });
    app.on_agent_event(AgentEvent::ToolCallResult {
        id: "call_1".into(),
        output: "first".into(),
        is_error: true,
    });

    let card = |id: &str| {
        app.lines
            .iter()
            .find(|l| l.tool_id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("{id} has no card"))
    };
    assert_eq!(card("call_1").tool_status, Some(ActivityStatus::Error));
    assert_eq!(card("call_1").tool_output.as_deref(), Some("first"));
    // Still running, untouched by its neighbours' results.
    assert_eq!(card("call_2").tool_status, Some(ActivityStatus::Running));
    assert!(card("call_2").tool_output.is_none());
    assert_eq!(card("call_3").tool_status, Some(ActivityStatus::Done));
    assert_eq!(card("call_3").tool_output.as_deref(), Some("third"));

    // The thinking clock has not started: call_2 is still working, and
    // the time it takes is not the model thinking.
    assert!(app.thinking_since.is_none());
    app.on_agent_event(AgentEvent::ToolCallResult {
        id: "call_2".into(),
        output: "second".into(),
        is_error: false,
    });
    assert!(app.thinking_since.is_some());

    // Transcript order is still the model's order — cards are updated in
    // place, never re-appended as they finish.
    let ids: Vec<&str> = app
        .lines
        .iter()
        .filter_map(|l| l.tool_id.as_deref())
        .collect();
    assert_eq!(ids, vec!["call_1", "call_2", "call_3"]);
}

#[test]
fn thought_row_emitted_when_gap_exceeds_threshold() {
    let mut app = test_app();
    app.thinking_since = Some(Instant::now() - std::time::Duration::from_secs(2));
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "read_file".into(),
        input: serde_json::json!({"path": "src/main.rs"}),
    });
    assert!(app.lines.iter().any(|l| l.role == ChatRole::Thought));
}

#[test]
fn short_gap_does_not_emit_thought_row() {
    let mut app = test_app();
    app.thinking_since = Some(Instant::now());
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "read_file".into(),
        input: serde_json::json!({"path": "src/main.rs"}),
    });
    assert!(app.lines.iter().all(|l| l.role != ChatRole::Thought));
    // The gap timer is consumed and restarted so the next activity
    // measures a fresh window.
    assert!(app.thinking_since.is_none());
}

#[test]
fn tool_result_restarts_the_thinking_gap() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "read_file".into(),
        input: serde_json::json!({"path": "src/main.rs"}),
    });
    app.on_agent_event(AgentEvent::ToolCallResult {
        id: "call_1".into(),
        output: "ok".into(),
        is_error: false,
    });
    assert!(app.thinking_since.is_some());
}

#[test]
fn selection_survives_new_lines_arriving_while_the_user_reads() {
    let mut app = app_with_cards(2);
    app.toggle_card_focus();
    app.move_card_focus(false);
    assert_eq!(app.selected_card_id(), Some("call_0"));

    // A whole turn's worth of streaming lands underneath the cursor.
    app.on_agent_event(AgentEvent::AssistantTextDelta("mais texto".into()));
    for i in 9..12 {
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: format!("call_{i}"),
            tool_name: "grep".into(),
            input: serde_json::json!({ "pattern": "x" }),
        });
    }
    assert_eq!(
        app.selected_card_id(),
        Some("call_0"),
        "the cursor is carried by its line, not by an index"
    );
}

#[test]
fn ask_user_does_not_get_a_transcript_tool_line() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "ask_user".into(),
        input: serde_json::json!({}),
    });
    assert!(app.lines.iter().all(|l| l.role != ChatRole::Tool));
}

#[test]
fn write_tasks_does_not_get_a_transcript_tool_line() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "write_tasks".into(),
        input: serde_json::json!({"tasks": []}),
    });
    assert!(app.lines.iter().all(|l| l.role != ChatRole::Tool));
}

#[test]
fn tasks_updated_event_replaces_the_checklist() {
    let mut app = test_app();
    assert!(app.tasks.is_empty());
    app.on_agent_event(AgentEvent::TasksUpdated(vec![
        Task {
            content: "one".into(),
            status: TaskStatus::Completed,
        },
        Task {
            content: "two".into(),
            status: TaskStatus::InProgress,
        },
    ]));
    assert_eq!(app.tasks.len(), 2);
    assert_eq!(app.tasks[1].content, "two");

    app.on_agent_event(AgentEvent::TasksUpdated(vec![]));
    assert!(app.tasks.is_empty());
}

#[tokio::test]
async fn progress_lines_reach_the_running_tool_card() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "run_bash".into(),
        input: serde_json::json!({"command": "cargo build"}),
    });

    app.on_agent_event(AgentEvent::ToolProgress {
        id: "call_1".into(),
        line: "   Compiling smith-core".into(),
    });
    let card = app
        .lines
        .iter()
        .find(|l| l.tool_id() == Some("call_1"))
        .expect("the card exists");
    assert_eq!(card.tool_output(), Some("   Compiling smith-core"));

    // Only the newest line: the card is a status, not a scrollback.
    app.on_agent_event(AgentEvent::ToolProgress {
        id: "call_1".into(),
        line: "   Compiling smith-tui".into(),
    });
    let card = app
        .lines
        .iter()
        .find(|l| l.tool_id() == Some("call_1"))
        .unwrap();
    assert_eq!(card.tool_output(), Some("   Compiling smith-tui"));
}

#[tokio::test]
async fn progress_for_a_finished_call_does_not_overwrite_its_result() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "run_bash".into(),
        input: serde_json::json!({"command": "echo hi"}),
    });
    app.on_agent_event(AgentEvent::ToolCallResult {
        id: "call_1".into(),
        output: "hi".into(),
        is_error: false,
    });
    // A late progress line must not resurrect the card or clobber what it
    // actually returned.
    app.on_agent_event(AgentEvent::ToolProgress {
        id: "call_1".into(),
        line: "stale".into(),
    });
    let card = app
        .lines
        .iter()
        .find(|l| l.tool_id() == Some("call_1"))
        .unwrap();
    assert_eq!(card.tool_output(), Some("hi"));
}

#[test]
fn plan_gate_changed_event_syncs_state() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::PlanGateChanged { gated: true });
    assert!(app.plan_gated);
    app.on_agent_event(AgentEvent::PlanGateChanged { gated: false });
    assert!(!app.plan_gated);
}

#[test]
fn goal_changed_event_syncs_state_and_transcript() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::GoalChanged(Some(
        "ship the login page".to_string(),
    )));
    assert_eq!(app.goal.as_deref(), Some("ship the login page"));
    assert!(app.lines.iter().any(|l| l.text.contains("goal set:")));

    app.on_agent_event(AgentEvent::GoalChanged(None));
    assert!(app.goal.is_none());
    assert!(app.lines.iter().any(|l| l.text.contains("goal cleared")));
}

#[test]
fn loop_iteration_started_updates_progress_and_transcript() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::LoopIterationStarted {
        iteration: 2,
        max_iterations: 25,
    });
    assert_eq!(app.loop_progress, Some((2, 25)));
    assert!(matches!(app.phase, AgentPhase::Looping));
    assert!(app
        .lines
        .iter()
        .any(|l| l.text.contains("loop iteration 2/25")));
}

#[test]
fn assistant_turn_complete_mid_loop_does_not_reset_waiting_flag() {
    let mut app = test_app();
    app.loop_active = true;
    app.waiting_on_assistant = true;
    app.phase = AgentPhase::Looping;
    app.on_agent_event(AgentEvent::AssistantTurnComplete {
        message: smith_core::Message::assistant(vec![smith_core::ContentBlock::Text {
            text: "iteration one done".to_string(),
        }]),
        stop_reason: StopReason::EndTurn,
    });
    assert!(app.waiting_on_assistant);
    assert!(matches!(app.phase, AgentPhase::Looping));
}

#[test]
fn loop_finished_done_resets_state_and_reports_iterations() {
    let mut app = test_app();
    app.loop_active = true;
    app.waiting_on_assistant = true;
    app.loop_progress = Some((3, 25));
    app.phase = AgentPhase::Looping;
    app.on_agent_event(AgentEvent::LoopFinished {
        reason: smith_core::LoopStopReason::Done,
        iterations: 3,
    });
    assert!(!app.loop_active);
    assert!(app.loop_progress.is_none());
    assert!(!app.waiting_on_assistant);
    assert!(matches!(app.phase, AgentPhase::Idle));
    assert!(app
        .lines
        .iter()
        .any(|l| l.text.contains("loop finished") && l.text.contains("3")));
}

#[test]
fn loop_finished_cancelled_reports_cancellation() {
    let mut app = test_app();
    app.loop_active = true;
    app.on_agent_event(AgentEvent::LoopFinished {
        reason: smith_core::LoopStopReason::Cancelled,
        iterations: 1,
    });
    assert!(app.lines.iter().any(|l| l.text.contains("loop cancelled")));
}

#[test]
fn phase_changed_updates_label() {
    let mut app = test_app();
    assert_eq!(app.phase_label(), "idle");
    app.on_agent_event(AgentEvent::PhaseChanged(AgentPhase::Thinking));
    assert_eq!(app.phase_label(), "thinking…");
    app.on_agent_event(AgentEvent::PhaseChanged(AgentPhase::Building));
    assert_eq!(app.phase_label(), "building…");
    assert!(app.is_animating());
}

#[test]
fn building_phase_survives_thinking_event() {
    let mut app = test_app();
    app.phase = AgentPhase::Building;
    app.on_agent_event(AgentEvent::PhaseChanged(AgentPhase::Thinking));
    assert_eq!(app.phase, AgentPhase::Building);
}

/// The `run_bash` caveat has to survive the trip through the event channel
/// and land in the transcript — it is the one line that stops a user
/// believing the rewind was total.
#[test]
fn a_rewind_report_lands_in_the_transcript_caveats_and_all() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::Rewind(smith_core::RewindReport {
        turn: Some(3),
        status: smith_core::RewindStatus::Preview,
        restore: vec!["src/main.rs".into()],
        delete: Vec::new(),
        conflicts: Vec::new(),
        uncovered: vec![("run_bash".into(), 1)],
        notes: Vec::new(),
    }));

    let text: String = app
        .lines
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("rewind of turn 3 would"), "{text}");
    assert!(text.contains("restore src/main.rs"), "{text}");
    assert!(text.contains("NOT COVERED"), "{text}");
    assert!(text.contains("/rewind 3 confirm"), "{text}");
}

#[test]
fn the_catalogue_opens_a_picker_on_the_model_already_in_use() {
    let mut app = picker_app(&[("a:cloud", true), ("b:cloud", true), ("c:cloud", true)]);
    app.model_label = "b:cloud".to_string();

    // Re-deliver now that the label is set — the picker starts on what is in
    // use, so switching to the neighbour is one keypress rather than a scroll.
    app.on_agent_event(AgentEvent::ModelsAvailable {
        provider: "ollama".to_string(),
        models: vec![
            smith_core::ModelChoice {
                id: "a:cloud".into(),
                detail: String::new(),
                supports_tools: true,
            },
            smith_core::ModelChoice {
                id: "b:cloud".into(),
                detail: String::new(),
                supports_tools: true,
            },
        ],
    });
    let picker = app.modal.model().expect("the picker is open");
    assert_eq!(picker.selected, 1, "starts on the model in use");
}

/// An empty catalogue must not open an empty picker: the provider could not
/// be asked, and the way out is typing a name.
#[test]
fn an_unreadable_catalogue_says_so_instead_of_opening_nothing() {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ModelsAvailable {
        provider: "9router".to_string(),
        models: Vec::new(),
    });
    assert!(app.modal.is_none());
    assert!(app
        .lines
        .iter()
        .any(|l| l.text.contains("could not read") && l.text.contains("/model <name>")));
}
