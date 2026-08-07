//! Tool cards: header, grouping, diff body, verbose output, errors.

use super::*;

#[test]
fn tool_card_header_shows_friendly_label_target_and_duration() {
    let theme = Theme::ansi();
    let lines = tool_card(
        &theme,
        &tool_line("read_file", ActivityStatus::Done),
        60,
        false,
        0,
    );
    let header = lines[0].to_string();
    assert!(header.contains("✓"), "{header}");
    // Friendly label, not the raw tool name — that moved to verbose.
    assert!(header.contains("Read · src/main.rs"), "{header}");
    assert!(!header.contains("read_file"), "{header}");
    assert!(header.contains("400ms"), "{header}");
}

/// The three lifecycle states speak different labels — running reads as a
/// sentence, done as a verdict, error as a failure — and web_search never
/// shows its raw name outside verbose.
#[test]
fn tool_card_labels_follow_the_lifecycle() {
    let theme = Theme::ansi();
    let search = |status| {
        ChatLine::test_tool(
            "web_search",
            status,
            serde_json::json!({"query": "mega sena"}),
            Some("results"),
        )
    };

    let running = tool_card(&theme, &search(ActivityStatus::Running), 60, false, 0)[0].to_string();
    assert!(
        running.contains("Searching the web… mega sena"),
        "{running}"
    );

    let done = tool_card(&theme, &search(ActivityStatus::Done), 60, false, 0)[0].to_string();
    assert!(done.contains("Search completed · mega sena"), "{done}");

    let failed = tool_card(&theme, &search(ActivityStatus::Error), 60, false, 0)[0].to_string();
    assert!(failed.contains("Search failed · mega sena"), "{failed}");

    for header in [running, done, failed] {
        assert!(!header.contains("web_search"), "{header}");
    }
}

/// The raw tool name is still reachable — in the verbose body, where
/// "what actually ran" belongs.
#[test]
fn tool_card_verbose_body_carries_the_raw_tool_name() {
    let theme = Theme::ansi();
    let compact: String = tool_card(
        &theme,
        &tool_line("read_file", ActivityStatus::Done),
        60,
        false,
        0,
    )
    .iter()
    .map(Line::to_string)
    .collect();
    assert!(!compact.contains("read_file"), "{compact}");

    let verbose: String = tool_card(
        &theme,
        &tool_line("read_file", ActivityStatus::Done),
        60,
        true,
        0,
    )
    .iter()
    .map(Line::to_string)
    .collect();
    assert!(verbose.contains("read_file"), "{verbose}");
}

/// An MCP-bridged tool gets the server · tool reading, not the raw
/// `mcp__` mangling.
#[test]
fn tool_card_prettifies_mcp_tool_names() {
    let theme = Theme::ansi();
    let line = ChatLine::test_tool(
        "mcp__github__create_issue",
        ActivityStatus::Done,
        serde_json::json!({}),
        Some("done"),
    );
    let header = tool_card(&theme, &line, 60, false, 0)[0].to_string();
    assert!(
        header.contains("github · create_issue completed"),
        "{header}"
    );
    assert!(!header.contains("mcp__"), "{header}");
}

/// The newest ToolProgress line shows on the running card — before this
/// it was stored by `set_progress` and never rendered.
#[test]
fn tool_card_running_shows_the_latest_progress_line() {
    let theme = Theme::ansi();
    let line = ChatLine::test_tool(
        "web_search",
        ActivityStatus::Running,
        serde_json::json!({"query": "mega sena"}),
        Some("trying Bing…"),
    );
    let text: String = tool_card(&theme, &line, 60, false, 0)
        .iter()
        .map(Line::to_string)
        .collect();
    assert!(text.contains("trying Bing…"), "{text}");
}

#[test]
fn tool_card_compact_done_has_header_only() {
    let theme = Theme::ansi();
    let lines = tool_card(
        &theme,
        &tool_line("read_file", ActivityStatus::Done),
        60,
        false,
        0,
    );
    assert_eq!(lines.len(), 1);
}

#[test]
fn tool_card_verbose_shows_output() {
    let theme = Theme::ansi();
    let lines = tool_card(
        &theme,
        &tool_line("read_file", ActivityStatus::Done),
        60,
        true,
        0,
    );
    let text: String = lines.iter().map(|l| l.to_string()).collect();
    assert!(text.contains("file contents"), "{text}");
}

#[test]
fn tool_card_error_shows_tail_even_when_compact() {
    let theme = Theme::ansi();
    let line = ChatLine::test_tool(
        "run_bash",
        ActivityStatus::Error,
        serde_json::json!({"command": "cargo test"}),
        Some("boom: permission denied"),
    );
    let lines = tool_card(&theme, &line, 60, false, 0);
    let text: String = lines.iter().map(|l| l.to_string()).collect();
    assert!(text.contains("permission denied"), "{text}");
}

#[test]
fn tool_card_paints_raised_bg_across_full_width_in_buffer() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::Terminal;

    let theme = Theme::ansi();
    let lines = tool_card(
        &theme,
        &tool_line("read_file", ActivityStatus::Done),
        40,
        false,
        0,
    );
    let height = lines.len() as u16;
    let backend = TestBackend::new(40, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let paragraph = Paragraph::new(Text::from(lines));
    terminal
        .draw(|f| {
            f.render_widget(
                paragraph,
                Rect {
                    x: 0,
                    y: 0,
                    width: 40,
                    height,
                },
            );
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    // The trailing padding cell must carry the raised surface bg —
    // without fill_line the terminal's own bg would leak through.
    assert_eq!(
        buf.cell(Position::new(39, 0)).unwrap().style().bg,
        Some(theme.raised)
    );
    assert_eq!(
        buf.cell(Position::new(0, 0)).unwrap().style().bg,
        Some(theme.raised)
    );
}

// --- Per-card throbbers (design-system §2.13) -------------------------

#[test]
fn two_cards_started_at_different_times_show_different_frames() {
    use std::time::Duration;

    let theme = Theme::ansi();
    let frames = theme.spinner_frames();
    let early = ChatLine::test_tool_started(
        "run_bash",
        "a",
        Duration::from_millis(crate::app::SPINNER_INTERVAL_MS as u64 * 3),
    );
    let late = ChatLine::test_tool_started("run_bash", "b", Duration::from_millis(0));

    let a = tool_card(&theme, &early, 60, false, 0)[0].to_string();
    let b = tool_card(&theme, &late, 60, false, 0)[0].to_string();
    assert!(
        a.starts_with(frames[3]) && b.starts_with(frames[0]),
        "cards animated in lockstep: {a:?} vs {b:?}"
    );
}

#[test]
fn a_card_with_no_start_time_falls_back_to_the_global_counter() {
    let theme = Theme::ansi();
    let frames = theme.spinner_frames();
    let line = ChatLine::test_tool(
        "read_file",
        ActivityStatus::Running,
        serde_json::json!({"path": "src/main.rs"}),
        None,
    );
    for frame in [0usize, 1, 7] {
        let header = tool_card(&theme, &line, 60, false, frame)[0].to_string();
        assert!(header.starts_with(frames[frame % frames.len()]), "{header}");
    }
}

#[test]
fn the_card_icons_degrade_to_ascii() {
    let theme = Theme::ansi().ascii_glyphs();
    for (status, expected) in [(ActivityStatus::Done, "+"), (ActivityStatus::Error, "x")] {
        let header =
            tool_card(&theme, &tool_line("read_file", status), 60, false, 0)[0].to_string();
        assert!(header.starts_with(expected), "{header}");
        assert!(header.is_ascii(), "{header}");
    }
}

// --- Per-card selection and expansion (design-system §2.13) -----------

#[test]
fn a_selected_card_is_marked_and_raised_but_others_are_untouched() {
    let theme = Theme::ansi();
    let plain = tool_card(
        &theme,
        &tool_line("read_file", ActivityStatus::Done),
        60,
        false,
        0,
    );
    let picked = tool_card(
        &theme,
        &tool_line("read_file", ActivityStatus::Done).test_selected(),
        60,
        false,
        0,
    );

    assert!(!plain[0].to_string().starts_with(theme.marker_selected()));
    assert!(
        picked[0].to_string().starts_with(theme.marker_selected()),
        "no cursor: {}",
        picked[0]
    );
    assert_eq!(
        picked[0].spans[0].style.bg,
        Some(theme.hover),
        "the selected row must sit on the hover surface"
    );
}

#[test]
fn enter_expands_one_card_without_touching_the_global_default() {
    use crossterm::event::{KeyCode, KeyModifiers};
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
        output: "conteudo secreto do arquivo".into(),
        is_error: false,
    });

    let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    assert!(!screen_text(&terminal).contains("conteudo secreto"));

    app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    app.on_key(KeyCode::Enter, KeyModifiers::NONE);
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    assert!(
        screen_text(&terminal).contains("conteudo secreto"),
        "Enter did not expand the selected card"
    );
    assert!(!app.verbose_tools);
}

/// While the run is live, one header for the activity and one row per step.
#[test]
fn a_live_research_card_lists_its_steps_under_one_header() {
    let mut app = research_run(&["rust 1.97 release", "rust release schedule"]);
    let text = rendered(&mut app, 90, 24);

    assert_eq!(
        text.matches("Researching the web").count(),
        1,
        "the activity header repeated: {text}"
    );
    // Both queries, including the first — that one is the card's *own* call,
    // which the header no longer carries, so a step list that forgot to
    // include it fails here.
    assert!(text.contains("rust 1.97 release"), "{text}");
    assert!(text.contains("rust release schedule"), "{text}");
    assert!(text.contains("2 steps"), "{text}");
}

/// The point of the whole thing: a settled run is one row, and its steps come
/// back on demand.
#[test]
fn a_settled_research_card_collapses_to_its_summary_and_expands_again() {
    let mut app = research_run(&["rust 1.97 release", "rust release schedule"]);
    for i in 0..2 {
        app.on_agent_event(smith_core::AgentEvent::ToolCallResult {
            id: format!("s{i}"),
            output: "3 results".into(),
            is_error: i == 1,
        });
    }

    let collapsed = rendered(&mut app, 90, 24);
    assert!(collapsed.contains("Research"), "{collapsed}");
    assert!(collapsed.contains("2 steps"), "{collapsed}");
    // Named on the header, which is what lets the card settle as done without
    // the blocked search going quiet.
    assert!(collapsed.contains("1 failed"), "{collapsed}");
    assert!(
        !collapsed.contains("rust release schedule"),
        "a settled group must not still list its steps: {collapsed}"
    );

    // Opened through the gesture the feature is about: click to select the
    // card, click again to expand it.
    let index = app
        .lines
        .iter()
        .position(|l| l.role() == ChatRole::Tool)
        .unwrap();
    let (start, _) = app.transcript.entry_rows(index).unwrap();
    let click = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: app.message_area.x,
        row: app.message_area.y + (start as u16).saturating_sub(app.scroll),
        modifiers: crossterm::event::KeyModifiers::NONE,
    };
    app.on_mouse(click);
    app.on_mouse(click);

    let expanded = rendered(&mut app, 90, 24);
    assert!(expanded.contains("rust 1.97 release"), "{expanded}");
    assert!(expanded.contains("rust release schedule"), "{expanded}");
}
