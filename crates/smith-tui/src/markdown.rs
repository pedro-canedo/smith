use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use tui_markdown::{from_str_with_options, Options, StyleSheet};

use crate::highlight;
use crate::theme::Theme;

/// Marker `tui-markdown` is asked to emit around every fenced code block, so
/// the block's extent and its info string can be recovered from the rendered
/// `Vec<Line>` — the crate offers no per-block hook, and its `StyleSheet` is
/// consulted for exactly one string per fence.
///
/// Both marker lines are removed again by [`highlight_code_blocks`], so nothing
/// reaches the transcript. It is deliberately not a plausible fragment of prose:
/// a stray match would fold ordinary text into a code block.
const FENCE_SENTINEL: &str = "\u{1}smith\u{1}";

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
        FENCE_SENTINEL
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
    owned_lines(rendered, theme)
}

fn owned_lines(text: Text<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let lines: Vec<Line<'static>> = text
        .lines
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|span| {
                        let content = if theme.unicode {
                            span.content.to_string()
                        } else {
                            ascii_markdown_glyphs(&span.content)
                        };
                        Span::styled(content, span.style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let mut lines = highlight_code_blocks(lines, theme);
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// `(info string, count of leading prefix spans)` when this line is one of the
/// sentinel fence markers. The prefix is whatever the block is nested in — a
/// blockquote contributes `">"` and `" "` spans ahead of every line it holds.
fn fence_marker(line: &Line<'_>) -> Option<(String, usize)> {
    let last = line.spans.last()?;
    let info = last.content.strip_prefix(FENCE_SENTINEL)?;
    Some((info.to_string(), line.spans.len() - 1))
}

/// Replaces each fenced block with syntax-highlighted lines and drops the
/// sentinel markers around it.
///
/// Markers alternate open/close (code blocks cannot nest), so tracking which
/// one we are on needs no bookkeeping beyond the loop. A block whose language
/// is unknown keeps its text exactly as it was.
///
/// One subtlety earns its comment: **inside a block, one span is one source
/// line**. `tui-markdown` pushes a span per text event and only starts a new
/// `Line` when it thinks it needs one, so a fence inside a list item arrives as
/// several spans on a single line. Rebuilding the block from spans rather than
/// from lines is what keeps that case right.
fn highlight_code_blocks(mut lines: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
    if !lines.iter().any(|line| fence_marker(line).is_some()) {
        return lines;
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let Some((info, prefix_len)) = fence_marker(&lines[i]) else {
            out.push(std::mem::take(&mut lines[i]));
            i += 1;
            continue;
        };
        // The block runs to the closing marker, or to the end of the message
        // while the model is still streaming it.
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && fence_marker(&lines[end]).is_none() {
            end += 1;
        }
        let prefix: Vec<Span<'static>> = lines[i].spans[..prefix_len].to_vec();
        let mut code: Vec<Span<'static>> = Vec::new();
        for line in &lines[start..end] {
            let before = code.len();
            code.extend(line.spans.iter().skip(prefix_len).cloned());
            if code.len() == before {
                // An empty line still is a line.
                code.push(Span::raw(""));
            }
        }
        if !code.is_empty() {
            let source = code
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>()
                .join("\n");
            match highlight::highlight(&source, &info, theme) {
                Some(highlighted) => out.extend(
                    highlighted
                        .into_iter()
                        .map(|line| with_prefix(&prefix, line)),
                ),
                None => out.extend(
                    code.into_iter()
                        .map(|span| with_prefix(&prefix, Line::from(span))),
                ),
            }
        }
        i = (end + 1).min(lines.len());
    }
    out
}

fn with_prefix(prefix: &[Span<'static>], line: Line<'static>) -> Line<'static> {
    if prefix.is_empty() {
        return line;
    }
    let mut spans = prefix.to_vec();
    spans.extend(line.spans);
    Line::from(spans)
}

fn ascii_markdown_glyphs(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '│' => '|',
            '─' | '━' => '-',
            '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' => '+',
            '…' => '.',
            other => other,
        })
        .collect()
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

    fn styles(lines: &[Line]) -> Vec<Style> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.style))
            .collect()
    }

    #[test]
    fn a_fenced_block_with_a_known_language_gets_token_colours() {
        let theme = test_theme();
        let lines = render("```rust\nfn main() { /* hi */ }\n```", &theme);
        assert_eq!(plain(&lines), "fn main() { /* hi */ }");
        let used = styles(&lines);
        assert!(used.contains(&theme.ember()), "no keyword colour: {used:?}");
        assert!(used.contains(&theme.info()), "no function colour: {used:?}");
        assert!(
            used.contains(&theme.disabled()),
            "no comment colour: {used:?}"
        );
    }

    #[test]
    fn a_fenced_block_with_an_unknown_language_is_left_alone() {
        let theme = test_theme();
        let lines = render("```brainfuck\n+[-->-[>>+>-----<<]<--<---]\n```", &theme);
        assert_eq!(plain(&lines), "+[-->-[>>+>-----<<]<--<---]");
        assert!(styles(&lines).iter().all(|s| *s == Style::default()));
        // ...and so is a fence with no info string at all.
        let lines = render("```\nplain text\n```", &theme);
        assert_eq!(plain(&lines), "plain text");
        assert!(styles(&lines).iter().all(|s| *s == Style::default()));
    }

    #[test]
    fn the_fence_sentinel_never_reaches_the_transcript() {
        // It exists only to delimit blocks in the rendered lines; if one leaks,
        // the user sees a control character.
        let cases = [
            "```rust\nfn a() {}\n```",
            "```\nno language\n```",
            "```rust\nunclosed(",
            "text\n\n```py\nx = 1\n```\n\nmore",
            "- item\n\n  ```py\n  x = 1\n  y = 2\n  ```\n",
            "> quoted\n> ```rust\n> fn a() {}\n> ```\n",
            "    indented code\n    second line\n",
            "```rust\n```\n",
        ];
        for md in cases {
            for theme in [Theme::ansi(), Theme::ansi().ascii_glyphs()] {
                let text = plain(&render(md, &theme));
                assert!(
                    !text.contains('\u{1}'),
                    "sentinel leaked from {md:?}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn a_block_nested_in_a_list_keeps_one_line_per_source_line() {
        // tui-markdown emits those lines as several spans on one line; the
        // highlight pass reconstructs them, so code in a list is still code.
        let lines = render(
            "- item\n\n  ```py\n  x = 1\n  y = 2\n  ```\n",
            &test_theme(),
        );
        let text = plain(&lines);
        assert!(text.contains("x = 1\ny = 2"), "{text:?}");
    }

    #[test]
    fn a_block_inside_a_blockquote_keeps_its_quote_prefix() {
        let theme = test_theme();
        let lines = render("> ```rust\n> let x = 1;\n> ```\n", &theme);
        let text = plain(&lines);
        assert!(text.contains("> let x = 1;"), "{text:?}");
        assert!(styles(&lines).contains(&theme.ember()), "{text:?}");
    }

    #[test]
    fn a_multiline_string_stays_coloured_on_its_second_line() {
        let theme = test_theme();
        let lines = render("```python\ns = \"\"\"one\ntwo\"\"\"\n```", &theme);
        assert_eq!(plain(&lines), "s = \"\"\"one\ntwo\"\"\"");
        let second = &lines[1];
        assert!(
            second.spans.iter().all(|s| s.style == theme.success()),
            "{second:?}"
        );
    }

    #[test]
    fn highlighting_adds_no_non_ascii_under_an_ascii_theme() {
        let theme = Theme::ansi().ascii_glyphs();
        let md = "```rust\nfn main() { let s = \"hi\"; }\n```";
        for line in render(md, &theme) {
            for span in line.spans {
                assert!(span.content.is_ascii(), "{:?}", span.content);
            }
        }
    }
}
