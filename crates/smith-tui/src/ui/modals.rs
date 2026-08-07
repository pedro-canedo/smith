//! The four modal drawers.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::components::{chips, panel, scrollbar};
use crate::theme::Theme;

use super::*;

/// Floor for the permission popup: header, detail, key row and both borders.
pub(super) const PERMISSION_MODAL_MIN_HEIGHT: u16 = 8;

/// Outer height a bordered, padded `paragraph` needs at `outer_width`.
///
/// [`Paragraph::line_count`] wraps at exactly the width it is handed and adds
/// only the block's *vertical* chrome — measured, not assumed: with
/// `Borders::ALL` plus `Padding::horizontal(1)` it returns the same count as
/// with the borders alone, so the horizontal half is entirely the caller's
/// problem. Handing it the outer width therefore under-counts every line that
/// wraps in the last columns, and an under-counted height is an under-counted
/// `max_scroll` — a modal whose final rows cannot be scrolled to at all. That
/// is the failure this function exists to make impossible to repeat.
fn framed_height(paragraph: &Paragraph, outer_width: u16) -> u16 {
    paragraph.line_count(framed_content_width(outer_width)) as u16
}

/// The width text actually wraps at inside a framed, padded box.
fn framed_content_width(outer_width: u16) -> u16 {
    outer_width
        .saturating_sub(panel::framed_chrome_width())
        .max(1)
}

pub(super) fn draw_permission_modal(
    frame: &mut Frame,
    modal: &mut crate::app::PermissionModal,
    theme: &Theme,
    area: Rect,
) {
    let request = &modal.request;
    let width = area.width.saturating_sub(8).clamp(36, 72);
    let max_height = (area.height.saturating_mul(4) / 5).max(PERMISSION_MODAL_MIN_HEIGHT);

    let mut key_row = chips::confirm_hint("y", "allow once", theme);
    key_row.extend(chips::confirm_hint("a", "allow session", theme));
    key_row.extend(chips::cancel_hint("n", "deny", theme));

    let mut body_lines = vec![
        Line::from(vec![
            Span::styled("tool: ", theme.disabled()),
            Span::styled(
                request.tool_name.clone(),
                theme.warning().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
    ];
    // A `run_bash` detail is routinely multi-line; as one `Line` its newlines
    // are not row breaks at all, so the command the user is approving came
    // out mangled on a single row.
    body_lines.extend(
        request
            .detail
            .lines()
            .map(|l| Line::from(Span::styled(l.to_string(), theme.text()))),
    );
    body_lines.push(Line::from(""));
    body_lines.push(Line::from(key_row));

    let block = || {
        Block::default()
            .borders(Borders::ALL)
            .border_set(theme.block_border_set())
            .padding(panel::block_padding())
            .title(Span::styled(
                " permission requested ",
                theme.warning().add_modifier(Modifier::BOLD),
            ))
            .border_style(theme.warning())
    };
    let paragraph = Paragraph::new(Text::from(body_lines))
        .style(theme.raised_bg())
        .block(block())
        .wrap(Wrap { trim: false });

    let content_height = framed_height(&paragraph, width);
    let height = content_height
        .min(max_height)
        .max(PERMISSION_MODAL_MIN_HEIGHT)
        .min(area.height.max(1));
    let max_scroll = content_height.saturating_sub(height);
    modal.scroll = modal.scroll.min(max_scroll);

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph.block(block()).scroll((modal.scroll, 0)), popup);
    // The scrollbar replaces the bottom-title hint: a track that shows both
    // that there is more and *where* in it you are says strictly more than
    // the words "PgUp/PgDn scroll", in the same zero rows.
    scrollbar::vertical(frame, popup, content_height, height, modal.scroll, theme);
}

pub(super) fn draw_plan_modal(
    frame: &mut Frame,
    modal: &mut crate::app::PlanModal,
    theme: &Theme,
    area: Rect,
) {
    let width = (area.width.saturating_mul(4) / 5).clamp(40, 100);
    let max_height = (area.height.saturating_mul(4) / 5).max(12);

    let mut key_row = chips::confirm_hint("y/Enter", "build", theme);
    key_row.extend(chips::cancel_hint("n/Esc", "reject", theme));

    let mut body_lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "Review the plan, then build or reject.",
            theme.disabled(),
        )),
        Line::from(""),
    ];
    body_lines.extend(crate::markdown::render(&modal.text, theme));
    body_lines.push(Line::from(""));
    body_lines.push(Line::from(key_row));

    let paragraph = Paragraph::new(Text::from(body_lines))
        .style(theme.raised_bg())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(theme.block_border_set())
                .padding(panel::block_padding())
                .title(Span::styled(" plan ready ", theme.plan_bold()))
                .border_style(theme.plan()),
        )
        .wrap(Wrap { trim: false });

    // Measured, not counted from `body_lines`: a plan is markdown, and its
    // long rows wrap. Counting the unwrapped lines made a wrapped plan claim
    // to be shorter than it renders, which is the same under-counted
    // `max_scroll` `framed_height` exists to prevent.
    let content_height = framed_height(&paragraph, width);
    let height = content_height
        .min(max_height)
        .max(10)
        .min(area.height.max(1));
    let max_scroll = content_height.saturating_sub(height);
    modal.scroll = modal.scroll.min(max_scroll);

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph.scroll((modal.scroll, 0)), popup);
    scrollbar::vertical(frame, popup, content_height, height, modal.scroll, theme);
}

pub(super) fn draw_question_modal(
    frame: &mut Frame,
    modal: &mut crate::app::QuestionModal,
    theme: &Theme,
    area: Rect,
) {
    let width = area.width.saturating_sub(8).clamp(40, 72);
    let q = &modal.question;

    let mut body_lines = vec![
        Line::from(Span::styled(q.prompt.clone(), theme.text())),
        Line::from(""),
    ];
    for (i, opt) in q.options.iter().enumerate() {
        let selected = modal.selected == i;
        let marker = if selected {
            theme.marker_selected()
        } else {
            " "
        };
        let spans = vec![Span::styled(
            format!("{marker} {}. {opt}", i + 1),
            if selected {
                theme.info_bold()
            } else {
                theme.secondary()
            },
        )];
        body_lines.push(if selected {
            panel::fill_line(spans, width.saturating_sub(2) as usize, theme.hover_bg())
        } else {
            Line::from(spans)
        });
    }
    body_lines.push(Line::from(""));
    let custom_marker = if modal.selected == 3 {
        theme.marker_selected()
    } else {
        " "
    };
    let custom_display = if modal.custom.is_empty() {
        theme.ellipsis().to_string()
    } else {
        modal.custom.clone()
    };
    let custom_spans = vec![Span::styled(
        format!("{custom_marker} 4. Other: {custom_display}"),
        if modal.selected == 3 {
            theme.info_bold()
        } else {
            theme.secondary()
        },
    )];
    body_lines.push(if modal.selected == 3 {
        panel::fill_line(
            custom_spans,
            width.saturating_sub(2) as usize,
            theme.hover_bg(),
        )
    } else {
        Line::from(custom_spans)
    });
    body_lines.push(Line::from(""));

    let mut key_row = chips::confirm_hint("1-3", "pick", theme);
    key_row.extend(chips::info_hint("4", "type", theme));
    key_row.extend(chips::confirm_hint("Enter", "submit", theme));
    key_row.extend(chips::cancel_hint("Esc", "dismiss", theme));
    body_lines.push(Line::from(key_row));

    let paragraph = Paragraph::new(Text::from(body_lines))
        .style(theme.raised_bg())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(theme.block_border_set())
                .padding(panel::block_padding())
                .title(Span::styled(" question ", theme.info_bold()))
                .border_style(theme.info()),
        )
        .wrap(Wrap { trim: false });

    // Measured rather than counted: this modal does not scroll, so a long
    // prompt that wraps has to be given the rows it wraps into or its last
    // option is simply not on screen — and the options are the whole point.
    let height = framed_height(&paragraph, width)
        .clamp(12, 20)
        .min(area.height.max(1));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

/// The `/model` picker: a filter line, a scrolling list, and a hint row.
///
/// Sized to the terminal rather than to the list — a gateway can offer dozens,
/// and a modal that grows past the frame is one that cannot be read at the
/// bottom.
pub(super) fn draw_model_picker(
    frame: &mut Frame,
    modal: &mut crate::app::ModelPicker,
    theme: &Theme,
    area: Rect,
) {
    let outer = clamp_width(area, 76.min(area.width));
    let width = outer.width as usize - panel::box_chrome_width();
    let rows = area.height.saturating_sub(9).clamp(3, 14) as usize;
    modal.clamp(rows);

    let matches = modal.matches();
    let mut lines: Vec<Line<'static>> = Vec::new();

    let (filter, filter_style) = if modal.filter.is_empty() {
        ("type to filter".to_string(), theme.disabled())
    } else {
        (modal.filter.clone(), theme.text())
    };
    lines.push(panel::fill_line(
        vec![Span::styled(filter, filter_style)],
        width,
        theme.overlay_bg(),
    ));
    lines.push(panel::fill_line(Vec::new(), width, theme.raised_bg()));

    if matches.is_empty() {
        lines.push(panel::fill_line(
            vec![Span::styled("nothing matches", theme.disabled())],
            width,
            theme.raised_bg(),
        ));
    }
    for (offset, model) in matches.iter().enumerate().skip(modal.scroll).take(rows) {
        let picked = offset == modal.selected;
        let bg = if picked {
            theme.hover_bg()
        } else {
            theme.raised_bg()
        };
        let mut spans = vec![
            Span::styled(
                format!("{} ", if picked { theme.marker_selected() } else { " " }),
                theme.ember(),
            ),
            Span::styled(
                model.id.clone(),
                if picked { theme.bold() } else { theme.text() },
            ),
        ];
        if !model.detail.is_empty() {
            spans.push(Span::styled(
                format!("  {}", model.detail),
                theme.disabled(),
            ));
        }
        if !model.supports_tools {
            // Loudest thing on the row: picking it produces an agent that
            // cannot read a file, and the failure arrives a turn later.
            spans.push(Span::styled("  NO TOOLS", theme.danger()));
        }
        lines.push(panel::fill_line(spans, width, bg));
    }

    lines.push(panel::fill_line(Vec::new(), width, theme.raised_bg()));
    let mut hint = vec![Span::styled(
        format!("{} of {}   ", modal.selected + 1, matches.len().max(1)),
        theme.disabled(),
    )];
    hint.extend(chips::key_hint("Enter", "switch", theme.success, theme));
    hint.extend(chips::cancel_hint("Esc", "cancel", theme));
    lines.push(panel::fill_line(hint, width, theme.raised_bg()));

    let title = format!(" {} models ", modal.provider);
    let boxed = panel::themed_rounded_box_titled(
        theme,
        Some((title.as_str(), theme.ember_bold())),
        &lines,
        width,
        theme.ember(),
        theme.raised_bg(),
    );
    let height = boxed.len() as u16;
    let rect = center_vertically(outer, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(Text::from(boxed)), rect);
}
