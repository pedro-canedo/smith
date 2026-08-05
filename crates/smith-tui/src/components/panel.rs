//! Surface primitives: background-filling helpers and rounded box builders.
//!
//! The key insight: ratatui's `Paragraph` doesn't paint background behind
//! short lines, so a panel with a raised bg needs every row padded to the
//! target width with a styled space. These helpers do that once and for all.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Pad `spans` with trailing styled spaces so the row's background covers
/// exactly `width` cells. Without this, the terminal's own bg leaks through
/// at the end of short lines and the panel looks broken.
///
/// Every span that doesn't already carry its own background also gets
/// `bg`'s background color — a surface must paint *all* of its rows, not
/// just the trailing padding, otherwise the base bg shows through the
/// content itself.
pub fn fill_line(spans: Vec<Span<'static>>, width: usize, bg: Style) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = spans
        .into_iter()
        .map(|s| {
            if s.style.bg.is_none() {
                if let Some(color) = bg.bg {
                    return Span::styled(s.content, s.style.bg(color));
                }
            }
            s
        })
        .collect();
    let used: usize = spans.iter().map(|s| s.width()).sum();
    if width > used {
        spans.push(Span::styled(" ".repeat(width - used), bg));
    }
    Line::from(spans)
}

/// Cells of padding between a box's border and its content, on each side.
const BOX_PADDING: usize = 1;

/// A horizontal rule capping a box: `left` corner, a run of `─`, `right`
/// corner. An optional label interrupts the rule just after the corner
/// (`╭─ You ────╮`), which is how a box says what it is without spending a
/// whole row on a header.
pub fn rule(
    left: &str,
    right: &str,
    label: Option<(&str, Style)>,
    width: usize,
    border: Style,
    bg: Style,
) -> Line<'static> {
    let inner = width.saturating_sub(2);
    let mut spans = vec![Span::styled(left.to_string(), border)];

    match label {
        Some((text, style)) if inner > text.chars().count() + 4 => {
            let label = format!(" {text} ");
            let label_width = label.chars().count();
            spans.push(Span::styled("─".to_string(), border));
            spans.push(Span::styled(label, style));
            spans.push(Span::styled("─".repeat(inner - label_width - 1), border));
        }
        _ => spans.push(Span::styled("─".repeat(inner), border)),
    }

    spans.push(Span::styled(right.to_string(), border));
    fill_line(spans, width, bg)
}

/// A side-bordered row: `│` on both ends, padded content in the middle.
///
/// Guarantees a result of exactly `width` cells — content wider than the box
/// is truncated rather than allowed to push the closing border out, which is
/// what used to let the transcript's `Wrap` fold a box row and break the
/// frame's geometry.
pub fn bordered_row(
    content: Vec<Span<'static>>,
    width: usize,
    border: Style,
    bg: Style,
) -> Line<'static> {
    let inner = width.saturating_sub(2);
    let text_width = inner.saturating_sub(BOX_PADDING * 2);
    let content = crate::components::wrap::truncate_spans(&content, text_width);
    let used: usize = content.iter().map(|s| s.width()).sum();

    let mut spans = vec![Span::styled("│".to_string(), border)];
    spans.push(Span::styled(" ".repeat(BOX_PADDING), bg));
    spans.extend(content);
    // Pad on the right by whatever the content left over, plus the symmetric
    // padding — without the leading pad the text sat flush against the border.
    spans.push(Span::styled(
        " ".repeat(text_width.saturating_sub(used) + BOX_PADDING),
        bg,
    ));
    spans.push(Span::styled("│".to_string(), border));
    fill_line(spans, width, bg)
}

/// Build a complete rounded box around a list of content lines. The box
/// fills the full `width` with `bg` and uses `border` for the frame.
pub fn rounded_box(
    content: &[Line<'static>],
    width: usize,
    border: Style,
    bg: Style,
) -> Vec<Line<'static>> {
    rounded_box_titled(None, content, width, border, bg)
}

/// `rounded_box` with an optional label set into the top rule.
pub fn rounded_box_titled(
    title: Option<(&str, Style)>,
    content: &[Line<'static>],
    width: usize,
    border: Style,
    bg: Style,
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(content.len() + 2);
    out.push(rule("╭", "╮", title, width, border, bg));
    for line in content {
        out.push(bordered_row(line.spans.clone(), width, border, bg));
    }
    out.push(rule("╰", "╯", None, width, border, bg));
    out
}

/// Cells consumed by a box's frame: two borders plus the padding inside them.
pub const fn box_chrome_width() -> usize {
    2 + BOX_PADDING * 2
}

/// An inset block (e.g. `$ cmd` or command output inside a tool card):
/// no border, just a solid `overlay` background with 1-cell horizontal
/// padding. Each content line is padded to `width` with `bg`.
pub fn inset(content: &[Line<'static>], width: usize, bg: Style) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2);
    content
        .iter()
        .map(|line| {
            let body = crate::components::wrap::truncate_spans(&line.spans, inner);
            let mut spans = vec![Span::styled(" ".to_string(), bg)];
            spans.extend(body);
            let used: usize = spans.iter().map(|s| s.width()).sum();
            if inner > used {
                spans.push(Span::styled(" ".repeat(inner - used), bg));
            }
            spans.push(Span::styled(" ".to_string(), bg));
            fill_line(spans, width, bg)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn fill_line_pads_to_width_with_bg() {
        let bg = Style::default().bg(Color::Black);
        let line = fill_line(vec![Span::raw("hi")], 10, bg);
        assert_eq!(line.width(), 10);
    }

    #[test]
    fn fill_line_does_not_truncate_when_already_wide() {
        let bg = Style::default().bg(Color::Black);
        let line = fill_line(vec![Span::raw("hello world")], 5, bg);
        assert_eq!(line.width(), 11);
    }

    #[test]
    fn rounded_box_has_top_bottom_and_bordered_rows() {
        let border = Style::default().fg(Color::White);
        let bg = Style::default().bg(Color::Black);
        let content = vec![Line::from("hi")];
        let lines = rounded_box(&content, 8, border, bg);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].to_string().starts_with('╭'));
        assert!(lines[2].to_string().starts_with('╰'));
    }

    #[test]
    fn rounded_box_draws_all_four_corners() {
        // The box used to be drawn open on the right: `rounded_corner` was
        // only ever called with `╭` and `╰`, and never emitted `╮`/`╯`.
        let border = Style::default().fg(Color::White);
        let bg = Style::default().bg(Color::Black);
        let lines = rounded_box(&[Line::from("hi")], 20, border, bg);
        let top = lines[0].to_string();
        let bottom = lines[lines.len() - 1].to_string();

        assert!(top.starts_with('╭'), "top: {top}");
        assert!(top.ends_with('╮'), "top: {top}");
        assert!(bottom.starts_with('╰'), "bottom: {bottom}");
        assert!(bottom.ends_with('╯'), "bottom: {bottom}");
    }

    #[test]
    fn rounded_box_rows_are_all_exactly_the_target_width() {
        let border = Style::default().fg(Color::White);
        let bg = Style::default().bg(Color::Black);
        let content = vec![
            Line::from(""),
            Line::from("short"),
            Line::from("a line far wider than the box it is being drawn inside of"),
        ];
        for line in rounded_box(&content, 30, border, bg) {
            assert_eq!(line.width(), 30, "row: {}", line);
        }
    }

    #[test]
    fn bordered_row_pads_symmetrically() {
        let border = Style::default().fg(Color::White);
        let bg = Style::default().bg(Color::Black);
        let row = bordered_row(vec![Span::raw("hi")], 20, border, bg).to_string();
        let chars: Vec<char> = row.chars().collect();

        assert_eq!(chars[0], '│');
        assert_eq!(chars[1], ' ', "content was flush against the left border");
        assert_eq!(chars[chars.len() - 1], '│');
        assert_eq!(chars[chars.len() - 2], ' ');
    }

    #[test]
    fn bordered_row_truncates_content_wider_than_the_box() {
        let border = Style::default().fg(Color::White);
        let bg = Style::default().bg(Color::Black);
        let row = bordered_row(vec![Span::raw("x".repeat(200))], 20, border, bg);
        assert_eq!(row.width(), 20);
        assert!(row.to_string().ends_with('│'));
    }

    #[test]
    fn titled_box_sets_the_label_into_the_top_rule() {
        let border = Style::default().fg(Color::White);
        let bg = Style::default().bg(Color::Black);
        let lines = rounded_box_titled(Some(("You", border)), &[Line::from("hi")], 30, border, bg);
        let top = lines[0].to_string();
        assert!(top.contains("You"), "top: {top}");
        assert_eq!(lines[0].width(), 30);
        assert!(top.ends_with('╮'), "top: {top}");
    }

    #[test]
    fn title_is_dropped_when_the_box_is_too_narrow_for_it() {
        let border = Style::default().fg(Color::White);
        let bg = Style::default().bg(Color::Black);
        let lines = rounded_box_titled(Some(("You", border)), &[Line::from("")], 6, border, bg);
        assert_eq!(lines[0].width(), 6);
        assert!(lines[0].to_string().ends_with('╮'));
    }

    #[test]
    fn inset_rows_never_exceed_the_target_width() {
        let bg = Style::default().bg(Color::Black);
        for line in inset(&[Line::from("y".repeat(120))], 24, bg) {
            assert_eq!(line.width(), 24);
        }
    }

    #[test]
    fn box_corners_land_in_the_rendered_buffer() {
        use ratatui::backend::TestBackend;
        use ratatui::widgets::Paragraph;
        use ratatui::Terminal;

        let border = Style::default().fg(Color::White);
        let bg = Style::default().bg(Color::Black);
        let lines = rounded_box(&[Line::from("hello")], 40, border, bg);
        let height = lines.len() as u16;

        let mut terminal = Terminal::new(TestBackend::new(40, height)).unwrap();
        terminal
            .draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
            .unwrap();

        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        for corner in ['╭', '╮', '╰', '╯'] {
            assert!(text.contains(corner), "missing {corner} in buffer");
        }
    }
}
