//! Unified-diff rendering for `edit_file` tool cards.
//!
//! Uses the `similar` crate (already a workspace dep) to compute a line-level
//! diff between `old_str` and `new_str`, then paints each change with the
//! design system's diff tokens: `+` on `diff_add_bg`, `-` on
//! `diff_del_bg`, unchanged lines dimmed on the overlay surface.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};

use crate::theme::Theme;

/// Render a unified diff of `old` vs `new`, each output line padded to
/// `width` with the appropriate background (add/del/overlay).
pub fn render_diff(old: &str, new: &str, theme: &Theme, width: usize) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();

    for change in diff.iter_all_changes() {
        let (sign, style) = match change.tag() {
            ChangeTag::Delete => ("-", Style::default().fg(theme.danger).bg(theme.diff_del_bg)),
            ChangeTag::Insert => (
                "+",
                Style::default().fg(theme.success).bg(theme.diff_add_bg),
            ),
            ChangeTag::Equal => (" ", Style::default().fg(theme.disabled).bg(theme.overlay)),
        };
        let text = change.to_string();
        // Strip trailing newline for display — the diff is line-oriented
        // already, so the newline is structural, not content.
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        let row = format!("{sign}  {text}");
        let used = row.chars().count();
        let mut spans = vec![Span::styled(row, style)];
        if width > used {
            spans.push(Span::styled(" ".repeat(width - used), style));
        }
        out.push(Line::from(spans));
    }

    if out.is_empty() {
        out.push(Line::from(Span::styled(
            "(no changes)",
            theme.disabled().bg(theme.overlay),
        )));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_diff_marks_additions_and_deletions() {
        let theme = Theme::ansi();
        let lines = render_diff("hello\nworld\n", "hello\nrust\n", &theme, 40);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("+  rust"), "expected added line:\n{text}");
        assert!(text.contains("-  world"), "expected removed line:\n{text}");
        assert!(
            text.contains("   hello"),
            "expected unchanged line:\n{text}"
        );
    }

    #[test]
    fn render_diff_handles_identical_strings() {
        let theme = Theme::ansi();
        let lines = render_diff("same\n", "same\n", &theme, 20);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("   same"));
    }
}
