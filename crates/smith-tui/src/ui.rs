use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Clear};
use ratatui::Frame;

use crate::app::{App, Modal};
use crate::components::input::INPUT_MIN_ROWS;

use chrome::{
    draw_context_strip, draw_idle, draw_input, draw_overlay, draw_queue, draw_slash_suggestions,
    draw_status_bar, strip_extras, MAX_QUEUE_ROWS,
};
use message::draw_messages;
use modals::{draw_model_picker, draw_permission_modal, draw_plan_modal, draw_question_modal};
use sidebar::draw_sidebar;

// `transcript.rs` renders a line the same way the transcript does.
pub(crate) use message::{render_chat_line, render_in_flight};

mod card;
mod chrome;
mod message;
mod modals;
mod sidebar;

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

/// Splits a transcript pane into the text column and the one-column gutter
/// its scrollbar lives in.
///
/// The gutter is reserved whether or not a bar is currently drawn in it, and
/// that is the whole point. Handing the column back whenever the content
/// happens to fit would make the text width depend on the scroll state — and
/// the render memo is keyed on width, so the entire transcript would re-wrap
/// and every line on screen would shift sideways at the exact moment the
/// content grew past the viewport, which is the moment the user is reading
/// new output.
///
/// Splitting here rather than inside `draw_messages` keeps that function
/// taking the rect it draws text into, which is what
/// `tests::legacy_draw_messages` compares against cell for cell.
fn transcript_panes(area: Rect) -> (Rect, Rect) {
    let gutter_width = crate::components::scrollbar::SCROLLBAR_WIDTH;
    if area.width <= gutter_width {
        return (area, Rect { width: 0, ..area });
    }
    let [content, gutter] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(gutter_width)]).areas(area);
    (content, gutter)
}

/// Draws the transcript and, beside it, how much of it is off screen.
fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let (content, gutter) = transcript_panes(area);
    draw_messages(frame, app, content);
    // Read back after the draw: `draw_messages` is what clamps `scroll` and
    // re-arms follow-the-tail, and the memo is what knows the height. Asking
    // before it ran would put the bar one frame behind the text.
    let content_height = u16::try_from(app.transcript.total_height()).unwrap_or(u16::MAX);
    crate::components::scrollbar::vertical(
        frame,
        gutter,
        content_height,
        content.height,
        app.scroll,
        &app.theme.clone(),
    );
}

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
            draw_transcript(frame, app, clamp_width(chat_area, MAX_CONTENT_WIDTH));
        }
        draw_sidebar(frame, &*app, sidebar_area);
    } else if modal_open {
        frame.render_widget(Clear, message_area);
        frame.render_widget(Block::new().style(theme.base_bg()), message_area);
    } else {
        draw_transcript(frame, app, clamp_width(message_area, MAX_CONTENT_WIDTH));
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
        Modal::Model(modal) => draw_model_picker(frame, modal, &theme, frame.area()),
        Modal::Plan(modal) => draw_plan_modal(frame, modal, &theme, frame.area()),
        Modal::Permission(modal) => draw_permission_modal(frame, modal, &theme, frame.area()),
        Modal::None => {
            if let Some(overlay) = app.overlay.as_mut() {
                draw_overlay(frame, overlay, &theme, frame.area());
            }
        }
    }
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

#[cfg(test)]
mod tests;
