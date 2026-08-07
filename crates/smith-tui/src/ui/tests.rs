use super::*;
// `use super::*` reaches what `ui.rs` still names; the rest moved into sibling
// modules and is imported here by its own path.
use super::card::tool_card;
use super::chrome::QUIT_HINT;
use super::message::{assistant_block, fit_lines, user_bubble};
use crate::app::{ActivityStatus, ChatLine, ChatRole};
use crate::testkit::test_app;
use crate::theme::Theme;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

/// Rows the input box actually *gets* once `vertical_layout` has protected
/// the transcript's floor. `draw` composes the two itself (it also has a
/// slash list and a strip to place); this is the two-step version the prompt's
/// own tests read.
fn input_height(app: &mut App, frame_area: Rect) -> u16 {
    let wanted = wanted_input_rows(app, frame_area);
    vertical_layout(frame_area.height, wanted, 0, 0, false).input
}

#[test]
fn clamp_width_shrinks_a_wide_area_and_leaves_position_alone() {
    let area = Rect {
        x: 3,
        y: 5,
        width: 220,
        height: 40,
    };
    let clamped = clamp_width(area, MAX_CONTENT_WIDTH);
    assert_eq!(clamped.width, MAX_CONTENT_WIDTH);
    assert_eq!(clamped.x, 3);
    assert_eq!(clamped.y, 5);
    assert_eq!(clamped.height, 40);
}

#[test]
fn clamp_width_leaves_a_narrower_area_untouched() {
    let area = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 10,
    };
    assert_eq!(clamp_width(area, MAX_CONTENT_WIDTH).width, 60);
}

fn tool_line(name: &str, status: ActivityStatus) -> ChatLine {
    ChatLine::test_tool(
        name,
        status,
        serde_json::json!({"path": "src/main.rs"}),
        Some("file contents"),
    )
}

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
fn transcript_renders_bubble_borders_in_the_right_column() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::Terminal;

    let mut app = test_app();
    app.lines.push(ChatLine::new(
        ChatRole::User,
        "uma mensagem bem longa que precisa quebrar em varias linhas dentro da bolha".to_string(),
    ));
    app.lines.push(ChatLine::new(
        ChatRole::Assistant,
        "e a resposta do agente logo abaixo".to_string(),
    ));

    // Narrow enough that the sidebar stays hidden, so the pane is the
    // full width and the bubble must land flush against it.
    let width = 60u16;
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let buf = terminal.backend().buffer();

    let mut bubble_rows = 0;
    for y in 0..24 {
        if buf.cell(Position::new(0, y)).unwrap().symbol() == "│" {
            bubble_rows += 1;
            assert_eq!(
                buf.cell(Position::new(width - 1, y)).unwrap().symbol(),
                "│",
                "row {y} opened a bubble but never closed it"
            );
        }
    }
    assert!(bubble_rows > 0, "no bubble rows were rendered");
}

use crate::components::input::INPUT_MAX_ROWS;

fn frame(width: u16, height: u16) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

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

fn screen_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

fn app_with_long_permission_request() -> App {
    let mut app = test_app();
    let detail = (0..40)
        .map(|i| format!("echo line-{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.modal = Modal::Permission(crate::app::PermissionModal {
        request: smith_core::PermissionRequest {
            tool_call_id: "call_1".into(),
            tool_name: "run_bash".into(),
            detail,
        },
        scroll: 0,
    });
    app
}

#[test]
fn permission_modal_scrolls_through_a_long_detail_and_keeps_its_offset() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = app_with_long_permission_request();
    let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();

    let first = screen_text(&terminal);
    assert!(first.contains("line-00"), "top of the command is missing");
    assert!(
        first.contains("PgUp/PgDn"),
        "a clipped detail must say it scrolls"
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

/// The transcript renderer as it was before the memo: rebuild every row
/// from every `ChatLine`, measure the whole document, and let
/// `Paragraph::scroll` throw away everything above the offset.
///
/// Kept only as the oracle for `caching_does_not_change_a_single_cell` —
/// if the memo and this disagree on any cell, the memo is wrong.
fn legacy_draw_messages(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme.clone();
    let verbose = app.verbose_tools;
    let mut lines: Vec<Line> = Vec::new();
    for line in &app.lines {
        match line.role() {
            ChatRole::User => {
                lines.push(Line::from(""));
                lines.push(Line::from(""));
                lines.extend(user_bubble(&theme, line.text(), area.width));
            }
            ChatRole::Assistant => {
                lines.extend(assistant_block(
                    &theme,
                    line.text(),
                    line.meta(),
                    area.width,
                ));
            }
            ChatRole::System => {
                lines.push(Line::from(vec![
                    Span::styled("· ", theme.disabled()),
                    Span::styled(line.text().to_string(), theme.disabled()),
                ]));
            }
            ChatRole::Thought => {
                lines.push(Line::from(vec![
                    Span::styled("+ ", theme.ember_bold()),
                    Span::styled(format!("Thought: {}", line.text()), theme.ember()),
                ]));
            }
            ChatRole::Tool => {
                lines.extend(tool_card(
                    &theme,
                    line,
                    area.width,
                    verbose,
                    app.spinner_frame,
                ));
            }
        }
        lines.push(Line::from(""));
    }
    if let Some(text) = &app.in_flight_text {
        lines.extend(assistant_block(&theme, text, None, area.width));
    }

    let lines = fit_lines(lines, area.width as usize);
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let content_height = paragraph.line_count(area.width) as u16;
    let max_scroll = content_height.saturating_sub(area.height);
    if app.follow_bottom {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.follow_bottom = true;
        }
    }
    frame.render_widget(paragraph.scroll((app.scroll, 0)), area);
}

/// A transcript with everything the renderer has to handle: prose,
/// markdown structure that changes row counts, long unbroken tokens,
/// system/thought rows and a finished tool card.
fn long_transcript(app: &mut App, messages: usize) {
    for i in 0..messages {
        match i % 5 {
            0 => app.lines.push(ChatLine::new(
                ChatRole::User,
                format!("pergunta {i} - {}", "palavra ".repeat(12)),
            )),
            1 => app.lines.push(ChatLine::new(
                ChatRole::Assistant,
                format!("## resposta {i}\n\ncom `codigo` e **negrito**\n\n- um\n- dois"),
            )),
            2 => app.lines.push(ChatLine::new(
                ChatRole::Assistant,
                format!("| col | {i} |\n| --- | --- |\n| linha | valor bem comprido aqui |"),
            )),
            3 => app.lines.push(ChatLine::new(
                ChatRole::System,
                format!("nota de sistema {i} {}", "s".repeat(90)),
            )),
            _ => app.lines.push(ChatLine::test_tool(
                "run_bash",
                ActivityStatus::Done,
                serde_json::json!({ "command": format!("cargo test --package p{i}") }),
                Some("ok\nmais uma linha\ne outra"),
            )),
        }
    }
}

fn buffer_of(width: u16, height: u16, mut render: impl FnMut(&mut Frame, Rect)) -> Vec<String> {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let area = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render(f, area)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| format!("{}{:?}{:?}", c.symbol(), c.fg, c.bg))
        .collect()
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
fn resizing_mid_stream_never_leaves_a_row_wider_than_the_pane() {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::Terminal;

    let mut app = test_app();
    long_transcript(&mut app, 30);

    // Each resize both invalidates the memo and grows the stream, which is
    // the combination that used to be able to leave half-wrapped rows on
    // screen. Every user bubble that opens must still close in the last
    // column of the pane.
    for (i, width) in [80u16, 42, 61, 100, 38].into_iter().enumerate() {
        app.in_flight_text = Some(format!("delta {i} {}", "texto ".repeat(6 * (i + 1))));
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();

        let pane = clamp_width(
            Rect {
                x: 0,
                y: 0,
                width,
                height: 24,
            },
            MAX_CONTENT_WIDTH,
        );
        let pane_right = if width >= SIDEBAR_MIN_TERMINAL_WIDTH {
            pane.width - SIDEBAR_WIDTH - 1
        } else {
            pane.width - 1
        };
        let buf = terminal.backend().buffer();
        for y in 0..20 {
            if buf.cell(Position::new(0, y)).unwrap().symbol() == "│" {
                assert_eq!(
                    buf.cell(Position::new(pane_right, y)).unwrap().symbol(),
                    "│",
                    "width {width}, row {y}: bubble opened but never closed"
                );
            }
        }
    }
}

// --- The 80x24 contract (design-system §3) ----------------------------

#[test]
fn at_80x24_the_transcript_keeps_its_floor_whatever_else_is_open() {
    // Closed slash list: status + full prompt + the rest.
    let plain = vertical_layout(24, INPUT_MAX_ROWS, 0, 0, false);
    assert_eq!(plain.status, 1);
    assert_eq!(plain.input, INPUT_MAX_ROWS);
    assert_eq!(plain.messages, 13);

    // Slash list open — the case that used to squeeze the transcript,
    // because the prompt grew against `height - 3` and the list was
    // simply subtracted from whatever was left.
    let typing = vertical_layout(24, INPUT_MAX_ROWS, 7, 0, false);
    assert_eq!(typing.suggest, 7);
    assert_eq!(typing.messages, TRANSCRIPT_MIN_ROWS);
    assert_eq!(typing.input, 8);

    for l in [plain, typing] {
        assert_eq!(l.messages + l.strip + l.suggest + l.input + l.status, 24);
    }
}

#[test]
fn the_strip_costs_the_transcript_one_row_and_only_above_20() {
    let with = vertical_layout(24, INPUT_MIN_ROWS, 0, 0, true);
    let without = vertical_layout(24, INPUT_MIN_ROWS, 0, 0, false);
    assert_eq!(with.strip, 1);
    assert_eq!(with.messages + 1, without.messages);

    // Too short to spend a row on vitals.
    assert_eq!(vertical_layout(19, INPUT_MIN_ROWS, 0, 0, true).strip, 0);
}

#[test]
fn the_slash_list_may_borrow_from_the_floor_but_nothing_else_may() {
    // 10 rows: status 1 + prompt 3 leaves 6, less than the 8-row floor.
    let short = vertical_layout(10, INPUT_MAX_ROWS, 7, 0, false);
    assert_eq!(short.input, INPUT_MIN_ROWS, "growth never takes transcript");
    assert!(short.suggest > 0, "the completion list must stay visible");
    assert_eq!(short.messages, TRANSCRIPT_MIN_ROWS_WITH_SUGGEST);
    assert_eq!(
        short.messages + short.strip + short.suggest + short.input + short.status,
        10
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

fn app_with_context(width: u16) -> App {
    let mut app = test_app();
    app.lines
        .push(ChatLine::new(ChatRole::Assistant, "uma resposta"));
    app.on_agent_event(smith_core::AgentEvent::ContextUsage {
        used: 79_232,
        window: 128_000,
        estimated: false,
    });
    app.tasks = vec![smith_core::Task {
        content: "fazer algo".into(),
        status: smith_core::TaskStatus::Completed,
    }];
    let _ = width;
    app
}

#[test]
fn the_sidebar_carries_the_gauge_at_80_columns() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = app_with_context(80);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let text = screen_text(&terminal);

    assert!(text.contains("CONTEXT"), "{text}");
    assert!(text.contains("62% 79k/128k"), "gauge label missing: {text}");
    assert!(text.contains('━'), "gauge bar missing: {text}");
}

#[test]
fn below_80_columns_the_vitals_move_to_the_strip_instead_of_vanishing() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = app_with_context(70);
    let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let text = screen_text(&terminal);

    // The sidebar is gone…
    assert!(!text.contains("CONTEXT"), "sidebar drawn under 80: {text}");
    // …but nothing it was the only home for went with it.
    assert!(text.contains("62% 79k/128k"), "gauge lost: {text}");
    assert!(text.contains("tasks 1/1"), "task count lost: {text}");
}

#[test]
fn the_minimal_tier_drops_chrome_before_it_drops_the_gauge() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = app_with_context(40);
    let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let text = screen_text(&terminal);

    assert!(text.contains("62% 79k/128k"), "gauge lost: {text}");
    assert!(!text.contains("~/smith"), "cwd should be dropped: {text}");
    assert!(!text.contains("tasks 1/1"), "extras should be dropped");
}

#[test]
fn an_estimated_context_says_so_in_words_as_well_as_a_tilde() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = app_with_context(80);
    app.on_agent_event(smith_core::AgentEvent::ContextUsage {
        used: 79_232,
        window: 128_000,
        estimated: true,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let text = screen_text(&terminal);

    assert!(text.contains("~62% 79k/128k"), "{text}");
    assert!(
        text.contains(crate::components::gauge::ESTIMATE_LEGEND),
        "the tilde needs saying out loud: {text}"
    );
}

#[test]
fn the_gauge_never_makes_the_ui_animate() {
    // Idle cost is the whole reason the event loop skips a tick when
    // nothing is moving; a vital that ticks would undo it.
    let mut app = app_with_context(80);
    app.phase = smith_core::AgentPhase::Idle;
    app.waiting_on_assistant = false;
    assert!(!app.is_animating());
}

#[test]
fn every_row_fits_the_pane_at_80x24_in_ascii() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut app = test_app();
    app.theme = app.theme.clone().ascii_glyphs();
    long_transcript(&mut app, 60);
    app.on_agent_event(smith_core::AgentEvent::ContextUsage {
        used: 120_000,
        window: 128_000,
        estimated: true,
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    let text = screen_text(&terminal);
    assert!(
        text.is_ascii(),
        "non-ASCII bytes in 80x24 ASCII render: {text:?}"
    );
    let buf = terminal.backend().buffer();
    assert_eq!(buf.area().width, 80);
    // Every cell the transcript pane painted has to be one column wide,
    // or a folded row would have pushed a border out of the frame.
    for y in 0..24 {
        for x in 0..80 {
            let symbol = buf.cell(ratatui::layout::Position::new(x, y)).unwrap();
            assert!(
                symbol.symbol().chars().count() <= 2,
                "cell ({x},{y}) is not a single grapheme"
            );
        }
    }
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

use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn rendered(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| draw(f, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

/// A research run, driven through the events that produce one.
fn research_run(searches: &[&str]) -> App {
    let mut app = test_app();
    for (i, query) in searches.iter().enumerate() {
        app.on_agent_event(smith_core::AgentEvent::ToolCallStarted {
            id: format!("s{i}"),
            tool_name: "web_search".into(),
            input: serde_json::json!({ "query": query }),
        });
    }
    app
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

#[test]
fn the_sidebar_shows_its_tab_strip_and_switching_changes_what_is_under_it() {
    let mut app = app_with_context(100);
    let session = rendered(&mut app, 100, 30);
    assert!(session.contains("Session"), "no tab strip: {session}");
    assert!(
        session.contains("CONTEXT"),
        "the Session tab lost its gauge header: {session}"
    );

    app.sidebar_tab = crate::app::SidebarTab::Vitals;
    let vitals = rendered(&mut app, 100, 30);
    assert!(vitals.contains("COST"), "no cost section: {vitals}");
    assert!(
        !vitals.contains("CONTEXT"),
        "the Session tab's content leaked into Vitals: {vitals}"
    );
}

/// `Ctrl+B` is worth a key only if it actually hands the columns back.
#[test]
fn hiding_the_sidebar_gives_its_columns_to_the_transcript() {
    let mut app = app_with_context(100);
    assert!(rendered(&mut app, 100, 30).contains("Session"));
    app.sidebar_visible = false;
    let hidden = rendered(&mut app, 100, 30);
    assert!(!hidden.contains("Session"), "sidebar still drawn: {hidden}");
    assert!(
        !hidden.contains("CONTEXT"),
        "and no strip stood in for it: {hidden}"
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

// ---- the /model picker -----------------------------------------------------

fn picker(ids: &[(&str, &str, bool)], filter: &str, selected: usize) -> crate::app::ModelPicker {
    crate::app::ModelPicker {
        provider: "ollama".to_string(),
        all: ids
            .iter()
            .map(|(id, detail, tools)| smith_core::ModelChoice {
                id: (*id).to_string(),
                detail: (*detail).to_string(),
                supports_tools: *tools,
            })
            .collect(),
        filter: filter.to_string(),
        selected,
        scroll: 0,
    }
}

/// Renders into a real buffer and reads the cells back as rows, so a claim
/// about what is on screen is a claim about what was painted.
fn picker_rows(width: u16, height: u16, modal: &mut crate::app::ModelPicker) -> Vec<String> {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let theme = Theme::ansi();
    let area = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| draw_model_picker(frame, modal, &theme, area))
        .unwrap();
    let cells: Vec<String> = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect();
    cells
        .chunks(width as usize)
        .map(|row| row.concat())
        .collect()
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
