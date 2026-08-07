//! Input box, status bar, overlay, queue, suggestions, idle splash.

use super::*;

#[test]
fn input_box_starts_at_the_minimum_height() {
    let mut app = test_app();
    assert_eq!(input_height(&mut app, frame(80, 30)), INPUT_MIN_ROWS);
}

#[test]
fn input_box_grows_with_content_then_caps() {
    let mut app = test_app();
    app.input.set(&"palavra ".repeat(12));
    let grown = input_height(&mut app, frame(60, 30));
    assert!(
        grown > INPUT_MIN_ROWS,
        "expected growth beyond {INPUT_MIN_ROWS}, got {grown}"
    );

    app.input.set(&"palavra ".repeat(400));
    assert_eq!(input_height(&mut app, frame(60, 30)), INPUT_MAX_ROWS);
}

#[test]
fn input_box_never_crowds_out_a_short_terminal() {
    let mut app = test_app();
    app.input.set(&"palavra ".repeat(400));
    // 8 rows tall: the prompt must leave room for transcript + status bar.
    let height = input_height(&mut app, frame(60, 8));
    assert!(height <= 5, "prompt took {height} of 8 rows");
}

#[test]
fn long_input_renders_its_tail_and_places_the_caret_in_the_box() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = test_app();
    app.input.set(&"palavra ".repeat(60));

    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();

    // Goes through the real path: `draw_input` -> `set_cursor_position`
    // -> backend. Before this change nothing ever set a cursor at all.
    let position = terminal
        .get_cursor_position()
        .expect("a caret must be visible in the input box");
    let box_top = 20 - INPUT_MAX_ROWS - 1; // status bar takes the last row
    assert!(
        position.y > box_top && position.y < 19,
        "caret at {position:?} escaped the input box"
    );

    // The tail of the text — where the caret sits — has to be on screen;
    // the old unwrapped Paragraph clipped everything past the first row.
    let rendered = terminal.backend().buffer().area();
    assert_eq!(rendered.height, 20);
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        text.contains("palavra"),
        "input text never made it to screen"
    );
}

#[test]
fn a_pending_quit_announces_itself_in_the_status_bar() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = test_app();
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    assert!(!screen_text(&terminal).contains(QUIT_HINT));

    app.on_key(
        crossterm::event::KeyCode::Char('c'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    assert!(
        screen_text(&terminal).contains(QUIT_HINT),
        "the second press has to be discoverable"
    );
}

#[test]
fn a_tiny_terminal_still_gets_a_prompt_and_a_status_bar() {
    for height in [1u16, 2, 3, 4, 8] {
        let l = vertical_layout(height, INPUT_MAX_ROWS, 7, 0, true);
        assert_eq!(
            l.messages + l.strip + l.suggest + l.input + l.status,
            height,
            "height {height} over-allocated"
        );
        assert!(l.input <= height, "height {height}");
    }
}

#[test]
fn card_focus_publishes_its_keymap_in_the_status_bar() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = test_app();
    app.on_agent_event(smith_core::AgentEvent::ToolCallStarted {
        id: "call_1".into(),
        tool_name: "read_file".into(),
        input: serde_json::json!({"path": "src/main.rs"}),
    });
    app.on_agent_event(smith_core::AgentEvent::ToolCallResult {
        id: "call_1".into(),
        output: "ok".into(),
        is_error: false,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    assert!(!screen_text(&terminal).contains("Enter expand"));

    app.toggle_card_focus();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    assert!(
        screen_text(&terminal).contains("Enter expand"),
        "a mode that steals Enter has to say so"
    );
}

#[test]
fn an_overlay_table_draws_its_header_and_rows_inside_a_titled_box() {
    let mut app = app_with_context(100);
    app.request_count = 7;
    app.on_agent_event(smith_core::AgentEvent::SessionCost {
        usd: 1.5,
        unpriced_turns: 0,
    });
    app.run_slash_command("usage");

    let text = rendered(&mut app, 100, 30);
    assert!(text.contains("session usage"), "no panel title: {text}");
    assert!(text.contains("metric"), "no column header: {text}");
    assert!(text.contains("requests"), "no metric row: {text}");
    assert!(text.contains("~$1.5000"), "cost not shown: {text}");
}

#[test]
fn the_log_panel_is_reachable_and_renders_what_was_logged() {
    let mut app = app_with_context(100);
    app.logs.push(crate::logbuf::LogLine {
        level: crate::logbuf::LogLevel::Warn,
        target: "smith_mcp::transport".into(),
        message: "unparseable frame".into(),
    });
    app.on_key(
        crossterm::event::KeyCode::Char('l'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    let text = rendered(&mut app, 100, 30);
    assert!(text.contains("diagnostics"), "no panel: {text}");
    assert!(text.contains("unparseable frame"), "no log line: {text}");
}

/// The 80x24 contract from `docs/design-system.md` §3 covers the panels
/// too: a table that overflows its box is worse than no table.
#[test]
fn an_overlay_stays_inside_the_frame_at_80x24() {
    let mut app = app_with_context(80);
    app.theme = Theme::ansi().ascii_glyphs();
    app.run_slash_command("usage");
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    // `TestBackend` panics on an out-of-bounds write, so reaching here is
    // most of the assertion; the rest is that the panel is actually there.
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("session usage"), "{text}");
    assert!(text.is_ascii(), "non-ASCII glyph under an ASCII theme");
}

/// Regression: the list drew a fixed six rows while `slash_selected` was
/// bounded only by how many commands matched, so from the seventh match on
/// the highlighted row was off screen — nothing looked selected, and Enter
/// accepted a command the user could not see. `ListState` scrolls to the
/// selection instead.
#[test]
fn the_selected_suggestion_stays_on_screen_past_the_first_windowful() {
    let mut app = test_app();
    app.input.insert_str("/");
    let total = app.suggestions().len();
    assert!(
        total > 6,
        "fixture needs more than one window of commands, got {total}"
    );

    // Walk to the last one.
    for _ in 0..total {
        app.on_key(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
    }
    let picked = app.suggestions()[app.slash_selected].name.clone();
    let text = rendered(&mut app, 80, 24);
    assert!(
        text.contains(&picked),
        "the selected suggestion `{picked}` was not drawn:\n{text}"
    );
}
