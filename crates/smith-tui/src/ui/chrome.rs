//! Everything framing the transcript: overlay, idle splash, strip, input, status bar.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::{App, IdleHint, Overlay, OverlayBody};
use crate::components::{panel, scrollbar};
use crate::theme::Theme;

use super::card::truncate_chars;
use super::*;

/// Queued prompts listed before the rest collapse into a "+N more" row.
pub(super) const MAX_QUEUE_ROWS: usize = 3;

/// Shown in the status bar between the two `Ctrl+C` presses that quit.
pub(super) const QUIT_HINT: &str = "press Ctrl+C again to quit";

/// A read-only panel: `/usage` and `/mcp` as a real `Table`, the `Ctrl+L` log
/// as plain rows. Scroll is clamped here rather than in `App` because this is
/// the only place that knows how many rows the panel actually got.
pub(super) fn draw_overlay(frame: &mut Frame, overlay: &mut Overlay, theme: &Theme, area: Rect) {
    let width = (area.width.saturating_mul(9) / 10)
        .clamp(30, 110)
        .min(area.width);
    let footer_rows = overlay.footer.len() as u16;
    // Borders (2) + the footer and the blank row that separates it.
    let chrome = 2 + if footer_rows > 0 { footer_rows + 1 } else { 0 };
    let wanted = (overlay.row_count() as u16).saturating_add(chrome);
    let height = wanted
        .min(area.height.saturating_mul(4) / 5)
        .max(5)
        .min(area.height);

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(theme.block_border_set())
        .padding(panel::block_padding())
        .border_style(theme.ember())
        .title(Span::styled(
            format!(" {} ", overlay.title),
            theme.ember_bold(),
        ))
        .style(theme.raised_bg());
    let inner = block.inner(popup);
    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    let [body_area, gap_area, footer_area] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(if footer_rows > 0 { 1 } else { 0 }),
        Constraint::Length(footer_rows),
    ])
    .areas(inner);
    let _ = gap_area;

    // A table spends its first row on the header, which does not scroll.
    let visible = match &overlay.body {
        OverlayBody::Table { .. } => body_area.height.saturating_sub(1),
        OverlayBody::Lines(_) => body_area.height,
    } as usize;
    let scrollable = match &overlay.body {
        OverlayBody::Table { rows, .. } => rows.len(),
        OverlayBody::Lines(lines) => lines.len(),
    };
    let max_scroll = scrollable.saturating_sub(visible) as u16;
    overlay.scroll = overlay.scroll.min(max_scroll);
    let offset = overlay.scroll as usize;

    match &overlay.body {
        OverlayBody::Table {
            columns,
            widths,
            rows,
        } => {
            let header = Row::new(
                columns
                    .iter()
                    .map(|c| Span::styled(c.clone(), theme.info_bold()))
                    .collect::<Vec<_>>(),
            );
            let body: Vec<Row> = rows
                .iter()
                .skip(offset)
                .take(visible.max(1))
                .map(|r| {
                    Row::new(
                        r.iter()
                            .map(|c| Span::styled(c.clone(), theme.text()))
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();
            let constraints: Vec<Constraint> =
                widths.iter().map(|w| Constraint::Percentage(*w)).collect();
            frame.render_widget(
                Table::new(body, constraints)
                    .header(header)
                    .column_spacing(1),
                body_area,
            );
        }
        OverlayBody::Lines(lines) => {
            let shown: Vec<Line<'static>> = lines
                .iter()
                .skip(offset)
                .take(visible.max(1))
                .map(|l| Line::from(Span::styled(l.clone(), theme.text())))
                .collect();
            frame.render_widget(Paragraph::new(shown), body_area);
        }
    }

    if footer_rows > 0 {
        let footer: Vec<Line<'static>> = overlay
            .footer
            .iter()
            .map(|f| Line::from(Span::styled(f.clone(), theme.disabled())))
            .collect();
        frame.render_widget(Paragraph::new(footer), footer_area);
    }

    // Replaces the "(scrollable)" word the footer used to carry: the track
    // says the same thing and also says *where*, and it says it beside the
    // rows it is talking about rather than under them. Measured against the
    // body's own viewport, which is a header row short of `body_area` for a
    // table.
    scrollbar::framed(
        frame,
        popup,
        scrollable as u16,
        visible as u16,
        overlay.scroll,
        theme,
    );
}

pub(super) fn draw_idle(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;

    let mut lines: Vec<Line> = app
        .banner
        .lines()
        .map(|l| Line::from(l).style(theme.ember_bold()))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "{}{}{}",
            app.provider_label,
            theme.separator(),
            app.model_label
        ),
        theme.secondary(),
    )));
    let location = match &app.git_branch {
        Some(branch) => format!("{} · git:{branch}", app.cwd_display),
        None => app.cwd_display.clone(),
    };
    lines.push(Line::from(Span::styled(location, theme.disabled())));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter send   Alt+Enter newline   Esc cancel   Ctrl+O pick a tool card   Ctrl+C ×2 quit",
        theme.disabled(),
    )));
    lines.push(Line::from(Span::styled(
        "PgUp/PgDn scroll   Home/End jump   Ctrl+B sidebar   Ctrl+L logs",
        theme.disabled(),
    )));
    if let Some(url) = &app.console_url {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("Web console  ", theme.bold()),
            Span::styled(url.clone(), theme.info()),
        ]));
    }
    lines.push(Line::from(""));

    match &app.idle_hint {
        IdleHint::Tip(tip) => {
            lines.push(Line::from(vec![
                Span::styled("● ", theme.amber()),
                Span::styled("Tip ", theme.amber().add_modifier(Modifier::BOLD)),
                Span::styled(tip.clone(), theme.disabled()),
            ]));
        }
        IdleHint::NewSession { title } => {
            lines.push(Line::from(vec![
                Span::styled("Session   ", theme.bold()),
                Span::styled(title.clone(), theme.text()),
            ]));
        }
        IdleHint::ContinueSession { title, resume_cmd } => {
            lines.push(Line::from(vec![
                Span::styled("Session   ", theme.bold()),
                Span::styled(title.clone(), theme.text()),
            ]));
            lines.push(Line::from(vec![
                Span::styled("Continue  ", theme.bold()),
                Span::styled(resume_cmd.clone(), theme.disabled()),
            ]));
        }
    }

    let height = lines.len() as u16;
    let paragraph = Paragraph::new(Text::from(lines)).alignment(Alignment::Center);
    frame.render_widget(paragraph, center_vertically(area, height));
}

/// The vitals that live in the sidebar, compressed onto one row for the
/// terminals too narrow to have one (design-system §3.2). Priority order,
/// dropped from the right as the row runs out.
pub(super) fn strip_extras(app: &App) -> String {
    use smith_core::TaskStatus;

    let sep = app.theme.separator();
    let mut parts: Vec<String> = Vec::new();
    if !app.tasks.is_empty() {
        let done = app
            .tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .count();
        parts.push(format!("tasks {done}/{}", app.tasks.len()));
    }
    if let Some(rate) = app.display_tokens_per_sec() {
        parts.push(format!("{rate:.1} tok/s"));
    }
    // The full MACHINE block is what gets dropped here; CPU is the one line of
    // it that changes a decision (is the local model thrashing or thinking).
    if let Some(stats) = &app.resources {
        parts.push(format!("CPU {:.0}%", stats.cpu_percent));
    } else if let Some((usd, _)) = app.session_cost {
        parts.push(format!("~${usd:.4}"));
    }
    parts.join(sep)
}

/// One row between the transcript and the prompt, carrying what the sidebar
/// would have carried. The gauge takes the left and keeps whatever the extras
/// leave it; below `MINIMAL_TERMINAL_WIDTH` the extras go entirely and the
/// gauge gets the whole row.
pub(super) fn draw_context_strip(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let extras = if area.width >= MINIMAL_TERMINAL_WIDTH {
        strip_extras(app)
    } else {
        String::new()
    };
    let extras_width = extras.chars().count() as u16;

    if let Some((used, window, estimated)) = app.context {
        // +1 so the bar never runs flush into the extras.
        let gauge_width = area.width.saturating_sub(extras_width + 1);
        if gauge_width >= MIN_GAUGE_WIDTH {
            frame.render_widget(
                crate::components::gauge::context_gauge(used, window, estimated, theme),
                Rect {
                    width: gauge_width,
                    ..area
                },
            );
        }
    }

    if extras_width > 0 && extras_width <= area.width {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(extras, theme.secondary()))),
            Rect {
                x: area.x + area.width - extras_width,
                width: extras_width,
                ..area
            },
        );
    }
}

/// Prompts typed while the agent was busy, waiting their turn.
///
/// Drawn as its own region rather than as transcript lines: they have not been
/// sent, and a queued message rendered as a user bubble would claim the agent
/// has seen something it has not.
pub(super) fn draw_queue(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let w = area.width as usize;
    let marker = if theme.unicode { "\u{21b3} " } else { "> " };

    let mut lines: Vec<Line> = app
        .queued
        .iter()
        .take(MAX_QUEUE_ROWS)
        .map(|text| {
            let one_line = text.replace('\n', " ");
            Line::from(vec![
                Span::styled(marker.to_string(), theme.warning()),
                Span::styled(
                    truncate_chars(&one_line, w.saturating_sub(4)),
                    theme.secondary(),
                ),
            ])
        })
        .collect();

    if app.queued.len() > MAX_QUEUE_ROWS {
        lines.push(Line::from(Span::styled(
            format!("  +{} more queued", app.queued.len() - MAX_QUEUE_ROWS),
            theme.disabled(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(
                "  {} waiting — the agent picks it up between steps  \u{b7}  /queue clear",
                app.queued.len()
            ),
            theme.disabled(),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

pub(super) fn draw_slash_suggestions(
    frame: &mut Frame,
    app: &App,
    suggestions: &[crate::slash::SlashSuggestion],
    area: Rect,
) {
    let theme = &app.theme;
    let prefix = app.completion_kind.prefix();
    // A path is the whole entry and carries no description, so padding it to
    // a command's column width would just push every row right.
    let name_width = match app.completion_kind {
        crate::complete::CompletionKind::Slash => 12,
        crate::complete::CompletionKind::File => 0,
    };

    let [list_area, hint_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    // Every suggestion becomes an item, not just the first windowful, and
    // `ListState` scrolls to keep the selected one on screen. Drawing a fixed
    // six and letting the cursor run past them meant that from the seventh
    // match on, the highlighted row was off screen: nothing looked selected,
    // and Enter accepted something the user could not see.
    let items: Vec<ListItem> = suggestions
        .iter()
        .map(|s| {
            ListItem::new(Line::from(Span::styled(
                format!("{prefix}{:<name_width$} {}", s.name, s.description),
                theme.secondary(),
            )))
        })
        .collect();
    let marker = format!("{} ", theme.marker_selected());
    let list = List::new(items)
        .highlight_style(theme.info_bold().bg(theme.hover))
        .highlight_symbol(marker.as_str());
    let mut state = ListState::default().with_selected(Some(app.slash_selected));
    frame.render_stateful_widget(list, list_area, &mut state);

    let hint = if theme.unicode {
        "  Tab complete · ↑↓ select · Enter accept"
    } else {
        "  Tab complete | up/down select | Enter accept"
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, theme.disabled()))),
        hint_area,
    );
}

pub(super) fn draw_input(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme.clone();
    let plan = app.in_plan_mode();
    let border = if plan {
        theme.plan()
    } else if matches!(app.phase, smith_core::AgentPhase::WaitingPermission) {
        theme.warning()
    } else {
        theme.disabled()
    };

    let model_label = format!("{} ", app.model_label);
    let title_width = " smith ".len() + model_label.chars().count();
    let pad = (area.width as usize).saturating_sub(title_width + 2).max(1);
    let title = Line::from(vec![
        Span::styled(" smith ", theme.ember_bold().bg(theme.overlay)),
        Span::styled(" ".repeat(pad), border),
        Span::styled(model_label, theme.disabled()),
    ]);

    app.input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_set(theme.block_border_set())
            .padding(panel::block_padding())
            .title(title)
            .border_style(border),
    );
    app.input.set_placeholder(if plan {
        if theme.unicode {
            "Plan mode — approve the plan modal or /plan reject…"
        } else {
            "Plan mode - approve the plan modal or /plan reject..."
        }
    } else if theme.unicode {
        "Ask anything…  type / for commands"
    } else {
        "Ask anything...  type / for commands"
    });
    app.input.set_style(if app.input.text().starts_with('/') {
        theme.info()
    } else {
        theme.text()
    });

    // The widget soft-wraps, grows and scrolls itself; all we do is hand it
    // the area and then place the real terminal caret where it rendered.
    frame.render_widget(app.input.widget(), area);

    // A modal owns the keyboard while it's open, so the caret must not sit in
    // a box the user isn't typing into. ratatui hides it when we don't set it.
    if app.modal.is_none() {
        if let Some(position) = app.input.cursor_position() {
            frame.set_cursor_position(position);
        }
    }
}

/// Footer status bar on a raised surface: cwd+branch left, live phase +
/// spinner + elapsed in the middle while the agent works, version right.
pub(super) fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let bg = theme.raised_bg();
    let w = area.width as usize;

    // Minimal tier (design-system §3.1): the location and the version are
    // context, not state — on a 40-column terminal they crowd out the one
    // thing the bar exists to say, which is what the agent is doing right now.
    let minimal = area.width < MINIMAL_TERMINAL_WIDTH;
    let left = match (&app.git_branch, minimal) {
        (_, true) => String::new(),
        (Some(branch), false) => format!(" {} git:({}) ", app.cwd_display, branch),
        (None, false) => format!(" {} ", app.cwd_display),
    };
    let right = if minimal {
        String::new()
    } else {
        format!(" smith {} ", env!("CARGO_PKG_VERSION"))
    };

    let busy = !matches!(app.phase, smith_core::AgentPhase::Idle)
        || app.waiting_on_assistant
        || app.modal.is_some();
    // The armed-quit hint outranks the phase readout: it is the only thing
    // on screen telling the user what their next keystroke will do.
    let center = if app.quit_pending() {
        Some(QUIT_HINT.to_string())
    } else if app.selected_card().is_some() {
        // Above the busy readout on purpose: card focus is a mode the user
        // switched into, and while it is held `Enter` no longer submits. That
        // a turn is running is already visible from the cards themselves.
        let sep = theme.separator();
        Some(format!("up/down card{sep}Enter expand{sep}Esc back"))
    } else if busy {
        // A spinner claims work is in progress. While the agent is blocked on
        // the user — a permission prompt, a question — nothing is in progress,
        // and a frozen spinner would be worse than none: it reads as a hang.
        // `is_animating` is the same predicate the event loop wakes on, so the
        // glyph and the redraws cannot disagree about whether anything moves.
        let marker = if app.is_animating() {
            let frames = theme.spinner_frames();
            format!("{} ", frames[app.spinner_frame % frames.len()])
        } else {
            String::new()
        };
        let mut s = format!("{marker}{}", app.phase_label());
        if let Some((iteration, max_iterations)) = app.loop_progress {
            s.push_str(&format!(
                "{}{iteration}/{max_iterations}",
                theme.separator()
            ));
        }
        if let Some(secs) = app.turn_elapsed_secs() {
            s.push_str(&format!("{}{secs:.0}s", theme.separator()));
        }
        if let Some(tokens) = app.live_output_tokens_estimate() {
            if tokens > 0 {
                s.push_str(&format!("{}~{tokens} tok", theme.separator()));
            }
        }
        Some(s)
    } else {
        None
    };

    let mut spans = vec![Span::styled(left.clone(), theme.secondary())];
    match center {
        Some(c) => {
            let left_w = left.chars().count();
            let center_w = c.chars().count();
            let right_w = right.chars().count();
            let remaining = w.saturating_sub(left_w + center_w + right_w);
            let gap1 = remaining / 2;
            let gap2 = remaining.saturating_sub(gap1);
            let center_style = if app.quit_pending() {
                theme.warning()
            } else {
                theme.ember()
            };
            spans.push(Span::styled(" ".repeat(gap1), bg));
            spans.push(Span::styled(c, center_style));
            spans.push(Span::styled(" ".repeat(gap2), bg));
            spans.push(Span::styled(right, theme.disabled()));
        }
        None => {
            spans.push(Span::styled(right, theme.disabled()));
        }
    }
    frame.render_widget(Paragraph::new(panel::fill_line(spans, w, bg)), area);
}
