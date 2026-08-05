//! Width-safe wrapping and truncation for *styled* lines.
//!
//! The transcript is a single `Paragraph` with `Wrap { trim: false }`, so any
//! line wider than the pane gets soft-wrapped by ratatui — which is fine for
//! prose but destroys a box: the closing `│` lands on its own row and the
//! top/bottom rules end up a different length than the body. That was the
//! source of the misaligned user bubble.
//!
//! The fix is to make sure nothing ever exceeds the target width in the first
//! place. Since the content is already styled (`markdown::render` hands back
//! `Vec<Line>`), these work on spans rather than `&str` — flattening to text
//! would drop every style.
//!
//! Widths are measured in display cells, not chars: `ção` is 3 cells but 5
//! bytes, and `日本語` is 6 cells in 3 chars.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Cut `spans` down to at most `width` display cells, splitting a span mid-way
/// if that's where the limit falls. Used by the box builders, whose geometry
/// has to be exact.
pub fn truncate_spans(spans: &[Span<'static>], width: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for span in spans {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let span_width = span.width();
        if span_width <= remaining {
            used += span_width;
            out.push(span.clone());
            continue;
        }
        let mut taken = String::new();
        let mut taken_width = 0usize;
        for c in span.content.chars() {
            let cw = char_width(c);
            if taken_width + cw > remaining {
                break;
            }
            taken.push(c);
            taken_width += cw;
        }
        if !taken.is_empty() {
            out.push(Span::styled(taken, span.style));
        }
        break;
    }
    out
}

/// Word-wrap `spans` to `width` display cells, preserving each span's style
/// across the break. Words longer than `width` (a path, a URL) are hard-broken
/// rather than allowed to overflow.
pub fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let chars: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();

    if width == 0 || chars.is_empty() {
        return vec![spans.to_vec()];
    }

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut row_width = 0usize;
    // Where the current row could be broken without splitting a word.
    let mut last_break: Option<usize> = None;

    for &(c, style) in &chars {
        if c == '\n' {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
            last_break = None;
            continue;
        }
        let cw = char_width(c);
        if row_width + cw > width && !row.is_empty() {
            match last_break {
                Some(at) => {
                    let mut carry = row.split_off(at);
                    while carry.first().is_some_and(|(c, _)| c.is_whitespace()) {
                        carry.remove(0);
                    }
                    rows.push(std::mem::take(&mut row));
                    row = carry;
                }
                None => rows.push(std::mem::take(&mut row)),
            }
            row_width = row.iter().map(|&(c, _)| char_width(c)).sum();
            last_break = None;
        }
        if c.is_whitespace() {
            last_break = Some(row.len());
        }
        row.push((c, style));
        row_width += cw;
    }
    rows.push(row);

    rows.into_iter().map(regroup).collect()
}

/// Rebuild spans from `(char, style)` pairs, merging runs that share a style
/// so we don't emit one span per character.
fn regroup(chars: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut current = String::new();
    let mut current_style: Option<Style> = None;

    for (c, style) in chars {
        match current_style {
            Some(s) if s == style => current.push(c),
            Some(s) => {
                out.push(Span::styled(std::mem::take(&mut current), s));
                current.push(c);
                current_style = Some(style);
            }
            None => {
                current.push(c);
                current_style = Some(style);
            }
        }
    }
    if let Some(s) = current_style {
        out.push(Span::styled(current, s));
    }
    out
}

/// `wrap_spans` for a whole `Line`. A line that already fits comes back
/// untouched, so this is cheap to apply defensively.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line.clone()];
    }
    wrap_spans(&line.spans, width)
        .into_iter()
        .map(Line::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn plain(text: &str) -> Vec<Span<'static>> {
        vec![Span::raw(text.to_string())]
    }

    fn rendered(rows: &[Vec<Span<'static>>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wraps_at_word_boundaries() {
        let rows = wrap_spans(&plain("the quick brown fox"), 10);
        assert_eq!(rendered(&rows), vec!["the quick", "brown fox"]);
    }

    #[test]
    fn hard_breaks_a_word_longer_than_the_width() {
        let rows = wrap_spans(&plain("supercalifragilistic"), 8);
        assert!(rows.len() > 1);
        for row in &rows {
            let width: usize = row.iter().map(|s| s.width()).sum();
            assert!(width <= 8, "row exceeded width: {width}");
        }
    }

    #[test]
    fn keeps_explicit_newlines_as_row_breaks() {
        let rows = wrap_spans(&plain("one\ntwo"), 40);
        assert_eq!(rendered(&rows), vec!["one", "two"]);
    }

    #[test]
    fn measures_accents_by_display_cell_not_byte() {
        // "configuração" is 12 cells but 14 bytes — a byte-based wrap would
        // break it two cells early.
        let rows = wrap_spans(&plain("configuração"), 12);
        assert_eq!(rows.len(), 1, "should fit in exactly 12 cells");
    }

    #[test]
    fn counts_wide_characters_as_two_cells() {
        let rows = wrap_spans(&plain("日本語"), 4);
        assert_eq!(rows.len(), 2, "6 cells can't fit in 4");
    }

    #[test]
    fn preserves_span_styles_across_a_break() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let rows = wrap_spans(
            &[
                Span::styled("hello ".to_string(), red),
                Span::styled("world again".to_string(), blue),
            ],
            11,
        );
        assert!(rows.len() >= 2);
        assert_eq!(rows[0][0].style, red);
        assert!(rows
            .iter()
            .flatten()
            .any(|s| s.style == blue && s.content.contains("world")));
    }

    #[test]
    fn merges_adjacent_chars_that_share_a_style() {
        let rows = wrap_spans(&plain("abc def"), 40);
        assert_eq!(rows[0].len(), 1, "one style should mean one span");
    }

    #[test]
    fn truncate_never_exceeds_the_target_width() {
        let spans = vec![
            Span::raw("aaaa".to_string()),
            Span::raw("bbbbbbbbbb".to_string()),
        ];
        let out = truncate_spans(&spans, 6);
        let width: usize = out.iter().map(|s| s.width()).sum();
        assert_eq!(width, 6);
    }

    #[test]
    fn truncate_leaves_content_that_already_fits() {
        let spans = plain("short");
        assert_eq!(truncate_spans(&spans, 40), spans);
    }

    #[test]
    fn wrap_line_returns_short_lines_untouched() {
        let line = Line::from("short");
        assert_eq!(wrap_line(&line, 40).len(), 1);
    }

    #[test]
    fn wrap_line_splits_a_long_line_within_width() {
        let line = Line::from("the quick brown fox jumps over the lazy dog");
        for wrapped in wrap_line(&line, 15) {
            assert!(wrapped.width() <= 15, "got {}", wrapped.width());
        }
    }
}
