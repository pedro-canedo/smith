use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use tui_markdown::{from_str_with_options, Options, StyleSheet};

use crate::theme::Theme;

/// Project stylesheet, styled from the Ember design tokens: bold primary
/// headings, amber inline code on an overlay surface, dim math (the terminal
/// can't render LaTeX — keep delimiters readable).
#[derive(Clone, Debug)]
struct SmithStyleSheet {
    theme: Theme,
}

impl StyleSheet for SmithStyleSheet {
    fn heading(&self, level: u8) -> Style {
        let base = self.theme.bold();
        match level {
            1 => base.add_modifier(Modifier::UNDERLINED),
            3..=6 => base.add_modifier(Modifier::ITALIC),
            _ => base,
        }
    }

    fn code(&self) -> Style {
        Style::default().fg(self.theme.amber).bg(self.theme.overlay)
    }

    /// Keep the `#` prefix (`docs/design-system.md` §2.2): stripped entirely,
    /// a heading is only distinguishable from bold body text, which reads as
    /// emphasis rather than structure.
    fn heading_marker(&self, level: u8) -> &str {
        match level {
            1 => "# ",
            2 => "## ",
            3 => "### ",
            4 => "#### ",
            5 => "##### ",
            _ => "###### ",
        }
    }

    fn code_block_fence(&self) -> &str {
        ""
    }

    fn math_inline(&self) -> Style {
        self.theme.disabled().add_modifier(Modifier::ITALIC)
    }

    fn math_display(&self) -> Style {
        self.theme.disabled()
    }

    fn table_header(&self) -> Style {
        self.theme.info_bold()
    }

    fn table_border(&self) -> Style {
        self.theme.disabled()
    }
}

fn options(theme: &Theme) -> Options<SmithStyleSheet> {
    Options::new(SmithStyleSheet {
        theme: theme.clone(),
    })
}

// Counts `render` calls so the transcript memo's effectiveness can be
// asserted on rather than argued about. Thread-local, so parallel tests
// don't see each other's parses.
#[cfg(test)]
thread_local! {
    static RENDER_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn render_calls() -> usize {
    RENDER_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub fn reset_render_calls() {
    RENDER_CALLS.with(|c| c.set(0));
}

/// Renders markdown into owned ratatui lines (tables, headings, emphasis, code).
///
/// This is the single most expensive thing the transcript does, which is why
/// `crate::transcript` exists to call it once per message rather than once per
/// message per frame.
pub fn render(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    #[cfg(test)]
    RENDER_CALLS.with(|c| c.set(c.get() + 1));
    let rendered = from_str_with_options(text, &options(theme));
    owned_lines(rendered)
}

fn owned_lines(text: Text<'_>) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = text
        .lines
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.to_string(), span.style))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_theme() -> Theme {
        Theme::ansi()
    }

    fn plain(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_plain_paragraph() {
        let lines = render("hello world", &test_theme());
        assert_eq!(plain(&lines), "hello world");
    }

    #[test]
    fn renders_inline_code_as_text() {
        let lines = render("run `cargo test` now", &test_theme());
        assert_eq!(plain(&lines), "run cargo test now");
    }

    #[test]
    fn renders_fenced_code_block_verbatim() {
        let lines = render("```rust\nfn main() {}\n```", &test_theme());
        let text = plain(&lines);
        assert!(text.contains("fn main() {}"));
    }

    #[test]
    fn renders_table_as_multiple_bordered_lines() {
        let md = "| Problem | Status |\n| --- | --- |\n| Gravity | Open |";
        let lines = render(md, &test_theme());
        assert!(
            lines.len() >= 3,
            "expected multi-line table, got {} lines:\n{}",
            lines.len(),
            plain(&lines)
        );
        let text = plain(&lines);
        assert!(text.contains("Problem"), "{text}");
        assert!(text.contains("Gravity"), "{text}");
        // Unicode box-drawing from tui-markdown
        assert!(
            text.contains('│') || text.contains('|') || text.contains('┌'),
            "expected table borders in:\n{text}"
        );
    }

    #[test]
    fn never_returns_empty() {
        assert!(!render("", &test_theme()).is_empty());
    }
}
