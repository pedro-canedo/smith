//! Ember design tokens — the single source of truth for every color in the
//! TUI. See `docs/design-system.md` for the rationale: layered surfaces
//! (base/raised/overlay/hover), three text levels, and semantic role colors.
//! No `Color::` literal may appear outside this module.
//!
//! Three presets live here — `dark` (Ember), `light` and `high_contrast` —
//! each in a truecolor and a 256/16-colour variant, and every one of them is
//! held to WCAG 2.1 AA by `tests::every_preset_meets_wcag_aa`: 4.5:1 for a
//! token that carries text, 3:1 for `disabled`, which only ever carries
//! de-emphasised chrome (gutters, elapsed times, line numbers) that is never
//! the sole carrier of information. The thresholds are the fixed point of
//! that test; a colour that misses one gets changed, never the threshold.

use std::collections::BTreeMap;

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
    /// The page itself. Everything the UI does not explicitly raise sits on
    /// this, and it is painted (`ui::draw`) rather than inherited from the
    /// terminal — inheriting it is what made a light theme impossible, since
    /// `primary` was picked against a dark surface nobody had declared.
    pub base: Color,
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

/// Which palette to paint in. The capability axis (truecolor vs. 256/16
/// colours) is detected, never named here: a user picks a *look*, and the
/// terminal decides how faithfully it can be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    /// Ember: the forge — dark iron surfaces, ember/amber accents.
    #[default]
    Dark,
    /// Warm paper. Not an inversion of `Dark`: every role colour is picked
    /// again against a light surface, because a hue that reads as "hot" at
    /// `Rgb(255,140,60)` on near-black is unreadable on near-white.
    Light,
    /// Maximum separation for low vision and for 16-colour terminals.
    HighContrast,
}

impl ThemeName {
    pub const ALL: [ThemeName; 3] = [ThemeName::Dark, ThemeName::Light, ThemeName::HighContrast];

    /// `dark` / `light` / `high_contrast`, tolerant of case and of a hyphen
    /// in place of the underscore. Anything else is `None`, which the caller
    /// must turn into an error — a mistyped theme name that silently fell
    /// back to the default would be indistinguishable from the flag not
    /// working.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "dark" => Some(ThemeName::Dark),
            "light" => Some(ThemeName::Light),
            "high_contrast" => Some(ThemeName::HighContrast),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ThemeName::Dark => "dark",
            ThemeName::Light => "light",
            ThemeName::HighContrast => "high_contrast",
        }
    }
}

/// Why a configured theme could not be built. Every variant is a user-visible
/// message rather than a fallback: a palette that quietly ignores half of what
/// the config asked for is worse than one that refuses to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeError {
    UnknownName(String),
    UnknownToken(String),
    BadHex { token: String, value: String },
}

impl std::fmt::Display for ThemeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThemeError::UnknownName(name) => write!(
                f,
                "unknown theme {name:?} — expected one of: {}",
                ThemeName::ALL
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ThemeError::UnknownToken(token) => write!(
                f,
                "unknown theme colour {token:?} — expected one of: {}",
                TOKEN_NAMES.join(", ")
            ),
            ThemeError::BadHex { token, value } => write!(
                f,
                "theme colour {token} = {value:?} is not a hex colour (expected #rgb or #rrggbb)"
            ),
        }
    }
}

impl std::error::Error for ThemeError {}

/// Every overridable colour token, in declaration order. The one list the
/// config's per-token overrides, the error message and the WCAG sweep all
/// read from, so a token added to `Theme` and forgotten here is caught by
/// `tests::every_token_is_reachable_by_name`.
pub const TOKEN_NAMES: &[&str] = &[
    "base",
    "raised",
    "overlay",
    "hover",
    "primary",
    "secondary",
    "disabled",
    "ember",
    "amber",
    "success",
    "danger",
    "warning",
    "info",
    "plan",
    "diff_add_bg",
    "diff_del_bg",
];

impl Theme {
    /// The dark palette, capability-detected. Kept as the no-argument entry
    /// point because it is what every render test and the fallback path use.
    pub fn detect() -> Self {
        Self::named(ThemeName::Dark)
    }

    /// A named preset, with the terminal's colour depth and glyph capability
    /// filled in.
    pub fn named(name: ThemeName) -> Self {
        Self {
            unicode: unicode_capable(),
            ..Self::preset(name, truecolor_capable())
        }
    }

    /// The preset with no capability detection at all — the pure function the
    /// contrast tests sweep.
    pub fn preset(name: ThemeName, truecolor: bool) -> Self {
        match (name, truecolor) {
            (ThemeName::Dark, true) => Self::truecolor(),
            (ThemeName::Dark, false) => Self::ansi(),
            (ThemeName::Light, true) => Self::light(),
            (ThemeName::Light, false) => Self::light_ansi(),
            (ThemeName::HighContrast, true) => Self::high_contrast(),
            (ThemeName::HighContrast, false) => Self::high_contrast_ansi(),
        }
    }

    /// The whole `[theme]` config section, resolved: a name (default `dark`)
    /// plus per-token hex overrides. Both halves fail loudly — see
    /// `ThemeError`.
    pub fn resolve(
        name: Option<&str>,
        overrides: &BTreeMap<String, String>,
    ) -> Result<Self, ThemeError> {
        let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(raw) => {
                ThemeName::parse(raw).ok_or_else(|| ThemeError::UnknownName(raw.to_string()))?
            }
            None => ThemeName::default(),
        };
        let mut theme = Self::named(name);
        for (token, value) in overrides {
            theme.set_token(
                token,
                parse_hex(value).ok_or_else(|| ThemeError::BadHex {
                    token: token.clone(),
                    value: value.clone(),
                })?,
            )?;
        }
        Ok(theme)
    }

    /// Points one token at a colour. `Err` for a name that is not a token —
    /// a typo'd key in `[theme.colors]` is a colour the user believes they
    /// changed.
    pub fn set_token(&mut self, token: &str, color: Color) -> Result<(), ThemeError> {
        let slot = match token {
            "base" => &mut self.base,
            "raised" => &mut self.raised,
            "overlay" => &mut self.overlay,
            "hover" => &mut self.hover,
            "primary" => &mut self.primary,
            "secondary" => &mut self.secondary,
            "disabled" => &mut self.disabled,
            "ember" => &mut self.ember,
            "amber" => &mut self.amber,
            "success" => &mut self.success,
            "danger" => &mut self.danger,
            "warning" => &mut self.warning,
            "info" => &mut self.info,
            "plan" => &mut self.plan,
            "diff_add_bg" => &mut self.diff_add_bg,
            "diff_del_bg" => &mut self.diff_del_bg,
            other => return Err(ThemeError::UnknownToken(other.to_string())),
        };
        *slot = color;
        Ok(())
    }

    /// One token by name, for the contrast sweep and for `/theme` style
    /// introspection. `None` for a name that is not a token.
    pub fn token(&self, name: &str) -> Option<Color> {
        Some(match name {
            "base" => self.base,
            "raised" => self.raised,
            "overlay" => self.overlay,
            "hover" => self.hover,
            "primary" => self.primary,
            "secondary" => self.secondary,
            "disabled" => self.disabled,
            "ember" => self.ember,
            "amber" => self.amber,
            "success" => self.success,
            "danger" => self.danger,
            "warning" => self.warning,
            "info" => self.info,
            "plan" => self.plan,
            "diff_add_bg" => self.diff_add_bg,
            "diff_del_bg" => self.diff_del_bg,
            _ => return None,
        })
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
            // The value the terminal's own background used to have to supply.
            // Below `raised`, so the elevation ladder starts where the design
            // system says it does instead of wherever the user's profile left
            // it.
            base: Color::Rgb(16, 18, 21),
            raised: Color::Rgb(22, 24, 28),
            overlay: Color::Rgb(30, 33, 38),
            hover: Color::Rgb(38, 42, 48),
            primary: Color::Rgb(226, 229, 233),
            secondary: Color::Rgb(148, 154, 163),
            // Was Rgb(94,100,110): 2.42:1 on `hover`, under even the 3:1 that
            // de-emphasised chrome gets.
            disabled: Color::Rgb(114, 120, 130),
            ember: Color::Rgb(255, 140, 60),
            amber: Color::Rgb(255, 190, 90),
            success: Color::Rgb(88, 206, 128),
            // Was Rgb(240,90,90): 4.34:1 on `hover`, just under AA.
            danger: Color::Rgb(243, 102, 102),
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
    ///
    /// The elevation ladder is tighter than the truecolor one (8/28/38/48
    /// rather than 8/24/33/42-ish) for a measured reason: `hover` used to be
    /// `Indexed(238)` = grey 68, and six of the ten foreground tokens fell
    /// under AA against it. Lowering the top of the ladder by two steps costs
    /// a little separation between surfaces and buys back the entire palette.
    pub fn ansi() -> Self {
        Self {
            unicode: true,
            base: Color::Indexed(232),
            raised: Color::Indexed(234),
            overlay: Color::Indexed(235),
            hover: Color::Indexed(236),
            primary: Color::Indexed(253),
            secondary: Color::Indexed(247),
            disabled: Color::Indexed(244),
            ember: Color::Indexed(208),
            amber: Color::Indexed(215),
            // 84 rather than 78: the only green background the 6×6×6 cube
            // offers for `diff_add_bg` is `(0,95,0)`, and the dimmer green
            // reached only 4.38:1 on it.
            success: Color::Indexed(84),
            danger: Color::Indexed(210),
            warning: Color::Indexed(220),
            info: Color::Indexed(75),
            plan: Color::Indexed(177),
            diff_add_bg: Color::Indexed(22),
            diff_del_bg: Color::Indexed(52),
        }
    }

    /// Warm paper. Surfaces get *darker* as they rise, which is how elevation
    /// reads on a light ground, and every role colour is a fresh pick: the
    /// dark theme's accents are all far too bright to carry text here.
    pub fn light() -> Self {
        Self {
            unicode: true,
            base: Color::Rgb(250, 249, 247),
            raised: Color::Rgb(242, 240, 236),
            overlay: Color::Rgb(232, 229, 224),
            hover: Color::Rgb(220, 216, 209),
            primary: Color::Rgb(28, 27, 25),
            secondary: Color::Rgb(85, 82, 78),
            disabled: Color::Rgb(118, 114, 108),
            ember: Color::Rgb(160, 54, 0),
            amber: Color::Rgb(124, 78, 0),
            success: Color::Rgb(19, 102, 56),
            danger: Color::Rgb(174, 28, 28),
            warning: Color::Rgb(124, 82, 0),
            info: Color::Rgb(20, 84, 172),
            plan: Color::Rgb(108, 48, 162),
            diff_add_bg: Color::Rgb(214, 238, 219),
            diff_del_bg: Color::Rgb(250, 219, 219),
        }
    }

    /// The light palette on the 256-colour cube.
    ///
    /// The cube has no dark orange — its channel levels are 0/95/135/175/215/
    /// 255, and the darkest orange it can express, `(135,95,0)`, still misses
    /// AA against the `hover` surface. So `ember` becomes a deep brick and
    /// `amber`/`warning` share the one dark gold the cube does have. That
    /// collapse is the honest answer for 256 colours; the truecolor palette
    /// above keeps all three distinct.
    pub fn light_ansi() -> Self {
        Self {
            unicode: true,
            base: Color::Indexed(231),
            raised: Color::Indexed(255),
            overlay: Color::Indexed(254),
            hover: Color::Indexed(253),
            primary: Color::Indexed(16),
            secondary: Color::Indexed(239),
            disabled: Color::Indexed(243),
            ember: Color::Indexed(88),
            amber: Color::Indexed(58),
            success: Color::Indexed(22),
            danger: Color::Indexed(124),
            warning: Color::Indexed(58),
            info: Color::Indexed(25),
            plan: Color::Indexed(90),
            diff_add_bg: Color::Indexed(194),
            diff_del_bg: Color::Indexed(224),
        }
    }

    /// Maximum separation: pure black ground, foregrounds at full brightness,
    /// and no mid-greys anywhere. Every body pair clears AA by a wide margin
    /// and `disabled` clears 4.5:1 too — in a palette for low vision, the
    /// 3:1 allowance for de-emphasised chrome is not worth taking.
    pub fn high_contrast() -> Self {
        Self {
            unicode: true,
            base: Color::Rgb(0, 0, 0),
            raised: Color::Rgb(18, 18, 18),
            overlay: Color::Rgb(38, 38, 38),
            hover: Color::Rgb(58, 58, 58),
            primary: Color::Rgb(255, 255, 255),
            secondary: Color::Rgb(224, 224, 224),
            disabled: Color::Rgb(176, 176, 176),
            ember: Color::Rgb(255, 160, 60),
            amber: Color::Rgb(255, 214, 102),
            success: Color::Rgb(0, 255, 128),
            danger: Color::Rgb(255, 130, 130),
            warning: Color::Rgb(255, 255, 0),
            info: Color::Rgb(102, 204, 255),
            plan: Color::Rgb(220, 150, 255),
            diff_add_bg: Color::Rgb(0, 48, 0),
            diff_del_bg: Color::Rgb(64, 0, 0),
        }
    }

    /// High contrast on a real 16-colour terminal — the only preset that
    /// stays inside `Indexed(0..=15)`, so it survives where the 256-colour
    /// cube does not.
    ///
    /// Every surface is black, deliberately. The two backgrounds the 16-colour
    /// set could otherwise offer are grey 8 and blue 4, and each of them drops
    /// at least one role colour below 4.5:1 (bright red on blue measures
    /// 4.00:1). Elevation is the thing worth spending here, so structure falls
    /// to the `›` marker and the border glyphs, and the whole colour budget
    /// goes to foreground legibility. For the same reason `ember`, `amber` and
    /// `warning` share bright yellow: seven role hues do not fit in the five
    /// bright colours that clear AA on black (blue is not one of them).
    pub fn high_contrast_ansi() -> Self {
        Self {
            unicode: true,
            base: Color::Indexed(0),
            raised: Color::Indexed(0),
            overlay: Color::Indexed(0),
            hover: Color::Indexed(0),
            primary: Color::Indexed(15),
            secondary: Color::Indexed(7),
            disabled: Color::Indexed(8),
            ember: Color::Indexed(11),
            amber: Color::Indexed(11),
            success: Color::Indexed(10),
            danger: Color::Indexed(9),
            warning: Color::Indexed(11),
            info: Color::Indexed(14),
            plan: Color::Indexed(13),
            diff_add_bg: Color::Indexed(0),
            diff_del_bg: Color::Indexed(0),
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

    /// The page. Painted once over the whole frame before anything else, so
    /// no region falls through to the terminal's own background.
    pub fn base_bg(&self) -> Style {
        Style::default().bg(self.base)
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

// --- colour maths (WCAG 2.1) ----------------------------------------------

/// A `#rgb` / `#rrggbb` string (the `#` optional) as a truecolor value.
///
/// Only these two forms: `rgb()`, colour names and 8-digit alpha hex would
/// each need their own error message for the ways they can be wrong, and a
/// config file wants one obvious spelling.
pub fn parse_hex(text: &str) -> Option<Color> {
    let hex = text.trim().strip_prefix('#').unwrap_or(text.trim());
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |s: &str| u8::from_str_radix(s, 16).ok();
    match hex.len() {
        // #abc is #aabbcc — the CSS shorthand, because that is what anyone
        // copying a colour from anywhere else will paste.
        3 => {
            let d: Vec<u8> = hex
                .chars()
                .map(|c| c.to_digit(16).unwrap_or(0) as u8)
                .collect();
            Some(Color::Rgb(d[0] * 17, d[1] * 17, d[2] * 17))
        }
        6 => Some(Color::Rgb(
            byte(&hex[0..2])?,
            byte(&hex[2..4])?,
            byte(&hex[4..6])?,
        )),
        _ => None,
    }
}

/// The sRGB triple behind a colour, or `None` when there is nothing to
/// measure.
///
/// `Color::Reset` and the sixteen *named* ANSI variants return `None` on
/// purpose: their actual values belong to the user's terminal profile, so any
/// number computed from them would be a guess presented as a measurement. No
/// preset uses them — `tests::every_preset_token_is_measurable` is what keeps
/// that true, rather than the contrast sweep quietly skipping a pair.
pub fn rgb_of(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(n) => Some(indexed_rgb(n)),
        _ => None,
    }
}

/// The xterm 256-colour palette: 0–15 the ANSI names, 16–231 a 6×6×6 cube,
/// 232–255 a 24-step greyscale ramp.
///
/// The first sixteen are nominal — a terminal is free to remap them, and most
/// themes do. They are included anyway because `high_contrast_ansi` is built
/// from them and an untestable preset is an untested one; the numbers are
/// xterm's defaults, which is what a terminal that has not been retuned
/// shows.
pub fn indexed_rgb(index: u8) -> (u8, u8, u8) {
    const ANSI_16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match index {
        0..=15 => ANSI_16[index as usize],
        16..=231 => {
            let i = index as u16 - 16;
            let level = |v: u16| -> u8 {
                if v == 0 {
                    0
                } else {
                    (55 + 40 * v) as u8
                }
            };
            (level(i / 36), level((i % 36) / 6), level(i % 6))
        }
        _ => {
            let v = 8 + 10 * (index - 232);
            (v, v, v)
        }
    }
}

/// Relative luminance per WCAG 2.1: linearise each sRGB channel, then weight
/// them 0.2126 / 0.7152 / 0.0722.
pub fn relative_luminance((r, g, b): (u8, u8, u8)) -> f64 {
    fn linearise(channel: u8) -> f64 {
        let c = channel as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linearise(r) + 0.7152 * linearise(g) + 0.0722 * linearise(b)
}

/// WCAG 2.1 contrast ratio, `(L1 + 0.05) / (L2 + 0.05)` with `L1` the lighter
/// of the two. 1.0 for identical colours, 21.0 for black on white. `None`
/// when either colour has no measurable value — see `rgb_of`.
pub fn contrast_ratio(a: Color, b: Color) -> Option<f64> {
    let (a, b) = (
        relative_luminance(rgb_of(a)?),
        relative_luminance(rgb_of(b)?),
    );
    let (lighter, darker) = if a >= b { (a, b) } else { (b, a) };
    Some((lighter + 0.05) / (darker + 0.05))
}

/// Whether the terminal advertises 24-bit colour through `COLORTERM`.
fn truecolor_capable() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v == "truecolor" || v == "24bit")
        .unwrap_or(false)
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

    // --- WCAG ------------------------------------------------------------

    /// Tokens that carry running text, and so owe the full AA ratio.
    const BODY_TOKENS: &[&str] = &[
        "primary",
        "secondary",
        "ember",
        "amber",
        "success",
        "danger",
        "warning",
        "info",
        "plan",
    ];
    /// Tokens that only ever carry de-emphasised chrome — gutters, borders,
    /// gauge tracks, elapsed times, diff line numbers — none of which is the
    /// sole carrier of any information. AA's non-text / large-text ratio.
    const CHROME_TOKENS: &[&str] = &["disabled"];
    const SURFACE_TOKENS: &[&str] = &["base", "raised", "overlay", "hover"];
    const AA_BODY: f64 = 4.5;
    const AA_LARGE: f64 = 3.0;

    fn all_presets() -> Vec<(String, Theme)> {
        let mut out = Vec::new();
        for name in ThemeName::ALL {
            for truecolor in [true, false] {
                let depth = if truecolor { "truecolor" } else { "256/16" };
                out.push((
                    format!("{} ({depth})", name.as_str()),
                    Theme::preset(name, truecolor),
                ));
            }
        }
        out
    }

    fn ratio(theme: &Theme, fg: &str, bg: &str) -> f64 {
        contrast_ratio(theme.token(fg).unwrap(), theme.token(bg).unwrap())
            .unwrap_or_else(|| panic!("{fg}/{bg} is not measurable"))
    }

    /// The gate the whole module exists to satisfy. Every foreground token,
    /// on every surface it can land on, in every preset.
    #[test]
    fn every_preset_meets_wcag_aa() {
        // Foreground/background pairs the renderer actually produces beyond
        // the surface sweep: `components::diff` paints `+` and `-` rows with
        // these exact combinations.
        let diff_pairs = [("success", "diff_add_bg"), ("danger", "diff_del_bg")];
        let mut failures = Vec::new();
        for (label, theme) in all_presets() {
            for (tokens, floor) in [(BODY_TOKENS, AA_BODY), (CHROME_TOKENS, AA_LARGE)] {
                for fg in tokens {
                    for bg in SURFACE_TOKENS {
                        let r = ratio(&theme, fg, bg);
                        if r < floor {
                            failures.push(format!("{label}: {fg} on {bg} is {r:.2}:1 < {floor}"));
                        }
                    }
                }
            }
            for (fg, bg) in diff_pairs {
                let r = ratio(&theme, fg, bg);
                if r < AA_BODY {
                    failures.push(format!("{label}: {fg} on {bg} is {r:.2}:1 < {AA_BODY}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "fix the colour, never the threshold:\n{}",
            failures.join("\n")
        );
    }

    /// The sweep above must never be able to pass by *skipping* a pair, which
    /// is what a `Color::Reset` or a named ANSI variant would make it do.
    #[test]
    fn every_preset_token_is_measurable() {
        for (label, theme) in all_presets() {
            for token in TOKEN_NAMES {
                let color = theme.token(token).unwrap();
                assert!(
                    rgb_of(color).is_some(),
                    "{label}: {token} = {color:?} has no measurable value"
                );
            }
        }
    }

    #[test]
    fn every_token_is_reachable_by_name() {
        let mut theme = Theme::truecolor();
        for (i, token) in TOKEN_NAMES.iter().enumerate() {
            let color = Color::Rgb(i as u8, 0, 0);
            theme.set_token(token, color).unwrap();
            assert_eq!(
                theme.token(token),
                Some(color),
                "{token} did not round-trip"
            );
        }
        assert_eq!(
            TOKEN_NAMES.len(),
            16,
            "a token was added to Theme without being added to TOKEN_NAMES"
        );
    }

    #[test]
    fn contrast_ratio_matches_the_wcag_reference_values() {
        let white = Color::Rgb(255, 255, 255);
        let black = Color::Rgb(0, 0, 0);
        assert!((contrast_ratio(black, white).unwrap() - 21.0).abs() < 0.001);
        assert!((contrast_ratio(white, white).unwrap() - 1.0).abs() < 0.001);
        // The ratio is symmetric: the lighter colour is always the numerator.
        assert_eq!(contrast_ratio(black, white), contrast_ratio(white, black));
        // #767676 is the canonical "smallest grey that passes AA on white".
        let grey = parse_hex("#767676").unwrap();
        assert!((contrast_ratio(grey, white).unwrap() - 4.54).abs() < 0.01);
        // A colour the terminal owns cannot be measured.
        assert_eq!(contrast_ratio(Color::Reset, white), None);
    }

    #[test]
    fn the_256_colour_cube_maps_the_way_xterm_does() {
        assert_eq!(indexed_rgb(0), (0, 0, 0));
        assert_eq!(indexed_rgb(15), (255, 255, 255));
        // Cube corners and one interior value.
        assert_eq!(indexed_rgb(16), (0, 0, 0));
        assert_eq!(indexed_rgb(231), (255, 255, 255));
        assert_eq!(indexed_rgb(208), (255, 135, 0));
        assert_eq!(indexed_rgb(75), (95, 175, 255));
        // Greyscale ramp: 8, then every tenth up to 238.
        assert_eq!(indexed_rgb(232), (8, 8, 8));
        assert_eq!(indexed_rgb(255), (238, 238, 238));
    }

    // --- presets ---------------------------------------------------------

    /// `base` has to sit at the bottom of the ladder in a dark theme and at
    /// the top of it in a light one, or "raised" means nothing. Ties are
    /// allowed: `high_contrast_ansi` deliberately has none.
    #[test]
    fn the_elevation_ladder_climbs_away_from_the_page_in_every_preset() {
        for (label, theme) in all_presets() {
            let l = |token: &str| relative_luminance(rgb_of(theme.token(token).unwrap()).unwrap());
            let (base, raised, overlay, hover) = (l("base"), l("raised"), l("overlay"), l("hover"));
            let light = l("primary") < base; // dark text ⇒ light theme
            let climbs = if light {
                base >= raised && raised >= overlay && overlay >= hover
            } else {
                base <= raised && raised <= overlay && overlay <= hover
            };
            assert!(
                climbs,
                "{label}: elevation ladder is out of order \
                 (base {base:.4}, raised {raised:.4}, overlay {overlay:.4}, hover {hover:.4})"
            );
        }
    }

    #[test]
    fn the_light_preset_is_a_repick_and_not_an_inversion() {
        let dark = Theme::truecolor();
        let light = Theme::light();
        let lum = |c: Color| relative_luminance(rgb_of(c).unwrap());
        assert!(lum(light.base) > 0.8, "a light theme needs a light page");
        assert!(lum(dark.base) < 0.05);
        // An inversion would leave the accents at the same hue and lightness
        // complement; a repick lands them somewhere else entirely.
        for token in [
            "ember", "amber", "success", "danger", "warning", "info", "plan",
        ] {
            let (d, l) = (
                lum(dark.token(token).unwrap()),
                lum(light.token(token).unwrap()),
            );
            assert!(
                l < d,
                "{token}: a light theme's accents must be darker than a dark theme's ({l:.3} vs {d:.3})"
            );
        }
    }

    #[test]
    fn the_high_contrast_preset_separates_further_than_the_dark_one() {
        let ember = Theme::truecolor();
        let hc = Theme::high_contrast();
        assert!(
            contrast_ratio(hc.primary, hc.base).unwrap()
                > contrast_ratio(ember.primary, ember.base).unwrap()
        );
        // No mid-greys: every neutral is either near-black or well clear of
        // the middle of the range.
        for token in [
            "base",
            "raised",
            "overlay",
            "hover",
            "disabled",
            "secondary",
            "primary",
        ] {
            let l = relative_luminance(rgb_of(hc.token(token).unwrap()).unwrap());
            assert!(
                !(0.06..0.35).contains(&l),
                "{token} sits in the mid-grey band ({l:.3})"
            );
        }
    }

    #[test]
    fn the_16_colour_high_contrast_preset_stays_inside_the_ansi_range() {
        // The whole point of this variant: it has to render on a terminal
        // that has no 256-colour cube at all.
        let hc = Theme::high_contrast_ansi();
        for token in TOKEN_NAMES {
            match hc.token(token).unwrap() {
                Color::Indexed(n) => assert!(n < 16, "{token} = Indexed({n}) is outside 0..16"),
                other => panic!("{token} = {other:?} is not an ANSI index"),
            }
        }
    }

    // --- config ----------------------------------------------------------

    #[test]
    fn hex_colours_parse_in_both_lengths_and_with_or_without_the_hash() {
        assert_eq!(parse_hex("#ff8c3c"), Some(Color::Rgb(255, 140, 60)));
        assert_eq!(parse_hex("ff8c3c"), Some(Color::Rgb(255, 140, 60)));
        assert_eq!(parse_hex("  #FF8C3C  "), Some(Color::Rgb(255, 140, 60)));
        // The CSS shorthand expands per digit, not by padding with zeroes.
        assert_eq!(parse_hex("#fff"), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(parse_hex("#08f"), Some(Color::Rgb(0, 136, 255)));
        for bad in [
            "",
            "#",
            "#ff",
            "#ff8c3",
            "#ff8c3cff",
            "red",
            "#gg0000",
            "rgb(1,2,3)",
        ] {
            assert_eq!(parse_hex(bad), None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn resolve_defaults_to_dark_and_applies_overrides() {
        let mut overrides = BTreeMap::new();
        overrides.insert("ember".to_string(), "#00ff00".to_string());
        overrides.insert("base".to_string(), "#123456".to_string());
        let theme = Theme::resolve(None, &overrides).unwrap();
        assert_eq!(theme.ember, Color::Rgb(0, 255, 0));
        assert_eq!(theme.base, Color::Rgb(0x12, 0x34, 0x56));
        // Everything unstated keeps the preset's value.
        assert_eq!(theme.plan, Theme::named(ThemeName::Dark).plan);
    }

    #[test]
    fn resolve_accepts_the_three_names_and_refuses_anything_else() {
        for (input, expected) in [
            ("light", ThemeName::Light),
            ("LIGHT", ThemeName::Light),
            ("high-contrast", ThemeName::HighContrast),
            ("high_contrast", ThemeName::HighContrast),
            (" dark ", ThemeName::Dark),
        ] {
            let theme = Theme::resolve(Some(input), &BTreeMap::new()).unwrap();
            assert_eq!(theme.primary, Theme::named(expected).primary, "{input}");
        }
        // An unknown name must not quietly become the default: the user asked
        // for something and would otherwise never learn they did not get it.
        let err = Theme::resolve(Some("solarized"), &BTreeMap::new()).unwrap_err();
        assert_eq!(err, ThemeError::UnknownName("solarized".into()));
        assert!(err.to_string().contains("high_contrast"), "{err}");
    }

    #[test]
    fn a_bad_override_is_an_error_naming_the_token() {
        let mut overrides = BTreeMap::new();
        overrides.insert("ember".to_string(), "orange".to_string());
        let err = Theme::resolve(None, &overrides).unwrap_err();
        assert_eq!(
            err,
            ThemeError::BadHex {
                token: "ember".into(),
                value: "orange".into()
            }
        );
        assert!(err.to_string().contains("ember"), "{err}");

        let mut overrides = BTreeMap::new();
        overrides.insert("embers".to_string(), "#ffffff".to_string());
        let err = Theme::resolve(None, &overrides).unwrap_err();
        assert_eq!(err, ThemeError::UnknownToken("embers".into()));
        assert!(err.to_string().contains("ember,"), "{err}");
    }

    #[test]
    fn the_palette_is_part_of_the_memo_key() {
        // Same reason as the glyph set: `Theme` keys the transcript memo, so
        // switching preset has to invalidate every cached row.
        assert_ne!(Theme::truecolor(), Theme::light());
        assert_ne!(Theme::truecolor(), Theme::high_contrast());
        let mut recoloured = Theme::truecolor();
        recoloured.base = Color::Rgb(1, 2, 3);
        assert_ne!(Theme::truecolor(), recoloured);
    }

    // --- the base surface -------------------------------------------------

    /// The whole point of the `base` token: after a frame is drawn there must
    /// be no cell left showing whatever background the terminal was set to.
    /// Lives here rather than in `ui.rs` because this is the token's test, not
    /// the layout's.
    #[test]
    fn no_cell_falls_through_to_the_terminals_own_background() {
        use ratatui::backend::TestBackend;
        use ratatui::layout::Position;
        use ratatui::Terminal;

        let with_text = || {
            vec![crate::app::ChatLine::new(
                crate::app::ChatRole::User,
                "hello".to_string(),
            )]
        };
        let question = || {
            crate::app::Modal::Question(crate::app::QuestionModal {
                question: smith_core::UserQuestion {
                    id: "q1".into(),
                    prompt: "which one?".into(),
                    options: ["a".into(), "b".into(), "c".into()],
                },
                selected: 0,
                custom: String::new(),
            })
        };
        // A modal is the case that nearly got missed: it `Clear`s the pane
        // behind it, and `Clear` resets cells to the terminal's own colours.
        for (label, lines, modal) in [
            ("idle", Vec::new(), crate::app::Modal::None),
            ("transcript", with_text(), crate::app::Modal::None),
            ("modal", with_text(), question()),
        ] {
            let mut app = crate::app::App::new(crate::app::TuiConfig {
                banner: "smith".into(),
                provider_label: "ollama".into(),
                model_label: "qwen2.5".into(),
                cwd_display: "~/smith".into(),
                git_branch: None,
                idle_hint: crate::app::IdleHint::Tip(String::new()),
                initial_lines: lines,
                permission_policy: smith_core::PermissionPolicy::default(),
                // The light preset makes the failure mode real: on a dark
                // terminal an unpainted cell is a black hole in a white page.
                theme: Theme::light(),
                goal: None,
                tasks: Vec::new(),
                commands: crate::slash::SlashRegistry::builtin(),
            });
            app.modal = modal;
            // Both width tiers: with the sidebar (>= 80) and without it.
            for (width, height) in [(80u16, 24u16), (60, 20)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
                let buffer = terminal.backend().buffer().clone();
                for y in 0..height {
                    for x in 0..width {
                        let cell = buffer.cell(Position::new(x, y)).unwrap();
                        assert_ne!(
                            cell.bg,
                            Color::Reset,
                            "{label} at {width}x{height}: cell ({x},{y}) kept the terminal's background"
                        );
                    }
                }
            }
        }
    }

    // --- the rule ---------------------------------------------------------

    /// "No `Color::` literal outside `theme.rs`" was a convention. This makes
    /// it a build failure. Test modules are exempt: a render assertion needs
    /// to name the colour it expects to find in the buffer.
    #[test]
    fn no_colour_literal_lives_outside_this_module() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        assert!(files.len() > 5, "found no sources to check under {src:?}");

        let mut violations = Vec::new();
        for file in files {
            if file.file_name().is_some_and(|n| n == "theme.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            // Everything from the inline test module down is a test.
            let production = text.split("#[cfg(test)]").next().unwrap_or_default();
            for (i, line) in production.lines().enumerate() {
                if line.contains("Color::") && !line.trim_start().starts_with("//") {
                    violations.push(format!("{}:{}: {}", file.display(), i + 1, line.trim()));
                }
            }
        }
        assert!(
            violations.is_empty(),
            "colours belong in theme.rs:\n{}",
            violations.join("\n")
        );
    }
}
