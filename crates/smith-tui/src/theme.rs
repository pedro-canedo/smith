//! Ember design tokens — the single source of truth for every color in the
//! TUI. See `docs/design-system.md` for the rationale: layered surfaces
//! (base/raised/overlay/hover), three text levels, and semantic role colors.
//! No `Color::` literal may appear outside this module.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone)]
pub struct Theme {
    pub raised: Color,
    pub overlay: Color,
    pub hover: Color,
    pub primary: Color,
    pub secondary: Color,
    pub disabled: Color,
    pub ember: Color,
    pub amber: Color,
    pub success: Color,
    pub danger: Color,
    pub warning: Color,
    pub info: Color,
    pub plan: Color,
    pub diff_add_bg: Color,
    pub diff_del_bg: Color,
}

impl Theme {
    /// Truecolor palette when the terminal advertises it via `COLORTERM`,
    /// otherwise a 16-color fallback that keeps the same role semantics.
    pub fn detect() -> Self {
        let truecolor = std::env::var("COLORTERM")
            .map(|v| v == "truecolor" || v == "24bit")
            .unwrap_or(false);
        if truecolor {
            Self::truecolor()
        } else {
            Self::ansi()
        }
    }

    pub fn truecolor() -> Self {
        Self {
            raised: Color::Rgb(22, 24, 28),
            overlay: Color::Rgb(30, 33, 38),
            hover: Color::Rgb(38, 42, 48),
            primary: Color::Rgb(226, 229, 233),
            secondary: Color::Rgb(148, 154, 163),
            disabled: Color::Rgb(94, 100, 110),
            ember: Color::Rgb(255, 140, 60),
            amber: Color::Rgb(255, 190, 90),
            success: Color::Rgb(88, 206, 128),
            danger: Color::Rgb(240, 90, 90),
            warning: Color::Rgb(250, 204, 21),
            info: Color::Rgb(86, 182, 255),
            plan: Color::Rgb(198, 132, 255),
            diff_add_bg: Color::Rgb(24, 42, 30),
            diff_del_bg: Color::Rgb(46, 26, 28),
        }
    }

    /// Fallback for terminals that don't advertise truecolor. Uses the
    /// 256-color cube rather than the 16 ANSI names: with plain ANSI the three
    /// surfaces all collapse to `Black` (no elevation at all), and the
    /// `DarkGray`/`Gray` mapping the spec originally called for collides with
    /// the text tokens — `disabled` text is itself `DarkGray`, so it would be
    /// invisible on a `DarkGray` inset. 256-color support is far more
    /// universal than truecolor, so this costs nothing in practice.
    pub fn ansi() -> Self {
        Self {
            raised: Color::Indexed(234),
            overlay: Color::Indexed(236),
            hover: Color::Indexed(238),
            primary: Color::Indexed(253),
            secondary: Color::Indexed(246),
            disabled: Color::Indexed(242),
            ember: Color::Indexed(208),
            amber: Color::Indexed(215),
            success: Color::Indexed(78),
            danger: Color::Indexed(203),
            warning: Color::Indexed(220),
            info: Color::Indexed(75),
            plan: Color::Indexed(141),
            diff_add_bg: Color::Indexed(22),
            diff_del_bg: Color::Indexed(52),
        }
    }

    pub fn text(&self) -> Style {
        Style::default().fg(self.primary)
    }

    pub fn secondary(&self) -> Style {
        Style::default().fg(self.secondary)
    }

    pub fn disabled(&self) -> Style {
        Style::default().fg(self.disabled)
    }

    pub fn bold(&self) -> Style {
        Style::default()
            .fg(self.primary)
            .add_modifier(Modifier::BOLD)
    }

    pub fn ember(&self) -> Style {
        Style::default().fg(self.ember)
    }

    pub fn ember_bold(&self) -> Style {
        Style::default().fg(self.ember).add_modifier(Modifier::BOLD)
    }

    pub fn amber(&self) -> Style {
        Style::default().fg(self.amber)
    }

    pub fn success(&self) -> Style {
        Style::default().fg(self.success)
    }

    pub fn danger(&self) -> Style {
        Style::default().fg(self.danger)
    }

    pub fn warning(&self) -> Style {
        Style::default().fg(self.warning)
    }

    pub fn info(&self) -> Style {
        Style::default().fg(self.info)
    }

    pub fn info_bold(&self) -> Style {
        Style::default().fg(self.info).add_modifier(Modifier::BOLD)
    }

    pub fn plan(&self) -> Style {
        Style::default().fg(self.plan)
    }

    pub fn plan_bold(&self) -> Style {
        Style::default().fg(self.plan).add_modifier(Modifier::BOLD)
    }

    /// Background fill for raised panels (tool cards, user bubble, code).
    pub fn raised_bg(&self) -> Style {
        Style::default().bg(self.raised)
    }

    /// Background fill for insets inside raised panels (commands, output,
    /// diff gutters, key chips).
    pub fn overlay_bg(&self) -> Style {
        Style::default().bg(self.overlay)
    }

    pub fn hover_bg(&self) -> Style {
        Style::default().bg(self.hover)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_and_ansi_palettes_keep_role_semantics_but_differ_surfaces() {
        let tc = Theme::truecolor();
        let ansi = Theme::ansi();
        assert!(matches!(tc.raised, Color::Rgb(22, 24, 28)));
        assert!(matches!(ansi.raised, Color::Indexed(_)));
        assert!(matches!(tc.ember, Color::Rgb(255, 140, 60)));
        assert_eq!(ansi.ember, Color::Indexed(208));
    }

    #[test]
    fn ansi_surfaces_are_three_distinct_elevation_levels() {
        // They used to be `Black`, `Black`, `Black` — tool cards, bubbles and
        // insets were indistinguishable outside a truecolor terminal.
        let ansi = Theme::ansi();
        assert_ne!(ansi.raised, ansi.overlay);
        assert_ne!(ansi.overlay, ansi.hover);
        assert_ne!(ansi.raised, ansi.hover);
    }

    #[test]
    fn ansi_text_levels_stay_readable_against_the_surfaces() {
        let ansi = Theme::ansi();
        for text in [ansi.primary, ansi.secondary, ansi.disabled] {
            for surface in [ansi.raised, ansi.overlay, ansi.hover] {
                assert_ne!(text, surface, "text would be invisible on this surface");
            }
        }
    }
}
