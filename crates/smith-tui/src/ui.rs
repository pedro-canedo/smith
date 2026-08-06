use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::Frame;

use crate::app::{
    format_thought, ActivityStatus, App, ChatLine, ChatRole, IdleHint, Modal, Overlay, OverlayBody,
    SidebarTab,
};
use crate::components::input::INPUT_MIN_ROWS;
use crate::components::{chips, diff, panel, wrap};
use crate::theme::Theme;

/// Width at which the sidebar appears. 80 is *inside* this tier, not below
/// it: 80x24 is the terminal acceptance criterion #7 names, so it is the
/// full-fat layout that has to be legible there — see `docs/design-system.md`
/// §3.1.
const SIDEBAR_MIN_TERMINAL_WIDTH: u16 = 80;
const SIDEBAR_WIDTH: u16 = 28;
/// Below this width the status bar drops `cwd git:(branch)` and the version,
/// and the context strip keeps only the gauge.
const MINIMAL_TERMINAL_WIDTH: u16 = 48;
/// Terminals shorter than this drop the context strip: with fewer than 20
/// rows, one row of vitals costs the transcript more than it is worth.
const STRIP_MIN_TERMINAL_HEIGHT: u16 = 20;
/// Floor on transcript rows. Below it the pane stops being a transcript and
/// becomes a peephole; the input and the slash list grow only into what is
/// left above this line.
const TRANSCRIPT_MIN_ROWS: u16 = 8;
/// The floor relaxes while the slash list is open — that list is transient
/// and is what the user is actually reading at that moment.
const TRANSCRIPT_MIN_ROWS_WITH_SUGGEST: u16 = 4;
/// Narrowest a gauge can be and still show a bar next to its label.
const MIN_GAUGE_WIDTH: u16 = 16;
/// Comfortable reading width for the message/input panes. Without
/// this, a wide terminal stretches prose edge-to-edge — harder to read, and
/// tables in particular wrap mid-row instead of just running past a
/// reasonable margin.
const MAX_CONTENT_WIDTH: u16 = 100;
/// Error cards always surface the tail of the failure output, even in
/// compact mode — the reason a call broke should never be one keystroke away.
const ERROR_TAIL_LINES: usize = 3;
/// Queued prompts listed before the rest collapse into a "+N more" row.
const MAX_QUEUE_ROWS: usize = 3;
/// Cap for tool output in verbose (expanded) mode.
const VERBOSE_OUTPUT_CAP: usize = 12;
/// Shown in the status bar between the two `Ctrl+C` presses that quit.
const QUIT_HINT: &str = "press Ctrl+C again to quit";
/// Floor for the permission popup: header, detail, key row and both borders.
const PERMISSION_MODAL_MIN_HEIGHT: u16 = 8;

/// Rows each region of the vertical stack gets, allocated by priority rather
/// than by position — see `docs/design-system.md` §3.3. Pure, so the 80x24
/// contract is testable without a `Frame`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VerticalLayout {
    messages: u16,
    strip: u16,
    suggest: u16,
    /// Rows for the queued-prompt list, between the slash list and the input.
    queue: u16,
    input: u16,
    status: u16,
}

/// The whole compact-height rule in one place. Each step spends only what the
/// step before it left over, so the transcript's floor is structurally safe
/// from the input and the slash list instead of being safe by coincidence.
fn vertical_layout(
    height: u16,
    wanted_input: u16,
    wanted_suggest: u16,
    wanted_queue: u16,
    strip_wanted: bool,
) -> VerticalLayout {
    // 1. Status bar.
    let status = height.min(1);
    let mut free = height - status;

    // 2. The prompt, at its minimum. Never negotiable: a UI you cannot type
    //    into is not a smaller UI, it is a broken one.
    let input_min = INPUT_MIN_ROWS.min(free);
    free -= input_min;

    // 3. The transcript's floor.
    let mut messages = TRANSCRIPT_MIN_ROWS.min(free);
    free -= messages;

    // 4. The vitals the sidebar would have carried.
    let strip = if strip_wanted && height >= STRIP_MIN_TERMINAL_HEIGHT {
        free.min(1)
    } else {
        0
    };
    free -= strip;

    // 5. The slash list. It is the one region allowed to borrow from the
    //    transcript's floor — and only down to the relaxed one, and only when
    //    it would otherwise not fit at all. A completion list you cannot see
    //    is a keybinding you cannot discover.
    let mut suggest = wanted_suggest.min(free);
    free -= suggest;
    if suggest < wanted_suggest {
        let borrow = messages
            .saturating_sub(TRANSCRIPT_MIN_ROWS_WITH_SUGGEST)
            .min(wanted_suggest - suggest);
        messages -= borrow;
        suggest += borrow;
    }

    // 6. Queued prompts. Above the prompt's growth: a message you have
    //    already committed to sending outranks room to type the next one, and
    //    a queue you cannot see is a queue you will forget you filled.
    let queue = wanted_queue.min(free);
    free -= queue;

    // 7. The prompt's growth — last, so a long draft never costs transcript.
    let growth = wanted_input.saturating_sub(input_min).min(free);
    free -= growth;

    VerticalLayout {
        // Whatever nobody claimed belongs to the transcript.
        messages: messages + free,
        strip,
        suggest,
        queue,
        input: input_min + growth,
        status,
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    // The base surface, painted before anything else: every region that does
    // not declare its own background inherits the theme's page colour rather
    // than whatever the terminal happens to be set to.
    let whole_screen = frame.area();
    frame.render_widget(Block::new().style(app.theme.base_bg()), whole_screen);

    let suggestions = app.suggestions();
    let wanted_suggest = if suggestions.is_empty() {
        0
    } else {
        (suggestions.len().min(6) as u16) + 1 // +1 hint line
    };

    let is_idle = app.lines.is_empty() && app.in_flight_text.is_none();
    // The strip is the sidebar's understudy: it exists only when the sidebar
    // does not *fit*, and only when it would actually say something. A sidebar
    // the user dismissed with `Ctrl+B` deliberately gets no stand-in — they
    // asked for the rows back, and handing one straight back as a strip is
    // the opposite of what the key means.
    let strip_wanted = !is_idle
        && frame.area().width < SIDEBAR_MIN_TERMINAL_WIDTH
        && (app.context.is_some() || !strip_extras(app).is_empty());

    // One row per queued prompt plus the hint line naming the command, capped
    // so a long queue cannot eat the transcript.
    let wanted_queue = if app.queued.is_empty() {
        0
    } else {
        (app.queued.len().min(MAX_QUEUE_ROWS) as u16) + 1
    };

    let layout = vertical_layout(
        frame.area().height,
        wanted_input_rows(app, frame.area()),
        wanted_suggest,
        wanted_queue,
        strip_wanted,
    );
    let [message_area, strip_area, suggest_area, queue_area, input_area, footer_area] =
        Layout::vertical([
            Constraint::Length(layout.messages),
            Constraint::Length(layout.strip),
            Constraint::Length(layout.suggest),
            Constraint::Length(layout.queue),
            Constraint::Length(layout.input),
            Constraint::Length(layout.status),
        ])
        .areas(frame.area());
    // A modal takes over the interface — the transcript behind it must not
    // stay on screen too. Left up, its unwrapped-at-full-width lines poke
    // out on both sides of the centered popup and compete with it for
    // attention, which is exactly what made the plan/question boxes hard to
    // read. Blank the pane instead of drawing the transcript into it.
    let modal_open = app.modal.is_some();
    let theme = app.theme.clone();

    if is_idle {
        draw_idle(frame, &*app, message_area);
    } else if message_area.width >= SIDEBAR_MIN_TERMINAL_WIDTH && app.sidebar_visible {
        let [chat_area, sidebar_area] =
            Layout::horizontal([Constraint::Min(1), Constraint::Length(SIDEBAR_WIDTH)])
                .areas(message_area);
        if modal_open {
            frame.render_widget(Clear, chat_area);
            frame.render_widget(Block::new().style(theme.base_bg()), chat_area);
        } else {
            draw_messages(frame, app, clamp_width(chat_area, MAX_CONTENT_WIDTH));
        }
        draw_sidebar(frame, &*app, sidebar_area);
    } else if modal_open {
        frame.render_widget(Clear, message_area);
        frame.render_widget(Block::new().style(theme.base_bg()), message_area);
    } else {
        draw_messages(frame, app, clamp_width(message_area, MAX_CONTENT_WIDTH));
    }

    if layout.strip > 0 {
        draw_context_strip(frame, &*app, clamp_width(strip_area, MAX_CONTENT_WIDTH));
    }

    if layout.suggest > 0 {
        draw_slash_suggestions(
            frame,
            app,
            &suggestions,
            clamp_width(suggest_area, MAX_CONTENT_WIDTH),
        );
    }

    if layout.queue > 0 {
        draw_queue(frame, &*app, clamp_width(queue_area, MAX_CONTENT_WIDTH));
    }

    draw_input(frame, app, clamp_width(input_area, MAX_CONTENT_WIDTH));
    draw_status_bar(frame, &*app, footer_area);

    // The overlay yields to a real modal rather than stacking above it: a
    // permission prompt is a question the agent is blocked on, and hiding it
    // behind a table nobody asked to keep open is how a turn appears to hang.
    match &mut app.modal {
        Modal::Question(modal) => draw_question_modal(frame, modal, &theme, frame.area()),
        Modal::Plan(modal) => draw_plan_modal(frame, modal, &theme, frame.area()),
        Modal::Permission(modal) => draw_permission_modal(frame, modal, &theme, frame.area()),
        Modal::None => {
            if let Some(overlay) = app.overlay.as_mut() {
                draw_overlay(frame, overlay, &theme, frame.area());
            }
        }
    }
}

/// A read-only panel: `/usage` and `/mcp` as a real `Table`, the `Ctrl+L` log
/// as plain rows. Scroll is clamped here rather than in `App` because this is
/// the only place that knows how many rows the panel actually got.
fn draw_overlay(frame: &mut Frame, overlay: &mut Overlay, theme: &Theme, area: Rect) {
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
        let mut footer: Vec<Line<'static>> = overlay
            .footer
            .iter()
            .map(|f| Line::from(Span::styled(f.clone(), theme.disabled())))
            .collect();
        // Only claimed when there is something below to reach: a scroll hint
        // on a panel that fits is noise.
        if max_scroll > 0 {
            if let Some(first) = footer.first_mut() {
                first
                    .spans
                    .push(Span::styled("  (scrollable)", theme.warning()));
            }
        }
        frame.render_widget(Paragraph::new(footer), footer_area);
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

/// Caps `area` to `max_width`, left-aligned — the leftover space (on a wide
/// terminal) is simply left blank rather than stretching content into it.
fn clamp_width(area: Rect, max_width: u16) -> Rect {
    Rect {
        width: area.width.min(max_width),
        ..area
    }
}

/// Rows the input box *wants*: it grows with the text up to `INPUT_MAX_ROWS`
/// and then scrolls internally, the way a modern CLI prompt behaves.
///
/// There's no circularity here — the box's width comes from the whole frame,
/// which is known before the vertical split that consumes this height.
fn wanted_input_rows(app: &mut App, frame_area: Rect) -> u16 {
    let width = clamp_width(frame_area, MAX_CONTENT_WIDTH).width;
    app.input.outer_rows(width)
}

/// The vitals that live in the sidebar, compressed onto one row for the
/// terminals too narrow to have one (design-system §3.2). Priority order,
/// dropped from the right as the row runs out.
fn strip_extras(app: &App) -> String {
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
fn draw_context_strip(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_messages(frame: &mut Frame, app: &mut App, area: Rect) {
    // Recorded for the mouse: turning a click into a transcript row needs the
    // rect, and this is the only place that knows it.
    app.message_area = area;
    let key = crate::transcript::LayoutKey {
        width: area.width,
        verbose: app.verbose_tools,
        theme: app.theme.clone(),
    };
    let ember = app.theme.ember;
    let overlay = app.theme.overlay;

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
            Paragraph::new(Span::styled(label, Style::default().fg(ember).bg(overlay))),
            pill,
        );
    }
}

/// A tool call rendered as a raised card: one header row (icon, tool name,
/// target, duration) plus a context-dependent body — live command inset while
/// running, error tail on failure, full input/output/diff when verbose.
/// The throbber frame for one card.
///
/// A card with a `started_at` derives its phase from *its own* clock, so two
/// tools that began at different moments are visibly out of step — the
/// difference between the screen saying "something is happening" and "these
/// three things are happening". Cards with no start time (a restored
/// transcript, tests) fall back to the global counter.
///
/// This costs the render memo nothing: a `Running` card is already excluded
/// from it by `ChatLine::is_animating`, so it was being rebuilt every frame
/// regardless.
fn spinner_frame_for(line: &ChatLine, global: usize, theme: &Theme) -> &'static str {
    let frames = theme.spinner_frames();
    let phase = match line.started_at() {
        Some(started) => (started.elapsed().as_millis() / crate::app::SPINNER_INTERVAL_MS) as usize,
        None => global,
    };
    frames[phase % frames.len()]
}

fn tool_card(
    theme: &Theme,
    line: &ChatLine,
    area_width: u16,
    verbose: bool,
    spinner_frame: usize,
) -> Vec<Line<'static>> {
    let w = area_width as usize;
    // Selection is carried by the surface *and* by a marker glyph: on a
    // 16-colour terminal the elevation step can be invisible, and on a
    // monochrome one it certainly is.
    let selected = line.selected();
    let bg = if selected {
        theme.hover_bg()
    } else {
        theme.raised_bg()
    };
    let name = line.tool_name().unwrap_or_default().to_string();
    let target = tool_target(line);
    // Per-card expansion (`Enter`) on top of the global default.
    let verbose = verbose || line.expanded();

    let (icon, icon_style) = match line.tool_status() {
        Some(ActivityStatus::Running) | None => (
            spinner_frame_for(line, spinner_frame, theme).to_string(),
            theme.ember(),
        ),
        Some(ActivityStatus::Done) => (theme.icon_ok().to_string(), theme.success()),
        Some(ActivityStatus::Error) => (theme.icon_error().to_string(), theme.danger()),
    };

    // The header speaks in activity labels, not tool names: "Searching the
    // web…" while running, "Search completed" when done, "Search failed" on
    // error. The raw name (`web_search`) moves to the verbose body.
    let labels = crate::app::tool_labels(&name);
    let (header_label, running_header) = match line.tool_status() {
        Some(ActivityStatus::Running) | None => (labels.running, true),
        Some(ActivityStatus::Done) => (labels.done, false),
        Some(ActivityStatus::Error) => (labels.failed, false),
    };

    let mut left = Vec::new();
    if selected {
        left.push(Span::styled(
            format!("{} ", theme.marker_selected()),
            theme.info_bold(),
        ));
    }
    left.extend([
        Span::styled(format!("{icon} "), icon_style),
        Span::styled(header_label, theme.bold()),
    ]);
    if !target.is_empty() {
        // Running reads as a sentence ("Reading src/main.rs"); a settled card
        // separates verdict from target ("Read · src/main.rs").
        let separator = if running_header {
            " "
        } else {
            theme.separator()
        };
        left.push(Span::styled(
            format!("{separator}{target}"),
            match name.as_str() {
                "run_bash" => theme.text(),
                _ => theme.amber(),
            },
        ));
    }
    let left_width: usize = left.iter().map(|s| s.width()).sum();

    let duration = match line.tool_status() {
        Some(ActivityStatus::Running) | None => line
            .started_at()
            .map(|t| format!(" {} ", format_thought(t.elapsed().as_secs_f32())))
            .unwrap_or_default(),
        _ => line
            .tool_secs()
            .map(|s| format!(" {} ", format_thought(s)))
            .unwrap_or_default(),
    };
    let pad = w
        .saturating_sub(left_width + duration.chars().count())
        .max(1);
    left.push(Span::styled(" ".repeat(pad), theme.disabled()));
    left.push(Span::styled(duration, theme.disabled()));

    let mut out = vec![panel::fill_line(left, w, bg)];

    // Folded siblings, one row each: the header says what activity is running
    // and these say what it is running *on*. The first target is already in
    // the header, so the list starts with the second call.
    if !line.grouped().is_empty() {
        let branch = if theme.unicode {
            "\u{2514}\u{2500} "
        } else {
            "|- "
        };
        for call in line.grouped() {
            let (glyph, style) = match call.status {
                ActivityStatus::Running => (
                    spinner_frame_for(line, spinner_frame, theme).to_string(),
                    theme.ember(),
                ),
                ActivityStatus::Done => (theme.icon_ok().to_string(), theme.success()),
                ActivityStatus::Error => (theme.icon_error().to_string(), theme.danger()),
            };
            let text = truncate_chars(&call.label, w.saturating_sub(branch.chars().count() + 4));
            out.push(panel::fill_line(
                vec![
                    Span::styled(format!("  {branch}"), theme.disabled()),
                    Span::styled(format!("{glyph} "), style),
                    Span::styled(text, theme.secondary()),
                ],
                w,
                bg,
            ));
        }
    }

    let finished_error = matches!(line.tool_status(), Some(ActivityStatus::Error));
    let finished = matches!(
        line.tool_status(),
        Some(ActivityStatus::Done) | Some(ActivityStatus::Error)
    );

    // Running: show what the call is actually doing.
    if matches!(line.tool_status(), Some(ActivityStatus::Running) | None) {
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
        // The newest `ToolProgress` line, so a long call visibly moves
        // instead of sitting on a spinner (`set_progress` keeps only the
        // latest). Before this, the line was stored and never shown.
        if let Some(progress) = line.tool_output().and_then(|o| o.lines().next_back()) {
            let row = Line::from(vec![
                Span::styled(format!("{} ", theme.ellipsis()), theme.disabled()),
                Span::styled(
                    truncate_chars(progress, w.saturating_sub(6)),
                    theme.secondary(),
                ),
            ]);
            out.extend(panel::inset(&[row], w, theme.overlay_bg()));
        }
    }

    // Errors always surface the tail of the failure output.
    if finished_error {
        for l in error_tail(line, ERROR_TAIL_LINES) {
            let mut spans = vec![Span::styled(
                theme.assistant_gutter().0.to_string(),
                theme.danger(),
            )];
            spans.push(Span::styled(
                truncate_chars(&l, w.saturating_sub(4)),
                theme.secondary(),
            ));
            out.push(panel::fill_line(spans, w, bg));
        }
    }

    // Verbose: full input + output (+ a real diff for edit_file).
    if verbose && finished {
        // The raw tool name lives here now that the header speaks in
        // friendly labels — verbose is exactly the "what actually ran" view.
        out.extend(panel::inset(
            &[Line::from(Span::styled(name.clone(), theme.disabled()))],
            w,
            theme.overlay_bg(),
        ));
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
                        rows.push(Line::from(Span::styled(
                            theme.ellipsis().to_string(),
                            theme.disabled(),
                        )));
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

        if let Some(output) = line.tool_output() {
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
    line.tool_input()
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_str())
}

/// The human-readable target of a tool call (path / command / pattern /
/// query), for the card header.
fn tool_target(line: &ChatLine) -> String {
    let name = line.tool_name().unwrap_or("");
    let target = match name {
        "read_file" | "write_file" | "edit_file" | "list_dir" => tool_field(line, "path"),
        "glob" => tool_field(line, "pattern"),
        "grep" => tool_field(line, "pattern"),
        "task" => tool_field(line, "description"),
        "multi_edit" => tool_field(line, "path"),
        "run_bash" => tool_field(line, "command"),
        "web_search" => tool_field(line, "query"),
        "web_fetch" => tool_field(line, "url"),
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
    line.tool_output()
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

/// The sidebar: a tab strip, then the active tab's content, split around the
/// one widget in it that isn't a `Line`.
///
/// `above` ends with the `CONTEXT` header, the gauge goes in the row after it,
/// and `below` is everything else. Both halves go through `fit_lines` instead
/// of relying on `Wrap`: the gauge is positioned by row offset, so the line
/// count has to equal the row count exactly — a single wrapped line would
/// slide the gauge off its header.
fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
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

    let gauge_rows = u16::from(app.context.is_some() && app.sidebar_tab == SidebarTab::Session);
    let [above_area, gauge_area, below_area] = Layout::vertical([
        Constraint::Length(above.len() as u16),
        Constraint::Length(gauge_rows),
        Constraint::Min(0),
    ])
    .areas(body_area);

    frame.render_widget(Paragraph::new(above), above_area);
    if let Some((used, window, estimated)) = app.context {
        if gauge_area.height > 0 {
            frame.render_widget(
                crate::components::gauge::context_gauge(used, window, estimated, theme),
                gauge_area,
            );
        }
    }
    frame.render_widget(Paragraph::new(below), below_area);
}

/// The tab strip. Titles are abbreviated to their first four characters when
/// the pane is too narrow for the full set — a clipped `Tabs` drops the last
/// title entirely, which would make a tab the user can select but not see.
fn sidebar_tabs(app: &App, width: u16) -> Tabs<'static> {
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
fn sidebar_lines(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    match app.sidebar_tab {
        SidebarTab::Session => session_tab_lines(app),
        SidebarTab::Tasks => (Vec::new(), tasks_tab_lines(app)),
        SidebarTab::Vitals => (Vec::new(), vitals_tab_lines(app)),
    }
}

fn sidebar_head(theme: &Theme, s: &str) -> Line<'static> {
    Line::from(Span::styled(
        s.to_string(),
        theme.secondary().add_modifier(Modifier::BOLD),
    ))
}

fn session_tab_lines(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
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

    (lines, below)
}

fn tasks_tab_lines(app: &App) -> Vec<Line<'static>> {
    let theme = &app.theme;
    if app.tasks.is_empty() {
        return vec![Line::from(Span::styled("no tasks yet", theme.disabled()))];
    }
    task_lines(&app.tasks, theme)
}

fn vitals_tab_lines(app: &App) -> Vec<Line<'static>> {
    let theme = &app.theme;
    let mut lines: Vec<Line<'static>> = Vec::new();

    if let Some(stats) = &app.resources {
        lines.extend(resource_lines(stats, theme));
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

    lines
}

/// Checklist section for the sidebar: a "3/8" count, a filled/unfilled
/// progress bar, then the not-yet-done tasks (completed ones collapse into
/// a trailing "+N completed" line so a long-running session's history
/// doesn't push the rest of the sidebar off-screen).
fn task_lines(tasks: &[smith_core::Task], theme: &Theme) -> Vec<Line<'static>> {
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
fn task_summary_line(tasks: &[smith_core::Task], theme: &Theme) -> Line<'static> {
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

fn sidebar_truncate(s: &str) -> String {
    const MAX: usize = 24;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(MAX).collect::<String>())
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

/// Prompts typed while the agent was busy, waiting their turn.
///
/// Drawn as its own region rather than as transcript lines: they have not been
/// sent, and a queued message rendered as a user bubble would claim the agent
/// has seen something it has not.
fn draw_queue(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_slash_suggestions(
    frame: &mut Frame,
    app: &App,
    suggestions: &[crate::slash::SlashSuggestion],
    area: Rect,
) {
    let theme = &app.theme;
    let w = area.width as usize;
    let prefix = app.completion_kind.prefix();
    // A path is the whole entry and carries no description, so padding it to
    // a command's column width would just push every row right.
    let name_width = match app.completion_kind {
        crate::complete::CompletionKind::Slash => 12,
        crate::complete::CompletionKind::File => 0,
    };
    let mut lines: Vec<Line> = suggestions
        .iter()
        .take(6)
        .enumerate()
        .map(|(i, s)| {
            let selected = i == app.slash_selected;
            let marker = if selected { "› " } else { "  " };
            let spans = vec![Span::styled(
                format!("{marker}{prefix}{:<name_width$} {}", s.name, s.description),
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
            .border_set(theme.block_border_set())
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
fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
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

fn draw_permission_modal(
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

    let block = |scrollable: bool| {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(theme.block_border_set())
            .title(Span::styled(
                " permission requested ",
                theme.warning().add_modifier(Modifier::BOLD),
            ))
            .border_style(theme.warning());
        if scrollable {
            block.title_bottom(Span::styled(" ↑↓ / PgUp/PgDn scroll ", theme.disabled()))
        } else {
            block
        }
    };
    let paragraph = Paragraph::new(Text::from(body_lines))
        .style(theme.raised_bg())
        .block(block(false))
        .wrap(Wrap { trim: false });

    // `line_count` takes the **outer** width and returns the **outer** height:
    // it subtracts the block's borders itself before wrapping, and adds its two
    // border rows back to the count. Handing it `inner_width` therefore wrapped
    // the text at `width - 4` and over-counted every line that wrapped.
    let content_height = paragraph.line_count(width) as u16;
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
    frame.render_widget(
        paragraph
            .block(block(max_scroll > 0))
            .scroll((modal.scroll, 0)),
        popup,
    );
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

    let paragraph = Paragraph::new(Text::from(body_lines))
        .style(theme.raised_bg())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(theme.block_border_set())
                .title(Span::styled(" plan ready ", theme.plan_bold()))
                .border_style(theme.plan()),
        )
        .wrap(Wrap { trim: false });
    // Both sides of this are *outer* measurements — see the note in
    // `draw_permission_modal`. Mixing them was worth two rows of overscroll:
    // `line_count` includes the two border rows, so subtracting the inner
    // height let the plan scroll two lines past its own end into blank space.
    let content_height = paragraph.line_count(width) as u16;
    let max_scroll = content_height.saturating_sub(height);
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
                .border_set(theme.block_border_set())
                .title(Span::styled(" question ", theme.info_bold()))
                .border_style(theme.info()),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(Clear, popup);
    frame.render_widget(paragraph, popup);
}

#[cfg(test)]
mod tests;
