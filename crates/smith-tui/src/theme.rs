//! Ember design tokens — the single source of truth for every color in the
//! TUI. See `docs/design-system.md` for the rationale: layered surfaces
//! (base/raised/overlay/hover), three text levels, and semantic role colors.
//! No `Color::` literal may appear outside this module.

use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;

/// Spinner frames for a terminal that can render braille.
pub const SPINNER_UNICODE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// ASCII fallback wheel. Four phases instead of ten: the point of a throbber
/// is that it is visibly moving, and a missing glyph renders as a *blank
/// cell*, so a spinner that flickers to nothing reads as a hang.
pub const SPINNER_ASCII: &[&str] = &["-", "\\", "|", "/"];

/// `PartialEq` is load-bearing: the transcript memo keys its cached rows on
/// the theme, since every span style comes from here — and, since `unicode`
/// lives here too, switching glyph sets invalidates rendered rows for free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Whether the terminal can be trusted with non-ASCII glyphs. Kept on the
    /// `Theme` rather than beside it precisely so the render memo picks it up:
    /// glyphs are design tokens on the other capability axis from colour.
    pub unicode: bool,
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
        let base = if truecolor {
            Self::truecolor()
        } else {
            Self::ansi()
        };
        Self {
            unicode: unicode_capable(),
            ..base
        }
    }

    /// The same palette with every glyph forced to ASCII — acceptance
    /// criterion #7's terminal.
    pub fn ascii_glyphs(mut self) -> Self {
        self.unicode = false;
        self
    }

    pub fn truecolor() -> Self {
        Self {
            unicode: true,
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
            unicode: true,
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

    // --- Glyph tokens (see `docs/design-system.md` §1.6) -------------------
    //
    // The colour rule's sibling: no new glyph literal outside this module.
    // A glyph the terminal's font lacks does not degrade to a wrong character,
    // it degrades to an empty cell — so the fallback is picked per *set*.

    /// Throbber frames. Indexed with `% len()` by the caller, because the two
    /// sets deliberately have different lengths.
    pub fn spinner_frames(&self) -> &'static [&'static str] {
        if self.unicode {
            SPINNER_UNICODE
        } else {
            SPINNER_ASCII
        }
    }

    /// Tool card icon for a call that succeeded.
    pub fn icon_ok(&self) -> &'static str {
        if self.unicode {
            "✓"
        } else {
            "+"
        }
    }

    /// Tool card icon for a call that failed.
    pub fn icon_error(&self) -> &'static str {
        if self.unicode {
            "✗"
        } else {
            "x"
        }
    }

    /// Cursor in front of a selected row (tool card, suggestion, option).
    pub fn marker_selected(&self) -> &'static str {
        if self.unicode {
            "›"
        } else {
            ">"
        }
    }

    /// Separator between items on one row (`a · b · c`).
    pub fn separator(&self) -> &'static str {
        if self.unicode {
            " · "
        } else {
            " | "
        }
    }

    pub fn ellipsis(&self) -> &'static str {
        if self.unicode {
            "…"
        } else {
            "..."
        }
    }

    pub fn assistant_gutter(&self) -> (&'static str, &'static str) {
        if self.unicode {
            ("▌ ", "▏ ")
        } else {
            ("| ", "| ")
        }
    }

    pub fn block_border_set(&self) -> symbols::border::Set<'static> {
        if self.unicode {
            symbols::border::PLAIN
        } else {
            symbols::border::Set {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                vertical_left: "|",
                vertical_right: "|",
                horizontal_top: "-",
                horizontal_bottom: "-",
            }
        }
    }

    pub fn rounded_corners(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        if self.unicode {
            ("╭", "╮", "╰", "╯")
        } else {
            ("+", "+", "+", "+")
        }
    }

    pub fn border_horizontal(&self) -> &'static str {
        if self.unicode {
            "─"
        } else {
            "-"
        }
    }

    pub fn border_vertical(&self) -> &'static str {
        if self.unicode {
            "│"
        } else {
            "|"
        }
    }

    /// Filled / unfilled cells of a `LineGauge`.
    pub fn gauge_symbols(&self) -> (&'static str, &'static str) {
        if self.unicode {
            ("━", "─")
        } else {
            ("#", "-")
        }
    }
}

/// Whether the terminal's locale claims UTF-8. `SMITH_ASCII=1` forces the
/// fallback — the escape hatch for a terminal that advertises UTF-8 and then
/// renders braille as blanks anyway, which is most of the ones that fail
/// acceptance criterion #7.
///
/// Unset locale variables are treated as UTF-8: that is the modern default,
/// and the cost of being wrong is a few blank cells, while the cost of the
/// opposite default is an ASCII UI for everybody on a bare `env`.
fn unicode_capable() -> bool {
    if std::env::var("SMITH_ASCII").is_ok_and(|v| v != "0" && !v.is_empty()) {
        return false;
    }
    for key in ["LC_ALL", "LC_CTYPE", "LANG"] {
        match std::env::var(key) {
            Ok(value) if !value.is_empty() => {
                let value = value.to_ascii_lowercase();
                return value.contains("utf-8") || value.contains("utf8");
            }
            _ => continue,
        }
    }
    true
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
    fn every_spinner_frame_is_exactly_one_visible_cell() {
        // Two of the ten braille frames used to be the empty string, so the
        // throbber blinked out twice per cycle and read as a stall.
        for set in [SPINNER_UNICODE, SPINNER_ASCII] {
            for (i, frame) in set.iter().enumerate() {
                assert_eq!(
                    unicode_width::UnicodeWidthStr::width(*frame),
                    1,
                    "frame {i} ({frame:?}) is not one cell wide"
                );
            }
        }
    }

    #[test]
    fn the_ascii_theme_answers_only_in_ascii() {
        let theme = Theme::truecolor().ascii_glyphs();
        let (filled, unfilled) = theme.gauge_symbols();
        let mut glyphs = vec![
            theme.icon_ok(),
            theme.icon_error(),
            theme.marker_selected(),
            theme.ellipsis(),
            theme.assistant_gutter().0,
            theme.assistant_gutter().1,
            theme.border_horizontal(),
            theme.border_vertical(),
            filled,
            unfilled,
        ];
        let border = theme.block_border_set();
        glyphs.extend([
            border.top_left,
            border.top_right,
            border.bottom_left,
            border.bottom_right,
            border.vertical_left,
            border.vertical_right,
            border.horizontal_top,
            border.horizontal_bottom,
        ]);
        let (tl, tr, bl, br) = theme.rounded_corners();
        glyphs.extend([tl, tr, bl, br]);
        glyphs.extend(theme.spinner_frames());
        for glyph in glyphs {
            assert!(glyph.is_ascii(), "{glyph:?} is not ASCII");
        }
    }

    #[test]
    fn the_glyph_set_is_part_of_the_memo_key() {
        // Otherwise a terminal switching capability would keep serving rows
        // drawn with the glyphs it can't show.
        assert_ne!(Theme::ansi(), Theme::ansi().ascii_glyphs());
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
