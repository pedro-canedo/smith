use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{format_thought, ActivityStatus, App, ChatLine, ChatRole, IdleHint, Modal};
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

    // 6. The prompt's growth — last, so a long draft never costs transcript.
    let growth = wanted_input.saturating_sub(input_min).min(free);
    free -= growth;

    VerticalLayout {
        // Whatever nobody claimed belongs to the transcript.
        messages: messages + free,
        strip,
        suggest,
        input: input_min + growth,
        status,
    }
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let suggestions = app.slash_suggestions();
    let wanted_suggest = if suggestions.is_empty() {
        0
    } else {
        (suggestions.len().min(6) as u16) + 1 // +1 hint line
    };

    let is_idle = app.lines.is_empty() && app.in_flight_text.is_none();
    // The strip is the sidebar's understudy: it exists only when the sidebar
    // does not, and only when it would actually say something.
    let strip_wanted = !is_idle
        && frame.area().width < SIDEBAR_MIN_TERMINAL_WIDTH
        && (app.context.is_some() || !strip_extras(app).is_empty());

    let layout = vertical_layout(
        frame.area().height,
        wanted_input_rows(app, frame.area()),
        wanted_suggest,
        strip_wanted,
    );
    let [message_area, strip_area, suggest_area, input_area, footer_area] = Layout::vertical([
        Constraint::Length(layout.messages),
        Constraint::Length(layout.strip),
        Constraint::Length(layout.suggest),
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

/// Rows the input box actually *gets* once `vertical_layout` has protected
/// the transcript's floor. `draw` composes the two itself (it also has a
/// slash list and a strip to place); this is the two-step version the prompt's
/// own tests read.
#[cfg(test)]
fn input_height(app: &mut App, frame_area: Rect) -> u16 {
    let wanted = wanted_input_rows(app, frame_area);
    vertical_layout(frame_area.height, wanted, 0, false).input
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
    } else if let Some(cost) =
        crate::pricing::estimate_cost_usd(&app.provider_label, &app.model_label, &app.usage)
    {
        parts.push(format!("~${cost:.4}"));
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
                Span::styled("· ", theme.disabled()),
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
                Span::styled("… ", theme.disabled()),
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

/// The sidebar, split around the one widget in it that isn't a `Line`.
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
        .border_style(theme.disabled());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;

    let (above, below) = sidebar_lines(app);
    let above = fit_lines(above, width);
    let below = fit_lines(below, width);

    let gauge_rows = u16::from(app.context.is_some());
    let [above_area, gauge_area, below_area] = Layout::vertical([
        Constraint::Length(above.len() as u16),
        Constraint::Length(gauge_rows),
        Constraint::Min(0),
    ])
    .areas(inner);

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

/// Sidebar content, split at the row the context gauge occupies.
fn sidebar_lines(app: &App) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
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

    lines.extend([Line::from(""), head("CONTEXT")]);

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

    below.push(Line::from(""));
    if let Some(stats) = &app.resources {
        below.extend(resource_lines(stats, theme));
    } else if let Some(cost) =
        crate::pricing::estimate_cost_usd(&app.provider_label, &app.model_label, &app.usage)
    {
        below.push(head("COST"));
        below.push(Line::from(Span::styled(
            format!("~${cost:.4} (est.)"),
            theme.text(),
        )));
    }

    (lines, below)
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
        let frames = theme.spinner_frames();
        let spinner = frames[app.spinner_frame % frames.len()];
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

    let inner_width = width.saturating_sub(2);
    let block = |scrollable: bool| {
        let block = Block::default()
            .borders(Borders::ALL)
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

    // `line_count` already includes the block's two border rows, so it is the
    // full popup height the content wants — clamp that, don't grow it again.
    let content_height = paragraph.line_count(inner_width) as u16;
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

        let running =
            tool_card(&theme, &search(ActivityStatus::Running), 60, false, 0)[0].to_string();
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
        let text = screen_text(&terminal);
        assert!(text.is_ascii(), "non-ASCII bytes in 80x24 ASCII render: {text:?}");
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
            theme: Theme::ansi(),
            goal: None,
            tasks: Vec::new(),
            commands: crate::slash::SlashRegistry::builtin(),
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
        let mut app = app_for_input_tests();
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

        let mut app = app_for_input_tests();
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
                    format!("pergunta {i} — {}", "palavra ".repeat(12)),
                )),
                1 => app.lines.push(ChatLine::new(
                    ChatRole::Assistant,
                    format!("## resposta {i}\n\ncom `código` e **negrito**\n\n- um\n- dois"),
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
            let mut cached = app_for_input_tests();
            long_transcript(&mut cached, 200);
            let mut legacy = app_for_input_tests();
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
        let mut cached = app_for_input_tests();
        long_transcript(&mut cached, 40);
        let mut legacy = app_for_input_tests();
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

        let mut app = app_for_input_tests();
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

        let mut app = app_for_input_tests();
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
        let plain = vertical_layout(24, INPUT_MAX_ROWS, 0, false);
        assert_eq!(plain.status, 1);
        assert_eq!(plain.input, INPUT_MAX_ROWS);
        assert_eq!(plain.messages, 13);

        // Slash list open — the case that used to squeeze the transcript,
        // because the prompt grew against `height - 3` and the list was
        // simply subtracted from whatever was left.
        let typing = vertical_layout(24, INPUT_MAX_ROWS, 7, false);
        assert_eq!(typing.suggest, 7);
        assert_eq!(typing.messages, TRANSCRIPT_MIN_ROWS);
        assert_eq!(typing.input, 8);

        for l in [plain, typing] {
            assert_eq!(l.messages + l.strip + l.suggest + l.input + l.status, 24);
        }
    }

    #[test]
    fn the_strip_costs_the_transcript_one_row_and_only_above_20() {
        let with = vertical_layout(24, INPUT_MIN_ROWS, 0, true);
        let without = vertical_layout(24, INPUT_MIN_ROWS, 0, false);
        assert_eq!(with.strip, 1);
        assert_eq!(with.messages + 1, without.messages);

        // Too short to spend a row on vitals.
        assert_eq!(vertical_layout(19, INPUT_MIN_ROWS, 0, true).strip, 0);
    }

    #[test]
    fn the_slash_list_may_borrow_from_the_floor_but_nothing_else_may() {
        // 10 rows: status 1 + prompt 3 leaves 6, less than the 8-row floor.
        let short = vertical_layout(10, INPUT_MAX_ROWS, 7, false);
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
            let l = vertical_layout(height, INPUT_MAX_ROWS, 7, true);
            assert_eq!(
                l.messages + l.strip + l.suggest + l.input + l.status,
                height,
                "height {height} over-allocated"
            );
            assert!(l.input <= height, "height {height}");
        }
    }

    fn app_with_context(width: u16) -> App {
        let mut app = app_for_input_tests();
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

        let mut app = app_for_input_tests();
        app.theme = app.theme.clone().ascii_glyphs();
        long_transcript(&mut app, 60);
        app.on_agent_event(smith_core::AgentEvent::ContextUsage {
            used: 120_000,
            window: 128_000,
            estimated: true,
        });

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
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

        let mut app = app_for_input_tests();
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
        let mut app = app_for_input_tests();
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
        let mut app = app_for_input_tests();
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

        let mut app = app_for_input_tests();
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
        let mut app = app_for_input_tests();
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
}
