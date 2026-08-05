//! Key-hint chips: `[y] allow once`, `[Esc] cancel`, etc.
//!
//! A chip is two spans — the key in semantic color on an overlay bg, then a
//! plain label — so the keyboard shortcuts pop visually against any panel.

use ratatui::style::Color;
use ratatui::text::Span;

use crate::theme::Theme;

/// Build a `[key] label` chip. `key_color` is the semantic role (success /
/// danger / info / …) used for the key text.
pub fn key_hint<'a>(key: &str, label: &str, key_color: Color, theme: &Theme) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            format!("[{key}]"),
            Style::default()
                .fg(key_color)
                .bg(theme.overlay)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label} "), theme.secondary()),
    ]
}

/// Convenience: a chip for a positive action (success color).
pub fn confirm_hint<'a>(key: &str, label: &str, theme: &Theme) -> Vec<Span<'a>> {
    key_hint(key, label, theme.success, theme)
}

/// Convenience: a chip for a negative action (danger color).
pub fn cancel_hint<'a>(key: &str, label: &str, theme: &Theme) -> Vec<Span<'a>> {
    key_hint(key, label, theme.danger, theme)
}

/// Convenience: a chip for an info/neutral action (info color).
pub fn info_hint<'a>(key: &str, label: &str, theme: &Theme) -> Vec<Span<'a>> {
    key_hint(key, label, theme.info, theme)
}

use ratatui::style::{Modifier, Style};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hint_renders_key_and_label() {
        let theme = Theme::ansi();
        let spans = key_hint("y", "allow once", theme.success, &theme);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "[y] allow once ");
    }
}
