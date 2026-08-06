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
mod tests;
