//! User bubbles, assistant blocks, and the transcript viewport.

use super::*;

#[test]
fn user_bubble_wraps_long_text_and_every_row_matches_the_width() {
    // The bubble used to clamp only the box, not the content: a long
    // message produced rows wider than the frame drawn around them, and
    // the transcript's `Wrap` then folded the closing border onto its own
    // row. Every row being exactly `width` is what makes that impossible.
    let theme = Theme::truecolor();
    let text = "palavra ".repeat(50);
    for line in user_bubble(&theme, &text, 60) {
        assert_eq!(line.width(), 60, "row: {line}");
    }
}

#[test]
fn user_bubble_is_closed_on_all_four_corners() {
    let theme = Theme::truecolor();
    let lines = user_bubble(&theme, "olá", 60);
    let top = lines[0].to_string();
    let bottom = lines[lines.len() - 1].to_string();
    assert!(top.starts_with('╭') && top.ends_with('╮'), "top: {top}");
    assert!(
        bottom.starts_with('╰') && bottom.ends_with('╯'),
        "bottom: {bottom}"
    );
    assert!(top.contains("You"), "bubble should be labelled: {top}");
}

#[test]
fn user_bubble_spans_the_full_width_even_for_a_short_message() {
    let theme = Theme::truecolor();
    let lines = user_bubble(&theme, "oi", 60);
    assert!(lines.iter().all(|l| l.width() == 60));
}

#[test]
fn user_bubble_keeps_explicit_newlines_as_separate_rows() {
    let theme = Theme::truecolor();
    let lines = user_bubble(&theme, "one\ntwo", 60);
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    assert!(text.iter().any(|l| l.contains("one")));
    assert!(text.iter().any(|l| l.contains("two")));
}

#[test]
fn assistant_block_gutters_every_row_and_stays_within_width() {
    let theme = Theme::truecolor();
    let (first_gutter, cont_gutter) = theme.assistant_gutter();
    let lines = assistant_block(&theme, &"resposta longa ".repeat(20), None, 50);
    assert!(lines.len() > 1, "should have wrapped");
    for (i, line) in lines.iter().enumerate() {
        let text = line.to_string();
        let expected = if i == 0 { first_gutter } else { cont_gutter };
        assert!(text.starts_with(expected), "row {i}: {text}");
        assert!(line.width() <= 50, "row {i} was {} wide", line.width());
    }
}

#[test]
fn assistant_block_folds_the_meta_caption_inside_the_gutter() {
    let theme = Theme::truecolor();
    let lines = assistant_block(&theme, "pronto", Some("ollama · 2.2s"), 50);
    let last = lines[lines.len() - 1].to_string();
    assert!(last.contains("ollama · 2.2s"), "last: {last}");
    assert!(last.starts_with(theme.assistant_gutter().1), "last: {last}");
}

#[test]
fn streaming_and_finished_replies_get_identical_chrome() {
    // Otherwise the text visibly jumps when the turn completes.
    let theme = Theme::truecolor();
    let streaming = assistant_block(&theme, "meia resp", None, 50);
    let finished = assistant_block(&theme, "meia resp", None, 50);
    assert_eq!(
        streaming.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
        finished.iter().map(|l| l.to_string()).collect::<Vec<_>>()
    );
}

#[test]
fn fit_lines_never_leaves_a_row_wider_than_the_pane() {
    let theme = Theme::truecolor();
    let mut lines = user_bubble(&theme, "curto", 40);
    lines.push(Line::from(
        "uma linha de sistema muito mais larga do que o painel",
    ));
    lines.extend(assistant_block(&theme, &"x ".repeat(80), None, 40));

    for line in fit_lines(lines, 40) {
        assert!(line.width() <= 40, "row: {line}");
    }
}

#[test]
fn caching_does_not_change_a_single_cell() {
    for (follow, scroll) in [(true, 0u16), (false, 0), (false, 37), (false, 9_999)] {
        let mut cached = test_app();
        long_transcript(&mut cached, 200);
        let mut legacy = test_app();
        long_transcript(&mut legacy, 200);
        for app in [&mut cached, &mut legacy] {
            app.follow_bottom = follow;
            app.scroll = scroll;
        }

        let a = buffer_of(72, 24, |f, area| draw_messages(f, &mut cached, area));
        let b = buffer_of(72, 24, |f, area| legacy_draw_messages(f, &mut legacy, area));
        assert_eq!(a, b, "follow_bottom={follow} scroll={scroll}");
        assert_eq!(
            cached.scroll, legacy.scroll,
            "scroll math diverged (follow_bottom={follow})"
        );
        assert_eq!(cached.follow_bottom, legacy.follow_bottom);
    }
}

#[test]
fn caching_does_not_change_a_single_cell_while_streaming() {
    let mut cached = test_app();
    long_transcript(&mut cached, 40);
    let mut legacy = test_app();
    long_transcript(&mut legacy, 40);
    for app in [&mut cached, &mut legacy] {
        app.in_flight_text = Some("resposta **parcial** ainda chegando…".repeat(4));
    }

    let a = buffer_of(72, 24, |f, area| draw_messages(f, &mut cached, area));
    let b = buffer_of(72, 24, |f, area| legacy_draw_messages(f, &mut legacy, area));
    assert_eq!(a, b);
}

#[test]
fn a_settled_transcript_parses_no_markdown_at_all_on_a_redraw() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = test_app();
    long_transcript(&mut app, 200);
    let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();

    crate::markdown::reset_render_calls();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let cold = crate::markdown::render_calls();
    assert!(
        cold >= 80,
        "expected one parse per assistant message: {cold}"
    );

    crate::markdown::reset_render_calls();
    for _ in 0..20 {
        terminal.draw(|f| draw(f, &mut app)).unwrap();
    }
    assert_eq!(
        crate::markdown::render_calls(),
        0,
        "20 further frames must not re-parse a single message"
    );
}

#[test]
fn expanding_a_card_rebuilds_only_that_card() {
    // The reason `expanded` is a `ChatLine` field: joining `LayoutKey`
    // would invalidate the whole memo on every `Enter`.
    let mut app = test_app();
    long_transcript(&mut app, 60);
    buffer_of(72, 24, |f, area| draw_messages(f, &mut app, area));

    app.toggle_card_focus();
    app.toggle_selected_card();
    buffer_of(72, 24, |f, area| draw_messages(f, &mut app, area));
    assert_eq!(
        app.transcript.misses(),
        1,
        "one Enter must not re-render the session"
    );
}

#[test]
fn selecting_a_card_scrolls_it_into_view() {
    let mut app = test_app();
    long_transcript(&mut app, 120);
    buffer_of(72, 24, |f, area| draw_messages(f, &mut app, area));
    let bottom = app.scroll;

    // Walk back through several cards; the viewport has to follow.
    app.toggle_card_focus();
    for _ in 0..8 {
        app.move_card_focus(false);
    }
    buffer_of(72, 24, |f, area| draw_messages(f, &mut app, area));
    assert!(
        app.scroll < bottom,
        "the cursor moved up but the viewport stayed at {bottom}"
    );

    let index = app.selected_card().unwrap();
    let (start, len) = app.transcript.entry_rows(index).unwrap();
    assert!(
        start >= app.scroll as usize && start + len <= app.scroll as usize + 24,
        "card rows {start}..{} are outside the viewport at {}",
        start + len,
        app.scroll
    );
}

#[test]
fn follow_bottom_pins_to_the_live_edge_and_a_manual_scroll_holds() {
    let mut app = test_app();
    long_transcript(&mut app, 60);

    buffer_of(72, 24, |f, area| draw_messages(f, &mut app, area));
    let bottom = app.scroll;
    assert!(
        bottom > 0,
        "a long transcript must have somewhere to scroll"
    );

    app.follow_bottom = false;
    app.scroll = bottom / 2;
    buffer_of(72, 24, |f, area| draw_messages(f, &mut app, area));
    assert_eq!(app.scroll, bottom / 2, "the redraw clobbered the offset");
    assert!(!app.follow_bottom);

    // Growing the transcript while pinned keeps the view at the new bottom.
    app.follow_bottom = true;
    app.lines
        .push(ChatLine::new(ChatRole::Assistant, "mais uma resposta"));
    buffer_of(72, 24, |f, area| draw_messages(f, &mut app, area));
    assert!(app.scroll > bottom);
}

/// The transcript's own track: it appears exactly when there is history off
/// screen, and the gutter it lives in is reserved either way so the text
/// never reflows as the content crosses the viewport height.
#[test]
fn the_transcript_shows_a_scrollbar_only_once_there_is_history_off_screen() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let short = {
        let mut app = test_app();
        app.lines
            .push(ChatLine::new(ChatRole::Assistant, "uma linha".to_string()));
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        (screen_text(&terminal), app.message_area)
    };
    assert!(
        !short.0.contains('█'),
        "a transcript that fits must not claim to scroll:\n{}",
        short.0
    );

    let long = {
        let mut app = test_app();
        long_transcript(&mut app, 30);
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        (screen_text(&terminal), app.message_area)
    };
    assert!(
        long.0.contains('█'),
        "history off screen must show a track:\n{}",
        long.0
    );

    // The pane is the same width in both — the gutter is reserved, not
    // borrowed, so nothing re-wraps when the transcript outgrows the pane.
    assert_eq!(
        short.1.width, long.1.width,
        "the text pane changed width with the scroll state"
    );
}
