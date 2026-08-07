//! The vertical scrollbar every scrollable surface shares.
//!
//! # Why a scrollbar at all, when the content already scrolls
//!
//! smith could scroll long before this existed — wheel, `PageUp`/`PageDown`,
//! a follow-the-tail flag. What it could not do was answer *where am I*. A
//! transcript that has silently scrolled off the top is indistinguishable
//! from one that starts there, and a permission modal whose command runs past
//! the bottom edge looks exactly like one that ends at the edge. The track is
//! the only thing on screen that carries both facts at once: that there is
//! more, and how much of it you are looking at.
//!
//! # Why it draws nothing when everything fits
//!
//! A full-height thumb is a control that cannot be moved, and a column of
//! track beside content that ends on screen is the widget claiming the
//! surface scrolls when it does not. Both teach the user to ignore the
//! column, which costs the signal in the case that matters.

use ratatui::layout::{Margin, Rect};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use crate::theme::Theme;

/// Columns a visible scrollbar occupies. Callers that reserve space for one
/// (rather than drawing it over a border) subtract this from their content
/// width — see `ui::message`.
pub const SCROLLBAR_WIDTH: u16 = 1;

/// Whether a surface of `viewport_height` rows showing `content_height` rows
/// of content has anything to scroll.
///
/// The single predicate both the drawing and the space-reservation decisions
/// go through, so a reserved column and a drawn track can never disagree.
pub fn is_scrollable(content_height: u16, viewport_height: u16) -> bool {
    content_height > viewport_height && viewport_height > 0
}

/// Draws the track down the rightmost column of `area`.
///
/// `offset` is in the same rows as `content_height` — the caller's own scroll
/// offset, unclamped is fine, since the state clamps its own position.
pub fn vertical(
    frame: &mut Frame,
    area: Rect,
    content_height: u16,
    viewport_height: u16,
    offset: u16,
    theme: &Theme,
) {
    if !is_scrollable(content_height, viewport_height) || area.width == 0 || area.height == 0 {
        return;
    }
    let (thumb, track) = theme.scrollbar_symbols();
    // Both halves carry an explicit background. The scrollbar is the one
    // widget routinely drawn over a `Clear`ed modal, and `Clear` resets a cell
    // to the terminal's own colours — a fg-only style would leave a stripe of
    // whatever the user's terminal is set to running down the surface.
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_symbol(thumb)
        .thumb_style(theme.ember().bg(theme.overlay))
        .track_symbol(Some(track))
        .track_style(theme.disabled().bg(theme.overlay))
        .begin_symbol(None)
        .end_symbol(None);

    let mut state = ScrollbarState::new(content_height as usize)
        .viewport_content_length(viewport_height as usize)
        .position(offset.min(content_height.saturating_sub(1)) as usize);

    frame.render_stateful_widget(bar, area, &mut state);
}

/// The same, for a surface inside a bordered block: the track replaces the
/// right border between the two corners.
///
/// Insetting by one row vertically is what keeps the corners intact — a
/// scrollbar drawn over them turns a closed box into one with two notches,
/// and the frame stops reading as a frame.
pub fn framed(
    frame: &mut Frame,
    outer: Rect,
    content_height: u16,
    viewport_height: u16,
    offset: u16,
    theme: &Theme,
) {
    if outer.height < 3 {
        return;
    }
    vertical(
        frame,
        outer.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        content_height,
        viewport_height,
        offset,
        theme,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(content: u16, viewport: u16, offset: u16, unicode: bool) -> String {
        let theme = crate::theme::Theme::preset(crate::theme::ThemeName::Dark, true);
        let theme = if unicode { theme } else { theme.ascii_glyphs() };
        let mut terminal = Terminal::new(TestBackend::new(4, viewport)).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 4, viewport);
                vertical(f, area, content, viewport, offset, &theme);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..viewport)
            .map(|y| buf.cell((3, y)).unwrap().symbol().to_string())
            .collect()
    }

    #[test]
    fn content_that_fits_draws_no_track_at_all() {
        assert_eq!(render(4, 4, 0, true), "    ");
        assert_eq!(render(2, 4, 0, true), "    ");
    }

    /// The two facts the track exists to carry: there is more, and this is
    /// where you are in it.
    #[test]
    fn the_thumb_sits_at_the_top_at_offset_zero_and_the_bottom_at_the_end() {
        let top = render(40, 4, 0, true);
        assert!(top.starts_with('█'), "{top}");
        assert!(top.ends_with('│'), "{top}");

        let bottom = render(40, 4, 36, true);
        assert!(bottom.ends_with('█'), "{bottom}");
        assert!(bottom.starts_with('│'), "{bottom}");
    }

    #[test]
    fn the_ascii_track_stays_ascii() {
        let bar = render(40, 4, 0, false);
        assert!(bar.is_ascii(), "{bar}");
        assert!(bar.contains('#') && bar.contains('|'), "{bar}");
    }

    /// A framed track never eats the corners, so the box stays closed.
    #[test]
    fn a_framed_track_leaves_the_corner_rows_alone() {
        let theme = crate::theme::Theme::preset(crate::theme::ThemeName::Dark, true);
        let mut terminal = Terminal::new(TestBackend::new(4, 6)).unwrap();
        terminal
            .draw(|f| framed(f, Rect::new(0, 0, 4, 6), 60, 4, 0, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(buf.cell((3, 0)).unwrap().symbol(), " ", "top corner row");
        assert_eq!(buf.cell((3, 5)).unwrap().symbol(), " ", "bottom corner row");
        assert_eq!(buf.cell((3, 1)).unwrap().symbol(), "█");
    }
}
