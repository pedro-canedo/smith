use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{
    format_thought, ActivityStatus, App, ChatLine, ChatRole, IdleHint, Modal, SPINNER_FRAMES,
};
use crate::components::input::INPUT_MIN_ROWS;
use crate::components::{chips, diff, panel, wrap};
use crate::theme::Theme;

const SIDEBAR_MIN_TERMINAL_WIDTH: u16 = 80;
const SIDEBAR_WIDTH: u16 = 28;
/// Comfortable reading width for the message/input panes. Without
/// this, a wide terminal stretches prose edge-to-edge — harder to read, and
/// tables in particular wrap mid-row instead of just running past a
/// reasonable margin.
const MAX_CONTENT_WIDTH: u16 = 100;
/// Error cards always surface the tail of the failure output, even in
/// compact mode — the reason a call broke should never be one keystroke away.
const ERROR_TAIL_LINES: usize = 3;
/// Cap for tool output in verbose (expanded) mode.
const VERBOSE_OUTPUT_CAP: usize = 12;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let suggestions = app.slash_suggestions();
    let suggest_height = if suggestions.is_empty() {
        0
    } else {
        (suggestions.len().min(6) as u16) + 1 // +1 hint line
    };

    let [message_area, suggest_area, input_area, footer_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(suggest_height),
        Constraint::Length(input_height(app, frame.area())),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let is_idle = app.lines.is_empty() && app.in_flight_text.is_none();
    // A modal takes over the interface — the transcript behind it must not
    // stay on screen too. Left up, its unwrapped-at-full-width lines poke
    // out on both sides of the centered popup and compete with it for
    // attention, which is exactly what made the plan/question boxes hard to
    // read. Blank the pane instead of drawing the transcript into it.
    let modal_open = app.modal.is_some();
    let theme = app.theme.clone();

    if is_idle {
        draw_idle(frame, &*app, message_area);
    } else if message_area.width >= SIDEBAR_MIN_TERMINAL_WIDTH {
        let [chat_area, sidebar_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(SIDEBAR_WIDTH)])
                .areas(message_area);
        if modal_open {
            frame.render_widget(Clear, chat_area);
        } else {
            draw_messages(frame, app, clamp_width(chat_area, MAX_CONTENT_WIDTH));
        }
        draw_sidebar(frame, &*app, sidebar_area);
    } else if modal_open {
        frame.render_widget(Clear, message_area);
    } else {
        draw_messages(frame, app, clamp_width(message_area, MAX_CONTENT_WIDTH));
    }

    if suggest_height > 0 {
        draw_slash_suggestions(
            frame,
            app,
            &suggestions,
            clamp_width(suggest_area, MAX_CONTENT_WIDTH),
        );
    }

    draw_input(frame, app, clamp_width(input_area, MAX_CONTENT_WIDTH));
    draw_status_bar(frame, &*app, footer_area);

    match &mut app.modal {
        Modal::Question(modal) => draw_question_modal(frame, modal, &theme, frame.area()),
        Modal::Plan(modal) => draw_plan_modal(frame, modal, &theme, frame.area()),
        Modal::Permission(modal) => draw_permission_modal(frame, modal, &theme, frame.area()),
        Modal::None => {}
    }
}

fn draw_idle(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;

    let mut lines: Vec<Line> = app
        .banner
        .lines()
        .map(|l| Line::from(l).style(theme.ember_bold()))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("{} · {}", app.provider_label, app.model_label),
        theme.secondary(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Enter send   Alt+Enter newline   Esc cancel   Ctrl+O tool detail   Ctrl+C quit",
        theme.disabled(),
    )));
    lines.push(Line::from(""));

    match &app.idle_hint {
        IdleHint::Tip(tip) => {
            lines.push(Line::from(vec![
                Span::styled("● ", theme.amber()),
                Span::styled("Tip ", theme.amber().add_modifier(Modifier::BOLD)),
                Span::styled(tip.clone(), theme.disabled()),
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

/// Caps `area` to `max_width`, left-aligned — the leftover space (on a wide
/// terminal) is simply left blank rather than stretching content into it.
fn clamp_width(area: Rect, max_width: u16) -> Rect {
    Rect {
        width: area.width.min(max_width),
        ..area
    }
}

/// Rows to reserve for the input box: it grows with the text up to
/// `INPUT_MAX_ROWS` and then scrolls internally, the way a modern CLI prompt
/// behaves. Split out of `draw` so it can be tested without a `Frame`.
///
/// There's no circularity here — the box's width comes from the whole frame,
/// which is known before the vertical split that consumes this height.
fn input_height(app: &mut App, frame_area: Rect) -> u16 {
    let width = clamp_width(frame_area, MAX_CONTENT_WIDTH).width;
    let wanted = app.input.outer_rows(width);
    // Never let the prompt crowd out the transcript on a short terminal.
    let budget = frame_area.height.saturating_sub(3).max(INPUT_MIN_ROWS);
    wanted.min(budget)
}

fn center_vertically(area: Rect, height: u16) -> Rect {
    let height = height.min(area.height);
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x: area.x,
        y,
        width: area.width,
        height,
    }
}

fn draw_messages(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme.clone();
    let spinner = SPINNER_FRAMES[app.spinner_frame % SPINNER_FRAMES.len()];
    let verbose = app.verbose_tools;
    let mut lines: Vec<Line> = Vec::new();
    for line in &app.lines {
        match line.role {
            ChatRole::User => {
                // Two blank lines before a user turn, one everywhere else —
                // the bubble is the transcript's chapter break.
                lines.push(Line::from(""));
                lines.push(Line::from(""));
                lines.extend(user_bubble(&theme, &line.text, area.width));
            }
            ChatRole::Assistant => {
                lines.extend(assistant_block(
                    &theme,
                    &line.text,
                    line.meta.as_deref(),
                    area.width,
                ));
            }
            ChatRole::System => {
                lines.push(Line::from(vec![
                    Span::styled("· ", theme.disabled()),
                    Span::styled(line.text.clone(), theme.disabled()),
                ]));
            }
            ChatRole::Thought => {
                lines.push(Line::from(vec![
                    Span::styled("+ ", theme.ember_bold()),
                    Span::styled(format!("Thought: {}", line.text), theme.ember()),
                ]));
            }
            ChatRole::Tool => {
                lines.extend(tool_card(&theme, line, area.width, verbose, spinner));
            }
        }
        lines.push(Line::from(""));
    }
    if let Some(text) = &app.in_flight_text {
        // Same chrome as a finished reply, so the text doesn't shift when the
        // turn completes and the streaming buffer becomes a ChatLine.
        lines.extend(assistant_block(&theme, text, None, area.width));
    }

    // Nothing may exceed the pane width by the time it reaches the Paragraph:
    // a box row folded by `Wrap` puts its closing border on the next row and
    // breaks the frame. Wrapping here also gives System/Thought rows the line
    // breaking they never had.
    let lines = fit_lines(lines, area.width as usize);

    // Build the paragraph first so scroll uses wrapped (visual) height,
    // not logical line count — long table rows otherwise leave the viewport
    // stuck above the true bottom even with follow_bottom.
    //
    // `Wrap` is a no-op now that `fit_lines` runs, but it stays as a safety
    // net for any future producer that forgets, and removing it would mean
    // rewriting the scroll math below in the same change.
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

    // Jump pill — the user scrolled up to read history while the agent kept
    // working; offer a visible way back to the live edge.
    if !app.follow_bottom
        && (app.waiting_on_assistant || app.in_flight_text.is_some())
        && area.width > 20
        && area.height > 2
    {
        let label = " ↓ new activity ";
        let pill = Rect {
            x: area.x + area.width.saturating_sub(label.chars().count() as u16 + 1),
            y: area.y + area.height.saturating_sub(1),
            width: label.chars().count() as u16,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default().fg(theme.ember).bg(theme.overlay),
            )),
            pill,
        );
    }
}

/// A tool call rendered as a raised card: one header row (icon, tool name,
/// target, duration) plus a context-dependent body — live command inset while
/// running, error tail on failure, full input/output/diff when verbose.
fn tool_card(
    theme: &Theme,
    line: &ChatLine,
    area_width: u16,
    verbose: bool,
    spinner: &str,
) -> Vec<Line<'static>> {
    let w = area_width as usize;
    let bg = theme.raised_bg();
    let name = line.tool_name.clone().unwrap_or_default();
    let target = tool_target(line);

    let (icon, icon_style) = match line.tool_status {
        Some(ActivityStatus::Running) | None => (spinner.to_string(), theme.ember()),
        Some(ActivityStatus::Done) => ("✓".to_string(), theme.success()),
        Some(ActivityStatus::Error) => ("✗".to_string(), theme.danger()),
    };

    let mut left = vec![
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(name.clone(), theme.bold()),
    ];
    if !target.is_empty() {
        left.push(Span::styled(
            format!(" {target}"),
            match name.as_str() {
                "run_bash" => theme.text(),
                _ => theme.amber(),
            },
        ));
    }
    let left_width: usize = left.iter().map(|s| s.width()).sum();

    let duration = match line.tool_status {
        Some(ActivityStatus::Running) | None => line
            .started_at
            .map(|t| format!(" {} ", format_thought(t.elapsed().as_secs_f32())))
            .unwrap_or_default(),
        _ => line
            .tool_secs
            .map(|s| format!(" {} ", format_thought(s)))
            .unwrap_or_default(),
    };
    let pad = w
        .saturating_sub(left_width + duration.chars().count())
        .max(1);
    left.push(Span::styled(" ".repeat(pad), theme.disabled()));
    left.push(Span::styled(duration, theme.disabled()));

    let mut out = vec![panel::fill_line(left, w, bg)];

    let finished_error = matches!(line.tool_status, Some(ActivityStatus::Error));
    let finished = matches!(
        line.tool_status,
        Some(ActivityStatus::Done) | Some(ActivityStatus::Error)
    );

    // Running: show what the call is actually doing.
    if matches!(line.tool_status, Some(ActivityStatus::Running) | None) {
        if name == "run_bash" {
            if let Some(cmd) = tool_field(line, "command") {
                let row = Line::from(vec![
                    Span::styled("$ ", theme.success()),
                    Span::styled(cmd.to_string(), theme.text()),
                ]);
                out.extend(panel::inset(&[row], w, theme.overlay_bg()));
            }
        } else if !target.is_empty() {
            let row = Line::from(Span::styled(target.clone(), theme.amber()));
            out.extend(panel::inset(&[row], w, theme.overlay_bg()));
        }
    }

    // Errors always surface the tail of the failure output.
    if finished_error {
        for l in error_tail(line, ERROR_TAIL_LINES) {
            let mut spans = vec![Span::styled("▌ ", theme.danger())];
            spans.push(Span::styled(
                truncate_chars(&l, w.saturating_sub(4)),
                theme.secondary(),
            ));
            out.push(panel::fill_line(spans, w, bg));
        }
    }

    // Verbose: full input + output (+ a real diff for edit_file).
    if verbose && finished {
        match name.as_str() {
            "edit_file" => {
                let old = tool_field(line, "old_str").unwrap_or("");
                let new = tool_field(line, "new_str").unwrap_or("");
                if !old.is_empty() || !new.is_empty() {
                    out.extend(diff::render_diff(old, new, theme, w));
                }
            }
            "write_file" => {
                let mut rows = vec![];
                if let Some(path) = tool_field(line, "path") {
                    rows.push(Line::from(Span::styled(path.to_string(), theme.amber())));
                }
                if let Some(content) = tool_field(line, "content") {
                    for l in content.lines().take(ERROR_TAIL_LINES) {
                        rows.push(Line::from(Span::styled(l.to_string(), theme.secondary())));
                    }
                    if content.lines().count() > ERROR_TAIL_LINES {
                        rows.push(Line::from(Span::styled("…", theme.disabled())));
                    }
                }
                if !rows.is_empty() {
                    out.extend(panel::inset(&rows, w, theme.overlay_bg()));
                }
            }
            _ => {
                let mut rows = vec![];
                if name == "run_bash" {
                    if let Some(cmd) = tool_field(line, "command") {
                        rows.push(Line::from(vec![
                            Span::styled("$ ", theme.success()),
                            Span::styled(cmd.to_string(), theme.text()),
                        ]));
                    }
                } else if !target.is_empty() {
                    rows.push(Line::from(Span::styled(target.clone(), theme.amber())));
                }
                if !rows.is_empty() {
                    out.extend(panel::inset(&rows, w, theme.overlay_bg()));
                }
            }
        }

        if let Some(output) = line.tool_output.as_deref() {
            let total = output.lines().count();
            let cap = total.min(VERBOSE_OUTPUT_CAP);
            let style = if finished_error {
                theme.danger()
            } else {
                theme.secondary()
            };
            let rows: Vec<Line> = output
                .lines()
                .take(cap)
                .map(|l| Line::from(Span::styled(truncate_chars(l, w.saturating_sub(4)), style)))
                .collect();
            if !rows.is_empty() {
                out.extend(panel::inset(&rows, w, theme.overlay_bg()));
            }
            if total > cap {
                let row = Line::from(Span::styled(
                    format!("… +{} lines", total - cap),
                    theme.disabled(),
                ));
                out.extend(panel::inset(&[row], w, theme.overlay_bg()));
            }
        }
    }

    out
}

fn tool_field<'a>(line: &'a ChatLine, key: &str) -> Option<&'a str> {
    line.tool_input
        .as_ref()
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
}

/// The human-readable target of a tool call (path / command / pattern /
/// query), for the card header.
fn tool_target(line: &ChatLine) -> String {
    let name = line.tool_name.as_deref().unwrap_or("");
    let target = match name {
        "read_file" | "write_file" | "edit_file" | "list_dir" => tool_field(line, "path"),
        "glob" => tool_field(line, "pattern"),
        "run_bash" => tool_field(line, "command"),
        "web_search" => tool_field(line, "query"),
        _ => None,
    }
    .unwrap_or_default();
    truncate_chars(target, MAX_LABEL_CHARS_DISPLAY)
}

const MAX_LABEL_CHARS_DISPLAY: usize = 64;

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn error_tail(line: &ChatLine, max: usize) -> Vec<String> {
    line.tool_output
        .as_deref()
        .map(|o| {
            o.lines()
                .rev()
                .take(max)
                .map(str::to_string)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        })
        .unwrap_or_default()
}

/// Gutter drawn down the left of an assistant reply: a solid bar on the first
/// row, a hairline on the continuations. Cheaper on the eye than a full box
/// and it keeps prose readable, while still marking the turn as the model's.
const ASSISTANT_GUTTER: &str = "▌ ";
const ASSISTANT_GUTTER_CONT: &str = "▏ ";
/// Cells the gutter occupies — both glyphs are one cell plus a space.
const ASSISTANT_GUTTER_WIDTH: usize = 2;

/// Renders a user message inside a raised rounded box titled `You`.
///
/// The box spans the full pane width, on the same grid as the assistant's
/// text. It used to size itself to its content, which left short messages as
/// a ragged stub against the left edge — and, worse, it never wrapped: only
/// the *box* was clamped to the pane, so a long message produced rows wider
/// than the frame around them.
fn user_bubble(theme: &Theme, text: &str, area_width: u16) -> Vec<Line<'static>> {
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

    panel::rounded_box_titled(
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
fn assistant_block(
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
            let (glyph, style) = if i == 0 {
                (ASSISTANT_GUTTER, theme.ember())
            } else {
                (ASSISTANT_GUTTER_CONT, theme.disabled())
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
fn fit_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
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

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let head = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            theme.secondary().add_modifier(Modifier::BOLD),
        ))
    };
    let total_tokens = app.usage.input_tokens + app.usage.output_tokens;

    let mut lines = vec![
        head("SESSION"),
        Line::from(Span::styled(
            format!("{} · {}", app.provider_label, app.model_label),
            theme.text(),
        )),
    ];

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

    if !app.tasks.is_empty() {
        lines.extend(task_lines(&app.tasks, theme));
    }

    lines.extend([
        Line::from(""),
        head("CONTEXT"),
        Line::from(Span::styled(format!("{total_tokens} tokens"), theme.text())),
        Line::from(Span::styled(
            format!(
                "{} in / {} out",
                app.usage.input_tokens, app.usage.output_tokens
            ),
            theme.secondary(),
        )),
    ]);

    if let Some(rate) = app.display_tokens_per_sec() {
        lines.push(Line::from(Span::styled(
            format!("{rate:.1} tok/s"),
            theme.secondary(),
        )));
    }

    lines.push(Line::from(""));
    if let Some(stats) = &app.resources {
        lines.extend(resource_lines(stats, theme));
    } else if let Some(cost) =
        crate::pricing::estimate_cost_usd(&app.provider_label, &app.model_label, &app.usage)
    {
        lines.push(head("COST"));
        lines.push(Line::from(Span::styled(
            format!("~${cost:.4} (est.)"),
            theme.text(),
        )));
    }

    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(theme.disabled());
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// Checklist section for the sidebar: a "3/8" count, a filled/unfilled
/// progress bar, then the not-yet-done tasks (completed ones collapse into
/// a trailing "+N completed" line so a long-running session's history
/// doesn't push the rest of the sidebar off-screen).
fn task_lines(tasks: &[smith_core::Task], theme: &Theme) -> Vec<Line<'static>> {
    use smith_core::TaskStatus;

    const MAX_SHOWN: usize = 6;
    const BAR_WIDTH: usize = 20;

    let total = tasks.len();
    let done = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .count();

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("TASKS", theme.secondary().add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {done}/{total}"), theme.disabled()),
        ]),
    ];

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
            TaskStatus::InProgress => ("▶", theme.ember()),
            _ => ("◻", theme.disabled()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{icon} "), style),
            Span::styled(sidebar_truncate(&task.content), theme.text()),
        ]));
    }
    if pending.len() > MAX_SHOWN {
        lines.push(Line::from(Span::styled(
            format!("… +{} more", pending.len() - MAX_SHOWN),
            theme.disabled(),
        )));
    }
    if done > 0 {
        lines.push(Line::from(vec![
            Span::styled("✔ ", theme.success()),
            Span::styled(format!("{done} completed"), theme.secondary()),
        ]));
    }
    lines
}

/// Filled/unfilled progress bar for the task checklist; `None` when there
/// are no tasks. Split out so the `total == 0` guard is an early return
/// rather than a `total > 0` wrapper around the division (clippy
/// `manual_checked_ops`).
fn progress_bar_line(
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
        Span::styled("▰".repeat(filled), theme.ember()),
        Span::styled(
            "▱".repeat(bar_width.saturating_sub(filled)),
            theme.disabled(),
        ),
        Span::styled(format!(" {pct}%"), theme.disabled()),
    ]))
}

fn sidebar_truncate(s: &str) -> String {
    const MAX: usize = 24;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    }
}

fn resource_lines(stats: &smith_core::ResourceStats, theme: &Theme) -> Vec<Line<'static>> {
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

fn draw_slash_suggestions(
    frame: &mut Frame,
    app: &App,
    suggestions: &[crate::slash::SlashSuggestion],
    area: Rect,
) {
    let theme = &app.theme;
    let w = area.width as usize;
    let mut lines: Vec<Line> = suggestions
        .iter()
        .take(6)
        .enumerate()
        .map(|(i, s)| {
            let selected = i == app.slash_selected;
            let marker = if selected { "› " } else { "  " };
            let spans = vec![Span::styled(
                format!("{marker}/{:<12} {}", s.name, s.description),
                if selected {
                    theme.info_bold()
                } else {
                    theme.secondary()
                },
            )];
            if selected {
                panel::fill_line(spans, w, theme.hover_bg())
            } else {
                Line::from(spans)
            }
        })
        .collect();
    lines.push(Line::from(Span::styled(
        "  Tab complete · ↑↓ select · Enter accept",
        theme.disabled(),
    )));
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_input(frame: &mut Frame, app: &mut App, area: Rect) {
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
            .title(title)
            .border_style(border),
    );
    app.input.set_placeholder(if plan {
        "Plan mode — approve the plan modal or /plan reject…"
    } else {
        "Ask anything…  type / for commands"
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
fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let bg = theme.raised_bg();
    let w = area.width as usize;

    let left = match &app.git_branch {
        Some(branch) => format!(" {} git:({}) ", app.cwd_display, branch),
        None => format!(" {} ", app.cwd_display),
    };
    let right = format!(" smith {} ", env!("CARGO_PKG_VERSION"));

    let busy = !matches!(app.phase, smith_core::AgentPhase::Idle)
        || app.waiting_on_assistant
        || app.modal.is_some();
    let center = if busy {
        let spinner = SPINNER_FRAMES[app.spinner_frame % SPINNER_FRAMES.len()];
        let mut s = format!("{spinner} {}", app.phase_label());
        if let Some((iteration, max_iterations)) = app.loop_progress {
            s.push_str(&format!(" · {iteration}/{max_iterations}"));
        }
        if let Some(secs) = app.turn_elapsed_secs() {
            s.push_str(&format!(" · {secs:.0}s"));
        }
        if let Some(tokens) = app.live_output_tokens_estimate() {
            if tokens > 0 {
                s.push_str(&format!(" · ~{tokens} tok"));
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
            spans.push(Span::styled(" ".repeat(gap1), bg));
            spans.push(Span::styled(c, theme.ember()));
            spans.push(Span::styled(" ".repeat(gap2), bg));
            spans.push(Span::styled(right, theme.disabled()));
        }
        None => {
            spans.push(Span::styled(right, theme.disabled()));
        }
    }
    frame.render_widget(Paragraph::new(panel::fill_line(spans, w, bg)), area);
}

fn draw_permission_modal(
    frame: &mut Frame,
    modal: &mut crate::app::PermissionModal,
    theme: &Theme,
    area: Rect,
) {
    let request = &modal.request;
    let width = area.width.saturating_sub(8).clamp(36, 72);

    let mut key_row = chips::confirm_hint("y", "allow once", theme);
    key_row.extend(chips::confirm_hint("a", "allow session", theme));
    key_row.extend(chips::cancel_hint("n", "deny", theme));

    let body_lines = vec![
        Line::from(vec![
            Span::styled("tool: ", theme.disabled()),
            Span::styled(
                request.tool_name.clone(),
                theme.warning().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(request.detail.clone(), theme.text())),
        Line::from(""),
        Line::from(key_row),
    ];

    let height = 8;
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let paragraph = Paragraph::new(Text::from(body_lines))
        .style(theme.raised_bg())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " permission requested ",
                    theme.warning().add_modifier(Modifier::BOLD),
                ))
                .border_style(theme.warning()),
        )
        .wrap(Wrap { trim: false });

    // Compact summary — keep scroll at 0 even if keys were pressed earlier.
    modal.scroll = 0;

    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

fn draw_plan_modal(
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
    body_lines.push(Line::from(Span::styled(
        "↑↓ / PgUp/PgDn scroll",
        theme.disabled(),
    )));

    let height = (body_lines.len() as u16)
        .saturating_add(2)
        .min(max_height)
        .max(10);

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let inner_width = width.saturating_sub(2);
    let inner_height = height.saturating_sub(2);
    let paragraph = Paragraph::new(Text::from(body_lines))
        .style(theme.raised_bg())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" plan ready ", theme.plan_bold()))
                .border_style(theme.plan()),
        )
        .wrap(Wrap { trim: false });
    let content_height = paragraph.line_count(inner_width) as u16;
    let max_scroll = content_height.saturating_sub(inner_height);
    modal.scroll = modal.scroll.min(max_scroll);

    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph.scroll((modal.scroll, 0)), popup);
}

fn draw_question_modal(
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
        let marker = if selected { "›" } else { " " };
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
    let custom_marker = if modal.selected == 3 { "›" } else { " " };
    let custom_display = if modal.custom.is_empty() {
        "…".to_string()
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

    let height = (body_lines.len() as u16).saturating_add(2).clamp(12, 20);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let paragraph = Paragraph::new(Text::from(body_lines))
        .style(theme.raised_bg())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(" question ", theme.info_bold()))
                .border_style(theme.info()),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ChatRole;

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
        ChatLine {
            role: ChatRole::Tool,
            text: format!("Running {name}"),
            meta: None,
            tool_id: Some("call_1".into()),
            tool_status: Some(status),
            tool_name: Some(name.into()),
            tool_input: Some(serde_json::json!({"path": "src/main.rs"})),
            tool_output: Some("file contents".into()),
            tool_secs: Some(0.4),
            started_at: None,
        }
    }

    #[test]
    fn tool_card_header_shows_name_target_and_duration() {
        let theme = Theme::ansi();
        let lines = tool_card(
            &theme,
            &tool_line("read_file", ActivityStatus::Done),
            60,
            false,
            "⠋",
        );
        let header = lines[0].to_string();
        assert!(header.contains("✓"), "{header}");
        assert!(header.contains("read_file"), "{header}");
        assert!(header.contains("src/main.rs"), "{header}");
        assert!(header.contains("400ms"), "{header}");
    }

    #[test]
    fn tool_card_compact_done_has_header_only() {
        let theme = Theme::ansi();
        let lines = tool_card(
            &theme,
            &tool_line("read_file", ActivityStatus::Done),
            60,
            false,
            "⠋",
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
            "⠋",
        );
        let text: String = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.contains("file contents"), "{text}");
    }

    #[test]
    fn tool_card_error_shows_tail_even_when_compact() {
        let theme = Theme::ansi();
        let mut line = tool_line("run_bash", ActivityStatus::Error);
        line.tool_input = Some(serde_json::json!({"command": "cargo test"}));
        line.tool_output = Some("boom: permission denied".into());
        let lines = tool_card(&theme, &line, 60, false, "⠋");
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
            "⠋",
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
        let lines = assistant_block(&theme, &"resposta longa ".repeat(20), None, 50);
        assert!(lines.len() > 1, "should have wrapped");
        for (i, line) in lines.iter().enumerate() {
            let text = line.to_string();
            let expected = if i == 0 {
                ASSISTANT_GUTTER
            } else {
                ASSISTANT_GUTTER_CONT
            };
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
        assert!(last.starts_with(ASSISTANT_GUTTER_CONT), "last: {last}");
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

        let mut app = app_for_input_tests();
        app.lines.push(ChatLine::new(
            ChatRole::User,
            "uma mensagem bem longa que precisa quebrar em varias linhas dentro da bolha"
                .to_string(),
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

    fn app_for_input_tests() -> App {
        App::new(crate::app::TuiConfig {
            banner: String::new(),
            provider_label: "ollama".into(),
            model_label: "qwen2.5".into(),
            cwd_display: "~/smith".into(),
            git_branch: None,
            idle_hint: IdleHint::Tip(String::new()),
            initial_lines: Vec::new(),
            permission_policy: smith_core::PermissionPolicy::default(),
            goal: None,
            tasks: Vec::new(),
        })
    }

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
        let mut app = app_for_input_tests();
        assert_eq!(input_height(&mut app, frame(80, 30)), INPUT_MIN_ROWS);
    }

    #[test]
    fn input_box_grows_with_content_then_caps() {
        let mut app = app_for_input_tests();
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
        let mut app = app_for_input_tests();
        app.input.set(&"palavra ".repeat(400));
        // 8 rows tall: the prompt must leave room for transcript + status bar.
        let height = input_height(&mut app, frame(60, 8));
        assert!(height <= 5, "prompt took {height} of 8 rows");
    }

    #[test]
    fn long_input_renders_its_tail_and_places_the_caret_in_the_box() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = app_for_input_tests();
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
}
