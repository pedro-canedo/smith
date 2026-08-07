//! The right-hand pane and its three tabs.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline, Tabs};
use ratatui::Frame;

use crate::app::{App, SidebarTab};
use crate::theme::Theme;

use super::message::fit_lines;

/// The sidebar: a tab strip, then the active tab's content, split around the
/// one widget in it that isn't a `Line`.
///
/// `above` ends with the `CONTEXT` header, the gauge goes in the row after it,
/// and `below` is everything else. Both halves go through `fit_lines` instead
/// of relying on `Wrap`: the gauge is positioned by row offset, so the line
/// count has to equal the row count exactly — a single wrapped line would
/// slide the gauge off its header.
pub(super) fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_set(theme.block_border_set())
        .border_style(theme.disabled());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;

    let [tabs_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    frame.render_widget(sidebar_tabs(app, tabs_area.width), tabs_area);

    let (above, below) = sidebar_lines(app);
    let above = fit_lines(above, width);
    let below = fit_lines(below, width);

    // The one widget slot, whose occupant depends on the tab: the context
    // gauge on Session, the throughput graph on Vitals, nothing on Tasks.
    let widget_rows = match app.sidebar_tab {
        SidebarTab::Session => u16::from(app.context.is_some()),
        SidebarTab::Vitals => sparkline_rows(app),
        SidebarTab::Tasks => 0,
    };
    let [above_area, widget_area, below_area] = Layout::vertical([
        Constraint::Length(above.len() as u16),
        Constraint::Length(widget_rows),
        Constraint::Min(0),
    ])
    .areas(body_area);

    frame.render_widget(Paragraph::new(above), above_area);
    if widget_area.height > 0 {
        match app.sidebar_tab {
            SidebarTab::Session => {
                if let Some((used, window, estimated)) = app.context {
                    frame.render_widget(
                        crate::components::gauge::context_gauge(used, window, estimated, theme),
                        widget_area,
                    );
                }
            }
            SidebarTab::Vitals => {
                // The *last* pane-width samples, not the first. `Sparkline`
                // renders `data.iter().take(area.width)`, so handing it the
                // whole series would pin the graph to the opening seconds of
                // the turn and never move again — a live readout frozen on
                // history, which is worse than no readout.
                let series = app.metrics.throughput();
                let skip = series.len().saturating_sub(widget_area.width as usize);
                let data: Vec<u64> = series.iter().skip(skip).copied().collect();
                frame.render_widget(
                    Sparkline::default()
                        .data(&data)
                        .style(theme.ember())
                        .absent_value_style(theme.disabled()),
                    widget_area,
                );
            }
            SidebarTab::Tasks => {}
        }
    }
    frame.render_widget(Paragraph::new(below), below_area);
}

/// The tab strip. Titles are abbreviated to their first four characters when
/// the pane is too narrow for the full set — a clipped `Tabs` drops the last
/// title entirely, which would make a tab the user can select but not see.
pub(super) fn sidebar_tabs(app: &App, width: u16) -> Tabs<'static> {
    let theme = &app.theme;
    // `theme.separator()` is ` · ` — already padded, and with ratatui's own
    // space on each side it costs five columns per divider. Three tabs would
    // then need 28 in a pane that has 27, and every title would be clipped to
    // four characters for the sake of two spaces. The bare rule glyph costs
    // three, which is what makes the full titles fit.
    let divider = theme.border_vertical();
    let divider_cost = divider.chars().count() + 2;
    let full: usize = SidebarTab::ALL
        .iter()
        .map(|t| t.title().chars().count())
        .sum::<usize>()
        + divider_cost * (SidebarTab::ALL.len() - 1);
    let short = full > width as usize;

    let titles: Vec<String> = SidebarTab::ALL
        .iter()
        .map(|t| {
            if short {
                t.title().chars().take(4).collect()
            } else {
                t.title().to_string()
            }
        })
        .collect();

    Tabs::new(titles)
        .select(app.sidebar_tab.index())
        .style(theme.disabled())
        .highlight_style(theme.ember_bold())
        .divider(divider)
        // ratatui pads every title with a space on each side by default, which
        // is six more columns than a 27-column pane can spare.
        .padding("", "")
}

/// Sidebar content for the active tab, split at the row the context gauge
/// occupies. Only the `Session` tab has a gauge, so for the other two the
/// first half is empty and everything lands in the second.
pub(super) fn sidebar_lines(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    match app.sidebar_tab {
        SidebarTab::Session => session_tab_lines(app),
        SidebarTab::Tasks => (Vec::new(), tasks_tab_lines(app)),
        SidebarTab::Vitals => vitals_tab_lines(app),
    }
}

pub(super) fn sidebar_head(theme: &Theme, s: &str) -> Line<'static> {
    Line::from(Span::styled(
        s.to_string(),
        theme.secondary().add_modifier(Modifier::BOLD),
    ))
}

pub(super) fn session_tab_lines(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let theme = &app.theme;
    let total_tokens = app.usage.input_tokens + app.usage.output_tokens;

    let mut lines = vec![Line::from(Span::styled(
        format!(
            "{}{}{}",
            app.provider_label,
            theme.separator(),
            app.model_label
        ),
        theme.text(),
    ))];

    if app.in_plan_mode() {
        lines.push(Line::from(Span::styled("PLAN MODE", theme.plan_bold())));
    }

    if !matches!(app.phase, smith_core::AgentPhase::Idle) || app.waiting_on_assistant {
        let phase_style = match app.phase {
            smith_core::AgentPhase::Planning | smith_core::AgentPhase::Building => theme.plan(),
            smith_core::AgentPhase::Asking => theme.info(),
            smith_core::AgentPhase::WaitingPermission => theme.warning(),
            smith_core::AgentPhase::Idle => theme.disabled(),
            _ => theme.ember(),
        };
        lines.push(Line::from(Span::styled(
            app.phase_label().to_string(),
            phase_style,
        )));
    }

    // A count, not the checklist: the list itself has its own tab now, and a
    // long one used to be what pushed `CONTEXT` off the bottom of the pane.
    if !app.tasks.is_empty() {
        lines.push(Line::from(""));
        lines.push(task_summary_line(&app.tasks, theme));
    }

    lines.extend([Line::from(""), sidebar_head(theme, "CONTEXT")]);

    // --- the gauge's row goes here ---

    let mut below: Vec<Line<'static>> = Vec::new();
    if app.context.is_some_and(|(_, _, estimated)| estimated) {
        // A tilde alone doesn't say *what* is estimated, and the difference
        // between "162k" and "roughly 162k" is the whole point of the field.
        below.push(Line::from(Span::styled(
            crate::components::gauge::ESTIMATE_LEGEND,
            theme.disabled(),
        )));
    }
    below.push(Line::from(Span::styled(
        format!("{total_tokens} tokens"),
        theme.text(),
    )));
    below.push(Line::from(Span::styled(
        format!(
            "{} in / {} out",
            app.usage.input_tokens, app.usage.output_tokens
        ),
        theme.secondary(),
    )));

    if let Some(rate) = app.display_tokens_per_sec() {
        below.push(Line::from(Span::styled(
            format!("{rate:.1} tok/s"),
            theme.secondary(),
        )));
    }

    // The console link, visible mid-session — the idle splash carries it too,
    // but the splash is gone the moment the first message lands. `fit_lines`
    // wraps the URL over a few rows; the token has to be copyable, so
    // truncating it would show a link that does not work.
    if let Some(url) = &app.console_url {
        below.push(Line::from(""));
        below.push(sidebar_head(theme, "WEB CONSOLE"));
        below.push(Line::from(Span::styled(url.clone(), theme.info())));
    }

    (lines, below)
}

pub(super) fn tasks_tab_lines(app: &App) -> Vec<Line<'static>> {
    let theme = &app.theme;
    if app.tasks.is_empty() {
        return vec![Line::from(Span::styled("no tasks yet", theme.disabled()))];
    }
    task_lines(&app.tasks, theme)
}

/// Vitals, split around the throughput sparkline the way the session tab is
/// split around its gauge: `above` ends on the `THROUGHPUT` header, the graph
/// occupies the rows after it, and everything else follows.
pub(super) fn vitals_tab_lines(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    let theme = &app.theme;
    let mut above: Vec<Line<'static>> = Vec::new();

    if let Some(stats) = &app.resources {
        above.extend(resource_lines(stats, theme));
        above.push(Line::from(""));
    }
    if sparkline_rows(app) > 0 {
        above.push(sidebar_head(theme, "THROUGHPUT"));
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    if sparkline_rows(app) > 0 {
        let series = app.metrics.throughput();
        let latest = series.back().copied().unwrap_or(0);
        let peak = series.iter().copied().max().unwrap_or(0);
        lines.push(Line::from(Span::styled(
            format!("{latest} tok/s   peak {peak}"),
            theme.disabled(),
        )));
        lines.push(Line::from(""));
    }

    lines.push(sidebar_head(theme, "COST"));
    match app.session_cost {
        Some((usd, unpriced)) => {
            lines.push(Line::from(Span::styled(
                format!("~${usd:.4} (est.)"),
                theme.text(),
            )));
            // "$0.00" and "we have no price for this model" are different
            // claims, and only one of them is about money.
            if unpriced > 0 {
                lines.push(Line::from(Span::styled(
                    format!("+{unpriced} turns unpriced"),
                    theme.warning(),
                )));
            }
        }
        None => lines.push(Line::from(Span::styled("n/a", theme.disabled()))),
    }
    lines.push(Line::from(Span::styled(
        format!("{} requests", app.request_count),
        theme.secondary(),
    )));
    lines.push(Line::from(Span::styled(
        format!("{} tool calls", app.tool_call_count),
        theme.secondary(),
    )));

    (above, lines)
}

/// Rows the throughput graph gets, or zero when there is nothing to draw.
///
/// Two samples, not one: a single reading drawn as a sparkline is one full
/// bar, which looks like a measurement of something rather than the absence
/// of a series. And two rows rather than one, because a one-row sparkline
/// quantises every value to eight steps of a single cell — enough to show a
/// stall, not enough to show a slope.
pub(super) fn sparkline_rows(app: &App) -> u16 {
    if app.metrics.throughput().len() < 2 {
        0
    } else {
        2
    }
}

/// Checklist section for the sidebar: a "3/8" count, a filled/unfilled
/// progress bar, then the not-yet-done tasks (completed ones collapse into
/// a trailing "+N completed" line so a long-running session's history
/// doesn't push the rest of the sidebar off-screen).
pub(super) fn task_lines(tasks: &[smith_core::Task], theme: &Theme) -> Vec<Line<'static>> {
    use smith_core::TaskStatus;

    // The checklist owns a whole tab now, so it can show far more than the
    // six rows it got when every section shared one column.
    const MAX_SHOWN: usize = 20;
    const BAR_WIDTH: usize = 20;

    let total = tasks.len();
    let done = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .count();

    let mut lines = vec![task_summary_line(tasks, theme)];

    // Progress bar — filled segment ember, unfilled disabled.
    if let Some(bar) = progress_bar_line(done, total, BAR_WIDTH, theme) {
        lines.push(bar);
    }

    let pending: Vec<&smith_core::Task> = tasks
        .iter()
        .filter(|t| t.status != TaskStatus::Completed)
        .collect();
    for task in pending.iter().take(MAX_SHOWN) {
        let (icon, style) = match task.status {
            TaskStatus::InProgress if theme.unicode => ("▶", theme.ember()),
            TaskStatus::InProgress => (">", theme.ember()),
            _ if theme.unicode => ("◻", theme.disabled()),
            _ => ("-", theme.disabled()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{icon} "), style),
            Span::styled(sidebar_truncate(&task.content), theme.text()),
        ]));
    }
    if pending.len() > MAX_SHOWN {
        lines.push(Line::from(Span::styled(
            format!("{} +{} more", theme.ellipsis(), pending.len() - MAX_SHOWN),
            theme.disabled(),
        )));
    }
    if done > 0 {
        lines.push(Line::from(vec![
            Span::styled(if theme.unicode { "✔ " } else { "+ " }, theme.success()),
            Span::styled(format!("{done} completed"), theme.secondary()),
        ]));
    }
    lines
}

/// `TASKS 3/8` — the checklist compressed to the one row the `Session` tab
/// can spare, and the header row of the `Tasks` tab.
pub(super) fn task_summary_line(tasks: &[smith_core::Task], theme: &Theme) -> Line<'static> {
    let done = tasks
        .iter()
        .filter(|t| t.status == smith_core::TaskStatus::Completed)
        .count();
    Line::from(vec![
        Span::styled("TASKS", theme.secondary().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {done}/{}", tasks.len()), theme.disabled()),
    ])
}

/// Filled/unfilled progress bar for the task checklist; `None` when there
/// are no tasks. Split out so the `total == 0` guard is an early return
/// rather than a `total > 0` wrapper around the division (clippy
/// `manual_checked_ops`).
pub(super) fn progress_bar_line(
    done: usize,
    total: usize,
    bar_width: usize,
    theme: &Theme,
) -> Option<Line<'static>> {
    if total == 0 {
        return None;
    }
    let filled = (done * bar_width) / total;
    let pct = (done * 100) / total;
    Some(Line::from(vec![
        Span::styled(
            (if theme.unicode { "▰" } else { "#" }).repeat(filled),
            theme.ember(),
        ),
        Span::styled(
            (if theme.unicode { "▱" } else { "-" }).repeat(bar_width.saturating_sub(filled)),
            theme.disabled(),
        ),
        Span::styled(format!(" {pct}%"), theme.disabled()),
    ]))
}

pub(super) fn sidebar_truncate(s: &str) -> String {
    const MAX: usize = 24;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(MAX).collect::<String>())
    }
}

pub(super) fn resource_lines(
    stats: &smith_core::ResourceStats,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "MACHINE",
            theme.secondary().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("CPU {:.0}%", stats.cpu_percent),
            theme.text(),
        )),
        Line::from(Span::styled(
            format!("RAM {} / {} MB", stats.ram_used_mb, stats.ram_total_mb),
            theme.text(),
        )),
    ];
    match (stats.vram_used_mb, stats.vram_total_mb) {
        (Some(used), Some(total)) if total > 0 => lines.push(Line::from(Span::styled(
            format!("VRAM {used} / {total} MB"),
            theme.text(),
        ))),
        (Some(used), _) => lines.push(Line::from(Span::styled(
            format!("VRAM {used} MB"),
            theme.text(),
        ))),
        _ => lines.push(Line::from(Span::styled("VRAM n/a", theme.disabled()))),
    }
    if let Some(gpu) = stats.gpu_percent {
        lines.push(Line::from(Span::styled(
            format!("GPU {gpu:.0}%"),
            theme.text(),
        )));
    }
    lines
}
