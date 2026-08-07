use super::*;
// `use super::*` reaches only what `app.rs` itself still names. The split moved
// most of this file's vocabulary into sibling modules, so it is imported here
// by its own path instead of inherited from the parent.
use smith_core::{AgentEvent, McpCommand, PermissionRequest, StopReason, TaskStatus, UserQuestion};
use std::time::Duration;

use crate::testkit::{app_with_cards, app_with_command_files, test_app};

mod chatline;
mod commands;
mod events;
mod keys;

// Fixtures, and the few tests too small to earn a file of their
// own: tool labels, `format_thought`, and the mouse.

#[test]
fn tool_labels_cover_the_builtin_lifecycles() {
    let search = tool_labels("web_search");
    assert_eq!(search.running, "Searching the web…");
    assert_eq!(search.done, "Search completed");
    assert_eq!(search.failed, "Search failed");

    let read = tool_labels("read_file");
    assert_eq!(read.running, "Reading");
    assert_eq!(read.done, "Read");

    let bash = tool_labels("run_bash");
    assert_eq!(bash.running, "Running command…");
    assert_eq!(bash.done, "Command completed");
}

#[test]
fn tool_labels_prettify_mcp_names_and_pass_unknowns_through() {
    let mcp = tool_labels("mcp__github__create_issue");
    assert_eq!(mcp.running, "Calling github · create_issue…");
    assert_eq!(mcp.done, "github · create_issue completed");
    assert_eq!(mcp.failed, "github · create_issue failed");

    let unknown = tool_labels("frobnicate");
    assert_eq!(unknown.running, "Calling frobnicate…");
}

use crossterm::event::{KeyCode, KeyModifiers};

/// Reads one cell out of the `/usage` table by its metric name.
fn usage_cell(app: &App, metric: &str) -> String {
    let overlay = app.overlay.as_ref().expect("usage should open a panel");
    let OverlayBody::Table { rows, .. } = &overlay.body else {
        panic!("expected a table");
    };
    rows.iter()
        .find(|r| r[0] == metric)
        .unwrap_or_else(|| panic!("no `{metric}` row in {rows:?}"))[1]
        .clone()
}

fn submit(app: &mut App, text: &str) {
    app.input.set(text);
    app.on_key(KeyCode::Enter, KeyModifiers::NONE);
}

#[test]
fn the_wheel_scrolls_the_transcript_and_releases_the_live_edge() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut app = test_app();
    app.scroll = 10;
    app.follow_bottom = true;

    let wheel = |kind| MouseEvent {
        kind,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };
    app.on_mouse(wheel(MouseEventKind::ScrollUp));
    assert_eq!(app.scroll, 7);
    assert!(!app.follow_bottom, "scrolling up unpins the live edge");
    app.on_mouse(wheel(MouseEventKind::ScrollDown));
    assert_eq!(app.scroll, 10);
    // A click outside any recorded transcript area is inert, not a panic.
    app.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 200,
        row: 200,
        modifiers: KeyModifiers::NONE,
    });
}

fn search(app: &mut App, id: &str, query: &str) {
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: id.to_string(),
        tool_name: "web_search".into(),
        input: serde_json::json!({ "query": query }),
    });
}

fn fetch(app: &mut App, id: &str, url: &str) {
    app.on_agent_event(AgentEvent::ToolCallStarted {
        id: id.to_string(),
        tool_name: "web_fetch".into(),
        input: serde_json::json!({ "url": url }),
    });
}

fn finish(app: &mut App, id: &str) {
    app.on_agent_event(AgentEvent::ToolCallResult {
        id: id.to_string(),
        output: "3 results".into(),
        is_error: false,
    });
}

fn fail(app: &mut App, id: &str, why: &str) {
    app.on_agent_event(AgentEvent::ToolCallResult {
        id: id.to_string(),
        output: why.to_string(),
        is_error: true,
    });
}

#[test]
fn format_thought_uses_ms_below_one_second() {
    assert_eq!(format_thought(0.959), "959ms");
    assert_eq!(format_thought(1.234), "1.2s");
}

fn ctrl_c(app: &mut App) -> Option<Action> {
    app.on_key(
        crossterm::event::KeyCode::Char('c'),
        crossterm::event::KeyModifiers::CONTROL,
    )
}

fn question_modal() -> Modal {
    Modal::Question(QuestionModal {
        question: UserQuestion {
            id: "q1".into(),
            prompt: "Which approach?".into(),
            options: ["Alpha".into(), "Beta".into(), "Gamma".into()],
        },
        selected: 0,
        custom: String::new(),
    })
}

fn permission_modal() -> Modal {
    Modal::Permission(PermissionModal {
        request: PermissionRequest {
            tool_call_id: "call_1".into(),
            tool_name: "run_bash".into(),
            detail: "rm -rf build".into(),
        },
        scroll: 0,
    })
}

// ---- the /model picker -----------------------------------------------------

fn picker_app(models: &[(&str, bool)]) -> App {
    let mut app = test_app();
    app.on_agent_event(AgentEvent::ModelsAvailable {
        provider: "ollama".to_string(),
        models: models
            .iter()
            .map(|(id, tools)| smith_core::ModelChoice {
                id: (*id).to_string(),
                detail: String::new(),
                supports_tools: *tools,
            })
            .collect(),
    });
    app
}
