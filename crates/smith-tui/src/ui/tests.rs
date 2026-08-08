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

mod card;
mod chrome;
mod message;
mod modals;
mod sidebar;

// Fixtures, and the layout tests — those exercise ui.rs itself:
// `vertical_layout`, `page_column`, `wanted_input_rows`, the tiers.

/// Rows the input box actually *gets* once `vertical_layout` has protected
/// the transcript's floor. `draw` composes the two itself (it also has a
/// slash list and a strip to place); this is the two-step version the prompt's
/// own tests read.
fn input_height(app: &mut App, frame_area: Rect) -> u16 {
    let wanted = wanted_input_rows(app, frame_area);
    vertical_layout(frame_area.height, wanted, 0, 0, false).input
}

/// The column is a measure with margins, not a left-aligned cap.
#[test]
fn page_column_centres_what_it_caps_and_leaves_a_narrow_area_alone() {
    let wide = Rect {
        x: 3,
        y: 5,
        width: 220,
        height: 40,
    };
    let column = page_column(wide, MAX_CONTENT_WIDTH);
    assert_eq!(column.width, MAX_CONTENT_WIDTH);
    assert_eq!(column.x, 3 + (220 - MAX_CONTENT_WIDTH) / 2);
    assert_eq!(column.y, 5);
    assert_eq!(column.height, 40);

    let narrow = Rect {
        x: 0,
        y: 0,
        width: 60,
        height: 10,
    };
    let column = page_column(narrow, MAX_CONTENT_WIDTH);
    assert_eq!(column.width, 60, "never widens past the area");
    assert_eq!(column.x, 0, "and never shifts what already fits");
}

fn tool_line(name: &str, status: ActivityStatus) -> ChatLine {
    ChatLine::test_tool(
        name,
        status,
        serde_json::json!({"path": "src/main.rs"}),
        Some("file contents"),
    )
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

        // Read the pane back from the frame that just drew rather than
        // recomputing the layout here: `draw_messages` records the rect it
        // actually used, so this stays true when the layout changes.
        let pane = app.message_area;
        let pane_right = pane.x + pane.width - 1;
        let buf = terminal.backend().buffer();
        for y in pane.y..pane.y + pane.height.min(20) {
            if buf.cell(Position::new(pane.x, y)).unwrap().symbol() == "│" {
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

fn app_with_context(width: u16) -> App {
    let mut app = test_app();
    app.lines
        .push(ChatLine::new(ChatRole::Assistant, "uma resposta"));
    app.on_agent_event(smith_core::AgentEvent::ContextUsage {
        used: 79_232,
        window: 128_000,
        estimated: false,
    });
    app.tasks = vec![smith_core::Task::new(
        "fazer algo",
        smith_core::TaskStatus::Completed,
    )];
    let _ = width;
    app
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
