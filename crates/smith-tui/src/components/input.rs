//! The prompt's text entry, a thin shell over `tui_textarea::TextArea`.
//!
//! Everything the rest of the TUI knows about text entry goes through this
//! type — `app.rs` and `ui.rs` never name `TextArea`. That keeps the widget
//! swappable, and it keeps `tui-textarea`'s own styling defaults from leaking
//! past the Ember tokens (no `Color::` outside `theme.rs`).
//!
//! The three behaviours the old `String` buffer couldn't do, all of which the
//! widget gives us for free: soft word wrap, a box that grows with the content
//! up to a cap and then scrolls, and a caret that can move through the text
//! instead of only appending at the end.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};
use tui_textarea::{CursorMove, CursorRenderMode, Input, TextArea, WrapMode};

use crate::theme::Theme;

/// Total rows the input box may occupy, borders included. Past this the box
/// stops growing and scrolls instead, keeping the caret in view.
pub const INPUT_MAX_ROWS: u16 = 10;
/// Rows the box occupies when empty: one text row between two borders.
pub const INPUT_MIN_ROWS: u16 = 3;

pub struct TextInput {
    area: TextArea<'static>,
}

impl TextInput {
    pub fn new(theme: &Theme) -> Self {
        let mut area = TextArea::default();
        // Wrap at word boundaries, but hard-break a single word too long to
        // fit — otherwise an unbroken path or URL would scroll off sideways,
        // which is the bug we're fixing.
        area.set_wrap_mode(WrapMode::WordOrGlyph);
        area.set_min_rows(INPUT_MIN_ROWS);
        area.set_max_rows(INPUT_MAX_ROWS);
        // We draw the real terminal caret via `frame.set_cursor_position`; a
        // buffer cell painted to look like a cursor doesn't blink and reads as
        // a selection rather than an insertion point.
        area.set_cursor_render_mode(CursorRenderMode::Hidden);
        area.remove_line_number();
        area.set_style(theme.text());
        // The crate underlines the cursor's line by default.
        area.set_cursor_line_style(Style::default());
        area.set_placeholder_style(theme.disabled());
        // A block must be set before the first `outer_rows` call: `measure`
        // derives the chrome height from it, and the row budget is counted in
        // outer rows. `draw_input` replaces this each frame with the titled,
        // mode-coloured version — same borders, same height.
        area.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(theme.block_border_set()),
        );
        Self { area }
    }

    pub fn text(&self) -> String {
        self.area.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.area.lines().iter().all(|l| l.is_empty())
    }

    /// Replace the whole buffer and park the caret at the end — what slash
    /// tab-completion wants.
    pub fn set(&mut self, text: &str) {
        let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        self.area.set_lines(lines, (0, 0));
        self.area.move_cursor(CursorMove::Bottom);
        self.area.move_cursor(CursorMove::End);
    }

    /// Hand back the text and clear the box — the `mem::take` of the old
    /// `String` buffer.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.area.set_lines(vec![String::new()], (0, 0));
        text
    }

    /// Feed a key to the editor. Returns whether it was consumed, so callers
    /// can fall through to another binding when it wasn't.
    pub fn handle(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        self.area.input(Input::from(KeyEvent::new(code, modifiers)))
    }

    pub fn insert_newline(&mut self) {
        self.area.insert_newline();
    }

    /// Insert text at the caret without treating newlines as submissions —
    /// the bracketed-paste path.
    pub fn insert_str(&mut self, text: &str) {
        self.area.insert_str(text);
    }

    /// Move the caret one visual row up. `false` means it was already on the
    /// first row, so the caller should use the key for something else.
    pub fn move_up(&mut self) -> bool {
        if self.area.cursor().0 == 0 {
            return false;
        }
        self.area.move_cursor(CursorMove::Up);
        true
    }

    /// Move the caret one visual row down; `false` when already on the last.
    pub fn move_down(&mut self) -> bool {
        if self.area.cursor().0 + 1 >= self.area.lines().len() {
            return false;
        }
        self.area.move_cursor(CursorMove::Down);
        true
    }

    /// Rows the box wants at this outer width, borders included, already
    /// clamped to `INPUT_MIN_ROWS..=INPUT_MAX_ROWS` by the widget.
    pub fn outer_rows(&mut self, outer_width: u16) -> u16 {
        self.area.measure(outer_width).preferred_rows
    }

    /// Where the caret landed in the last render. `None` before the first
    /// draw, or when the widget had no room.
    pub fn cursor_position(&self) -> Option<Position> {
        self.area.rendered_cursor_position()
    }

    pub fn set_block(&mut self, block: Block<'static>) {
        self.area.set_block(block);
    }

    pub fn set_placeholder(&mut self, text: &str) {
        self.area.set_placeholder_text(text);
    }

    /// Text style — swapped to `info` while the buffer holds a slash command.
    pub fn set_style(&mut self, style: Style) {
        self.area.set_style(style);
    }

    pub fn widget(&self) -> &TextArea<'static> {
        &self.area
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> TextInput {
        TextInput::new(&Theme::truecolor())
    }

    fn type_str(t: &mut TextInput, s: &str) {
        for c in s.chars() {
            t.handle(KeyCode::Char(c), KeyModifiers::NONE);
        }
    }

    #[test]
    fn typing_then_taking_round_trips_the_text() {
        let mut t = input();
        assert!(t.is_empty());
        type_str(&mut t, "hello");
        assert_eq!(t.text(), "hello");
        assert_eq!(t.take(), "hello");
        assert!(t.is_empty());
    }

    #[test]
    fn caret_moves_left_and_inserts_mid_string() {
        let mut t = input();
        type_str(&mut t, "helo");
        t.handle(KeyCode::Left, KeyModifiers::NONE);
        t.handle(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(t.text(), "hello");
    }

    #[test]
    fn caret_steps_by_character_not_byte_over_accents() {
        // "ação" is 4 chars but 6 bytes — the old `String::pop` buffer had no
        // caret at all, and a byte-indexed one would split the 'ç'.
        let mut t = input();
        type_str(&mut t, "ação");
        for _ in 0..4 {
            t.handle(KeyCode::Left, KeyModifiers::NONE);
        }
        t.handle(KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(t.text(), "ração");
    }

    #[test]
    fn backspace_deletes_the_whole_multibyte_char() {
        let mut t = input();
        type_str(&mut t, "ação");
        t.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(t.text(), "açã");
        t.handle(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(t.text(), "aç", "the 2-byte 'ã' goes as one unit");
    }

    #[test]
    fn home_and_end_move_within_the_current_line() {
        let mut t = input();
        type_str(&mut t, "world");
        t.handle(KeyCode::Home, KeyModifiers::NONE);
        type_str(&mut t, "hello ");
        assert_eq!(t.text(), "hello world");
        t.handle(KeyCode::End, KeyModifiers::NONE);
        type_str(&mut t, "!");
        assert_eq!(t.text(), "hello world!");
    }

    #[test]
    fn ctrl_w_deletes_the_word_before_the_caret() {
        let mut t = input();
        type_str(&mut t, "delete this");
        t.handle(KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(t.text(), "delete ");
    }

    #[test]
    fn newline_is_explicit_and_does_not_come_from_typing() {
        let mut t = input();
        type_str(&mut t, "one");
        t.insert_newline();
        type_str(&mut t, "two");
        assert_eq!(t.text(), "one\ntwo");
    }

    #[test]
    fn set_replaces_buffer_and_parks_caret_at_the_end() {
        let mut t = input();
        type_str(&mut t, "/pl");
        t.set("/plan ");
        type_str(&mut t, "x");
        assert_eq!(t.text(), "/plan x");
    }

    #[test]
    fn box_grows_with_content_then_stops_at_the_cap() {
        let mut t = input();
        assert_eq!(t.outer_rows(40), INPUT_MIN_ROWS);

        type_str(&mut t, &"palavra ".repeat(10));
        let grown = t.outer_rows(40);
        assert!(
            grown > INPUT_MIN_ROWS && grown <= INPUT_MAX_ROWS,
            "expected the box to grow, got {grown} rows"
        );

        type_str(&mut t, &"palavra ".repeat(200));
        assert_eq!(t.outer_rows(40), INPUT_MAX_ROWS);
    }

    #[test]
    fn long_text_is_wrapped_not_dropped() {
        let mut t = input();
        let typed = "a".repeat(300);
        type_str(&mut t, &typed);
        // The whole thing is still in the buffer — the old Paragraph silently
        // clipped everything past the box width.
        assert_eq!(t.text().chars().count(), 300);
    }

    #[test]
    fn move_up_and_down_report_when_they_hit_the_edge() {
        let mut t = input();
        assert!(!t.move_up(), "single row: nothing above");
        assert!(!t.move_down(), "single row: nothing below");

        type_str(&mut t, "one");
        t.insert_newline();
        type_str(&mut t, "two");
        assert!(t.move_up());
        assert!(!t.move_up());
        assert!(t.move_down());
        assert!(!t.move_down());
    }

    #[test]
    fn insert_str_keeps_pasted_newlines() {
        let mut t = input();
        t.insert_str("line one\nline two");
        assert_eq!(t.text(), "line one\nline two");
    }
}
