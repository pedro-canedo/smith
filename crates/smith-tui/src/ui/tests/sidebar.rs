//! The right-hand pane and its tabs.

use super::*;

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

    // Narrow enough that the sidebar stays hidden, so the pane is the whole
    // width bar the scrollbar gutter, and the bubble must land flush against
    // that edge — the gutter is reserved whether or not a bar is drawn in it,
    // so the pane's right edge does not move with the scroll state.
    let width = 60u16;
    let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
    terminal.draw(|f| draw(f, &mut app)).unwrap();
    // Scoped to the transcript's own rect, which `draw_messages` records.
    // Scanning the whole screen used to work only because the input box
    // happened to close in the same column; it spans the full width, while
    // the transcript stops short of the scrollbar gutter, so a screen-wide
    // scan now tests two different panes against one edge.
    let pane = app.message_area;
    let last_text_column = pane.x + pane.width - 1;
    let buf = terminal.backend().buffer();

    let mut bubble_rows = 0;
    for y in pane.y..pane.y + pane.height {
        if buf.cell(Position::new(pane.x, y)).unwrap().symbol() == "│" {
            bubble_rows += 1;
            assert_eq!(
                buf.cell(Position::new(last_text_column, y))
                    .unwrap()
                    .symbol(),
                "│",
                "row {y} opened a bubble but never closed it"
            );
        }
    }
    assert!(bubble_rows > 0, "no bubble rows were rendered");
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
