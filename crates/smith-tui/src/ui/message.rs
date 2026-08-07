//! User bubbles, assistant blocks, and the transcript viewport.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, ChatLine, ChatRole};
use crate::components::{panel, wrap};
use crate::theme::Theme;

use super::card::tool_card;

/// The rows one `ChatLine` contributes to the transcript, separators
/// included, already narrowed to `width`.
///
/// Self-contained by construction — nothing here depends on a neighbouring
/// line — which is what lets `crate::transcript` memoise it per line.
pub(crate) fn render_chat_line(
    line: &ChatLine,
    theme: &Theme,
    width: u16,
    verbose: bool,
    spinner_frame: usize,
) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    match line.role() {
        ChatRole::User => {
            // Two blank lines before a user turn, one everywhere else —
            // the bubble is the transcript's chapter break.
            rows.push(Line::from(""));
            rows.push(Line::from(""));
            rows.extend(user_bubble(theme, line.text(), width));
        }
        ChatRole::Assistant => {
            rows.extend(assistant_block(theme, line.text(), line.meta(), width));
        }
        ChatRole::System => {
            rows.push(Line::from(vec![
                Span::styled(theme.separator().trim_start().to_string(), theme.disabled()),
                Span::styled(line.text().to_string(), theme.disabled()),
            ]));
        }
        ChatRole::Thought => {
            rows.push(Line::from(vec![
                Span::styled("+ ", theme.ember_bold()),
                Span::styled(format!("Thought: {}", line.text()), theme.ember()),
            ]));
        }
        ChatRole::Tool => {
            rows.extend(tool_card(theme, line, width, verbose, spinner_frame));
        }
    }
    rows.push(Line::from(""));

    // Nothing may exceed the pane width by the time it reaches the Paragraph:
    // a box row folded by `Wrap` puts its closing border on the next row and
    // breaks the frame. Wrapping here also gives System/Thought rows the line
    // breaking they never had.
    fit_lines(rows, width as usize)
}

/// The streaming reply, with the same chrome a finished one gets so the text
/// doesn't shift when the turn completes and the buffer becomes a `ChatLine`.
pub(crate) fn render_in_flight(text: &str, theme: &Theme, width: u16) -> Vec<Line<'static>> {
    fit_lines(assistant_block(theme, text, None, width), width as usize)
}

pub(super) fn draw_messages(frame: &mut Frame, app: &mut App, area: Rect) {
    // Recorded for the mouse: turning a click into a transcript row needs the
    // rect, and this is the only place that knows it.
    app.message_area = area;
    let key = crate::transcript::LayoutKey {
        width: area.width,
        verbose: app.verbose_tools,
        theme: app.theme.clone(),
    };

    app.transcript.sync(
        &app.lines,
        app.in_flight_text.as_deref(),
        &key,
        app.spinner_frame,
    );

    // The memo already knows every row's height, so the document height is a
    // lookup rather than a `Paragraph::line_count` re-measure of the whole
    // session. `fit_lines` guarantees no row is wider than the pane, so a
    // logical row is a visual row and the two agree.
    let content_height = u16::try_from(app.transcript.total_height()).unwrap_or(u16::MAX);
    let max_scroll = content_height.saturating_sub(area.height);
    if app.follow_bottom {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.follow_bottom = true;
        }
    }

    // A selection cursor you cannot see is not navigation. The memo's height
    // index already knows where every card's rows are, so this is a lookup
    // rather than a re-measure — and it runs only on the frame after a
    // selection key, never on a streaming delta.
    if std::mem::take(&mut app.scroll_to_selected) {
        if let Some(index) = app.selected_card() {
            if let Some((start, len)) = app.transcript.entry_rows(index) {
                let start = u16::try_from(start).unwrap_or(u16::MAX);
                let end = u16::try_from(start as usize + len).unwrap_or(u16::MAX);
                if start < app.scroll {
                    app.scroll = start;
                } else if end > app.scroll + area.height {
                    app.scroll = end.saturating_sub(area.height);
                }
                app.scroll = app.scroll.min(max_scroll);
                app.follow_bottom = app.scroll >= max_scroll;
            }
        }
    }

    // Virtualisation: only the viewport's rows are handed to ratatui, instead
    // of the whole document with everything above the offset thrown away.
    //
    // `Wrap` is a no-op now that `fit_lines` runs, but it stays as a safety
    // net for any future producer that forgets.
    let window = app
        .transcript
        .window(app.scroll as usize, area.height as usize);
    frame.render_widget(
        Paragraph::new(Text::from(window)).wrap(Wrap { trim: false }),
        area,
    );
}

/// The readout that sits over the bottom-right of the transcript once the
/// user has left the live edge.
///
/// Shown whenever `follow_bottom` is off, because that is exactly when "how
/// far up am I, and how do I get back" is the question. It used to appear
/// only while a turn was in flight, so scrolling through history between
/// turns had no readout at all — the scrollbar shows the *proportion* of the
/// document on screen, never which row you are on.
///
/// Drawn by the caller rather than by `draw_messages`, alongside the
/// scrollbar: both are chrome about the transcript rather than part of it,
/// and `tests::legacy_draw_messages` compares the transcript itself cell for
/// cell.
pub(super) fn draw_position_pill(frame: &mut Frame, app: &App, area: Rect) {
    if app.follow_bottom || area.width <= 20 || area.height <= 2 {
        return;
    }
    let total = app.transcript.total_height();
    let position = format!(
        " {}/{} ",
        (app.scroll as usize + area.height as usize).min(total),
        total
    );
    let label = if app.waiting_on_assistant || app.in_flight_text.is_some() {
        // A live turn outranks the bare row count: new output is arriving off
        // screen, and this is what says so.
        if app.theme.unicode {
            format!("{position}↓ new activity ")
        } else {
            format!("{position}v new activity ")
        }
    } else {
        position
    };
    let width = (label.chars().count() as u16).min(area.width);
    let pill = Rect {
        x: area.x + area.width.saturating_sub(width),
        y: area.y + area.height.saturating_sub(1),
        width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            label,
            Style::default().fg(app.theme.ember).bg(app.theme.overlay),
        )),
        pill,
    );
}

/// Gutter drawn down the left of an assistant reply: a solid bar on the first
/// row, a hairline on the continuations. Cheaper on the eye than a full box
/// and it keeps prose readable, while still marking the turn as the model's.
/// Cells the gutter occupies — both glyphs are one cell plus a space.
pub(super) const ASSISTANT_GUTTER_WIDTH: usize = 2;

/// Renders a user message inside a raised rounded box titled `You`.
///
/// The box spans the full pane width, on the same grid as the assistant's
/// text. It used to size itself to its content, which left short messages as
/// a ragged stub against the left edge — and, worse, it never wrapped: only
/// the *box* was clamped to the pane, so a long message produced rows wider
/// than the frame around them.
pub(super) fn user_bubble(theme: &Theme, text: &str, area_width: u16) -> Vec<Line<'static>> {
    let width = (area_width as usize).max(panel::box_chrome_width() + 1);
    let text_width = width - panel::box_chrome_width();

    let mut content: Vec<Line<'static>> = Vec::new();
    for raw in text.split('\n') {
        let line = Line::from(Span::styled(raw.to_string(), theme.text()));
        content.extend(wrap::wrap_line(&line, text_width));
    }
    if content.is_empty() {
        content.push(Line::from(""));
    }

    panel::themed_rounded_box_titled(
        theme,
        Some(("You", theme.ember())),
        &content,
        width,
        theme.ember(),
        theme.raised_bg(),
    )
}

/// Renders an assistant reply behind an ember gutter, with the meta caption
/// (`provider · model · 4.2s`) tucked inside it.
///
/// Markdown has to be rendered first and decorated after: `tui_markdown` has
/// no width or indent option, so the gutter is prepended to the `Vec<Line>`
/// it hands back.
pub(super) fn assistant_block(
    theme: &Theme,
    text: &str,
    meta: Option<&str>,
    area_width: u16,
) -> Vec<Line<'static>> {
    let width = (area_width as usize)
        .saturating_sub(ASSISTANT_GUTTER_WIDTH)
        .max(1);

    let mut body: Vec<Line<'static>> = Vec::new();
    for line in crate::markdown::render(text, theme) {
        body.extend(wrap::wrap_line(&line, width));
    }
    if let Some(meta) = meta {
        let line = Line::from(Span::styled(meta.to_string(), theme.disabled()));
        body.extend(wrap::wrap_line(&line, width));
    }

    body.into_iter()
        .enumerate()
        .map(|(i, line)| {
            let (first_gutter, cont_gutter) = theme.assistant_gutter();
            let (glyph, style) = if i == 0 {
                (first_gutter, theme.ember())
            } else {
                (cont_gutter, theme.disabled())
            };
            let mut spans = vec![Span::styled(glyph.to_string(), style)];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Last line of defence before the transcript reaches its `Paragraph`: wrap
/// anything still wider than the pane, so `Wrap` can never fold a row that
/// carries box geometry.
pub(super) fn fit_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return lines;
    }
    lines
        .into_iter()
        .flat_map(|line| {
            if line.width() <= width {
                vec![line]
            } else {
                wrap::wrap_line(&line, width)
            }
        })
        .collect()
}
