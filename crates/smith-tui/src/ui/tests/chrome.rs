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
    assert_eq!(input_height(&mut app, frame(60, 30)), input_max_rows(30));
}

/// The ceiling follows the terminal. A flat ten rows was over 40% of an
/// 80x24 window and a sixth of a full-screen one, so the same draft was
/// cramped in the small terminal and lost in the big one.
#[test]
fn the_prompts_ceiling_grows_with_the_terminal_and_then_stops() {
    let mut app = test_app();
    app.input.set(&"palavra ".repeat(400));

    let short = input_height(&mut app, frame(60, 24));
    let tall = input_height(&mut app, frame(60, 50));
    assert!(
        tall > short,
        "a taller terminal must offer a taller prompt: {short} vs {tall}"
    );
    // ...but never the whole screen: the transcript is what the rows are for.
    assert_eq!(
        input_height(&mut app, frame(60, 200)),
        INPUT_MAX_ROWS_CEILING
    );
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

/// Renders a session and reports `(column_first, column_last, sidebar_x)` —
/// the prompt's own borders are the document column's edges, and the
/// sidebar's left border is where the sidebar starts.
fn laid_out(w: u16) -> (u16, u16, Option<u16>) {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = app_with_context(w);
    app.lines
        .push(ChatLine::new(ChatRole::User, "layout".to_string()));
    let mut t = Terminal::new(TestBackend::new(w, 20)).unwrap();
    t.draw(|f| draw(f, &mut app)).unwrap();
    let buf = t.backend().buffer().clone();

    let prompt_row = 18;
    let first = (0..w)
        .find(|x| buf.cell((*x, prompt_row)).unwrap().symbol() != " ")
        .unwrap();
    let last = (0..w)
        .rev()
        .find(|x| buf.cell((*x, prompt_row)).unwrap().symbol() != " ")
        .unwrap();
    // The sidebar's `Borders::LEFT` rule, on a transcript row.
    let sidebar = (0..w).find(|x| buf.cell((*x, 1)).unwrap().symbol() == "│" && *x > last);
    (first, last, sidebar)
}

/// The sidebar is window chrome, so it belongs on the window's edge. It used
/// to ride inside the centred column, which on a wide screen put a panel in
/// the middle of the terminal with a field of empty cells to its right —
/// visibly attached to nothing.
#[test]
fn the_sidebar_hugs_the_terminals_right_edge_on_a_wide_terminal() {
    let (_, _, sidebar) = laid_out(180);
    let sidebar = sidebar.expect("the sidebar was not drawn at 180 columns");
    assert_eq!(
        sidebar,
        180 - SIDEBAR_WIDTH,
        "the sidebar must start exactly its own width from the right edge"
    );
}

/// The document column centres in what the sidebar left, and grows with the
/// terminal instead of holding a hundred columns while the screen goes to
/// waste — capped, so a line of prose never becomes a scan across the desk.
#[test]
fn the_document_column_centres_in_what_the_sidebar_left_and_grows_with_the_screen() {
    let (first, last, _) = laid_out(180);
    let width = last - first + 1;
    let available = 180 - SIDEBAR_WIDTH;
    assert_eq!(width, content_width(available), "column width");
    assert!(
        width > MAX_CONTENT_WIDTH,
        "a 180-column terminal must buy more than the reading measure, got {width}"
    );
    assert!(width <= MAX_WIDE_CONTENT_WIDTH, "past the wide ceiling");
    // Centred in the space left of the sidebar, not in the whole terminal.
    assert_eq!(first, (available - width) / 2, "left margin");

    // A very wide terminal stops at the ceiling rather than following the
    // window out.
    let (first, last, _) = laid_out(400);
    assert_eq!(last - first + 1, MAX_WIDE_CONTENT_WIDTH);
}

/// At or below the measure nothing moves: a narrow terminal is laid out
/// exactly as it was before there was a column at all.
#[test]
fn a_narrow_terminal_still_uses_every_column_it_has() {
    for w in [100u16, 80, 60] {
        let (first, last, _) = laid_out(w);
        let available = if w >= SIDEBAR_MIN_TERMINAL_WIDTH {
            w - SIDEBAR_WIDTH
        } else {
            w
        };
        assert_eq!(first, 0, "w={w} grew a margin it cannot afford");
        assert_eq!(
            last,
            available - 1,
            "w={w} left a gap between the column and the sidebar"
        );
    }
}

/// The splash's box has to *look* like a box. One row of the source art
/// carried a stray space past its right border, and a centred `Paragraph`
/// centres each row by its own width — so that row sat a cell off from its
/// neighbours and the frame came out ragged.
#[test]
fn the_idle_banner_draws_a_rectangular_box() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = test_app();
    // The real art, not the fixture's placeholder — the box is the point.
    app.banner = crate::banner::banner();
    assert!(app.lines.is_empty(), "the splash needs an idle session");
    let mut t = Terminal::new(TestBackend::new(200, 30)).unwrap();
    t.draw(|f| draw(f, &mut app)).unwrap();
    let buf = t.backend().buffer().clone();

    // Every row of the banner opens and closes in the same two columns.
    // Scanned above the prompt, whose own box would otherwise join in with
    // a different (and legitimately different) pair of edges.
    let mut edges: Vec<(u16, u16)> = Vec::new();
    for y in 0..30 - (INPUT_MIN_ROWS + 3) {
        let left = (0..200).find(|x| {
            matches!(
                buf.cell((*x, y)).unwrap().symbol(),
                "\u{250c}" | "\u{2502}" | "\u{2514}"
            )
        });
        let right = (0..200).rev().find(|x| {
            matches!(
                buf.cell((*x, y)).unwrap().symbol(),
                "\u{2510}" | "\u{2502}" | "\u{2518}"
            )
        });
        if let (Some(l), Some(r)) = (left, right) {
            if r > l + 40 {
                edges.push((l, r));
            }
        }
    }
    assert!(edges.len() >= 8, "the banner box did not draw: {edges:?}");
    let first = edges[0];
    assert!(
        edges.iter().all(|e| *e == first),
        "ragged banner box — rows disagree about their borders: {edges:?}"
    );
}
