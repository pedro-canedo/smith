//! The four modal drawers.

use super::*;

#[test]
fn permission_modal_scrolls_through_a_long_detail_and_keeps_its_offset() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = app_with_long_permission_request();
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();

    let first = screen_text(&terminal);
    assert!(first.contains("line-00"), "top of the command is missing");
    // A clipped detail has to say it is clipped. The scrollbar carries that
    // now — and unlike the words it replaced, it also says how far down the
    // command the visible rows are.
    assert!(
        first.contains('█'),
        "a clipped detail must show a scrollbar thumb"
    );

    for _ in 0..20 {
        app.on_key(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
    }
    assert_eq!(app.modal.permission().unwrap().scroll, 20);

    terminal.draw(|f| draw(f, &mut app)).unwrap();
    // The redraw used to reset `scroll` to 0, which made every scroll key
    // a no-op and left the tail of the command unreadable.
    assert_eq!(
        app.modal.permission().unwrap().scroll,
        20,
        "the redraw clobbered the scroll offset"
    );
    let scrolled = screen_text(&terminal);
    assert!(
        !scrolled.contains("line-00"),
        "the popup never actually moved"
    );
    assert!(scrolled.contains("line-20"), "scrolled past the content");
}

#[test]
fn permission_modal_grows_with_its_content_but_stays_inside_the_frame() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = app_with_long_permission_request();
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();

    // The popup was hardcoded to 8 rows — 6 of body — regardless of how
    // much detail there was to read. Count the rows its left border owns;
    // the input box's own border sits in column 0, not here.
    let popup_x = (60 - 60u16.saturating_sub(8).clamp(36, 72)) / 2;
    let popup_rows = (0..20)
        .filter(|y| {
            terminal
                .backend()
                .buffer()
                .cell(ratatui::layout::Position::new(popup_x, *y))
                .unwrap()
                .symbol()
                == "│"
        })
        .count();
    assert!(popup_rows > 6, "popup body was only {popup_rows} rows tall");
    assert!(popup_rows < 20, "popup grew past the frame");
}

/// Scrolling a modal to its end must leave no blank rows at the bottom.
///
/// `Paragraph::line_count` takes the **outer** width and returns the
/// **outer** height — it subtracts the block's borders before wrapping and
/// adds the two border rows back to the count. The plan modal mixed that
/// with the *inner* height, so `max_scroll` was two too large and the view
/// could sit two rows past its own content. Asserting "the last line is
/// still visible" is not enough to catch that: overscrolled by two, it is
/// visible, just with dead space under it. The dead space is the symptom.
#[test]
fn scrolling_a_modal_to_the_end_leaves_no_dead_space_under_it() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = test_app();
    let body: String = (0..60).map(|i| format!("plan line {i}\n")).collect();
    app.modal = crate::app::Modal::Plan(crate::app::PlanModal {
        text: format!("{body}THE-LAST-LINE"),
        scroll: 0,
    });

    // Far past any plausible end; the renderer clamps it.
    for _ in 0..40 {
        app.on_key(
            crossterm::event::KeyCode::PageDown,
            crossterm::event::KeyModifiers::NONE,
        );
    }

    let (w, h) = (100u16, 30u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer().clone();

    let row_text = |y: u16| -> String {
        (0..w)
            .map(|x| {
                buf.cell(ratatui::layout::Position::new(x, y))
                    .unwrap()
                    .symbol()
            })
            .collect()
    };

    // Anchored on the modal's own title, not on "the last corner glyph on
    // screen" — the input box is drawn below the modal and owns the last
    // border on the frame, so that shortcut measured the wrong widget.
    let top = (0..h)
        .find(|&y| row_text(y).contains("plan ready"))
        .expect("the modal drew its title");
    let bottom = ((top + 1)..h)
        .find(|&y| {
            let r = row_text(y);
            r.contains('╰') || r.contains('┘') || r.contains('+')
        })
        .expect("the modal drew a bottom border");

    // The row directly above it belongs to the content, and at the end of
    // the scroll it must not be empty.
    let last_content = row_text(bottom - 1);
    let inner: String = last_content
        .chars()
        .filter(|c| !matches!(c, '│' | '|' | ' '))
        .collect();
    assert!(
        !inner.is_empty(),
        "blank row above the bottom border: the modal scrolled past its \
             own content.\nbottom border row {bottom}: {last_content:?}"
    );
}

/// A permission prompt is a question the turn is blocked on; a panel the
/// user left open must not cover it.
#[test]
fn a_modal_takes_the_screen_back_from_an_open_overlay() {
    let mut app = app_with_context(100);
    app.run_slash_command("usage");
    app.on_agent_event(smith_core::AgentEvent::PermissionPromptNeeded(
        smith_core::PermissionRequest {
            tool_call_id: "1".into(),
            tool_name: "run_bash".into(),
            detail: "rm -rf /tmp/x".into(),
        },
    ));
    let text = rendered(&mut app, 100, 30);
    assert!(text.contains("rm -rf /tmp/x"), "modal not drawn: {text}");
    assert!(
        !text.contains("session usage"),
        "the overlay covered the prompt: {text}"
    );
}

#[test]
fn the_model_picker_shows_the_rows_and_what_to_press() {
    let mut modal = picker(
        &[
            ("nemotron-3-super:cloud", "cloud · 262k ctx", true),
            ("qwen3.5:9b", "6.6 GB", true),
        ],
        "",
        0,
    );
    let text = picker_rows(80, 24, &mut modal).join("\n");

    assert!(text.contains("ollama models"), "{text}");
    assert!(text.contains("nemotron-3-super:cloud"), "{text}");
    assert!(text.contains("262k ctx"), "{text}");
    assert!(text.contains("type to filter"), "{text}");
    assert!(text.contains("Enter"), "{text}");
    assert!(text.contains("Esc"), "{text}");
}

/// A model that cannot call tools makes an agent that cannot open a file, and
/// the failure would arrive a turn later. The row says so.
#[test]
fn a_model_without_tools_is_marked_on_its_row() {
    let mut modal = picker(&[("embed-only", "", false)], "", 0);
    let text = picker_rows(80, 24, &mut modal).join("\n");
    assert!(text.contains("NO TOOLS"), "{text}");
}

/// The filter is echoed as typed, and only what survives it is drawn.
#[test]
fn the_filter_is_visible_and_the_list_follows_it() {
    let mut modal = picker(
        &[
            ("gpt-oss:20b-cloud", "", true),
            ("gemma4:31b-cloud", "", true),
        ],
        "gpt",
        0,
    );
    let text = picker_rows(80, 24, &mut modal).join("\n");
    assert!(text.contains("gpt-oss:20b-cloud"), "{text}");
    assert!(!text.contains("gemma4"), "filtered out: {text}");
}

/// Acceptance criterion #7's terminal. A picker taller than the frame is one
/// whose hint row cannot be read, and that row is where the way out is
/// written.
#[test]
fn the_picker_fits_an_eighty_by_twentyfour_terminal() {
    let many: Vec<(&str, &str, bool)> = (0..40)
        .map(|_| ("openrouter/nvidia/nemotron-3-nano-30b-a3b:free", "", true))
        .collect();
    let mut modal = picker(&many, "", 0);
    let rows = picker_rows(80, 24, &mut modal);

    assert_eq!(rows.len(), 24);
    for row in &rows {
        assert!(
            row.chars().count() <= 80,
            "a row ran past the frame: {row:?}"
        );
    }
    assert!(rows.join("\n").contains("Enter"), "the hint survived");
}
