//! Remappable key bindings for the TUI's discretionary commands.
//!
//! # Why this exists, concretely
//!
//! `Ctrl+B` is tmux's default prefix. A tmux user cannot press it — the
//! multiplexer eats it before smith ever sees the byte. `Ctrl+O` is `discard`
//! in a stock termios line discipline and a pane command in screen. These are
//! not hypothetical collisions; they are the default configuration of the two
//! programs most likely to be wrapped around a terminal agent.
//!
//! So the handful of bindings that are *ours* — the panes and panels, not the
//! text editing — are configurable. Deliberately a handful:
//!
//! - **Not** `Enter`, `Esc`, `Backspace`, or the arrows. Remapping the keys
//!   that submit, cancel, and edit turns a misconfiguration into a terminal
//!   you cannot type into or escape from, which is not a trade a config file
//!   should be able to make.
//! - **Not** `Ctrl+C`. It is checked before every other branch precisely so
//!   there is always a way out; a config that could move it could remove it.
//!
//! Bindings are `action = "key"` under `[keys]`, e.g.
//! `toggle_sidebar = "ctrl+t"`. An unknown action name or an unparseable key
//! is an error naming the offender, never a silent fallback: a keybinding that
//! quietly did not apply is worse than one that refused to load, because the
//! user concludes the feature is broken rather than their spelling.

use std::collections::BTreeMap;
use std::fmt;

use crossterm::event::{KeyCode, KeyModifiers};

/// A TUI command whose key can be changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyAction {
    ToggleSidebar,
    CycleSidebarTab,
    ToggleLogs,
    ToggleCardFocus,
    InsertNewline,
}

impl KeyAction {
    pub const ALL: [KeyAction; 5] = [
        KeyAction::ToggleSidebar,
        KeyAction::CycleSidebarTab,
        KeyAction::ToggleLogs,
        KeyAction::ToggleCardFocus,
        KeyAction::InsertNewline,
    ];

    /// The name used in `[keys]`.
    pub fn name(self) -> &'static str {
        match self {
            KeyAction::ToggleSidebar => "toggle_sidebar",
            KeyAction::CycleSidebarTab => "cycle_sidebar_tab",
            KeyAction::ToggleLogs => "toggle_logs",
            KeyAction::ToggleCardFocus => "toggle_card_focus",
            KeyAction::InsertNewline => "insert_newline",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.name() == name)
    }

    /// The binding shipped out of the box.
    fn default_binding(self) -> Binding {
        match self {
            KeyAction::ToggleSidebar => Binding::ctrl('b'),
            KeyAction::CycleSidebarTab => Binding::plain(KeyCode::BackTab),
            KeyAction::ToggleLogs => Binding::ctrl('l'),
            KeyAction::ToggleCardFocus => Binding::ctrl('o'),
            KeyAction::InsertNewline => Binding::ctrl('j'),
        }
    }
}

/// One key combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl Binding {
    fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
        }
    }

    fn plain(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Whether a keystroke triggers this binding.
    ///
    /// Only the three modifiers that carry meaning are compared. Terminals
    /// differ on whether they report `SHIFT` for a capital letter or for a
    /// function key, and a binding that matched only when the terminal agreed
    /// with us about that would work on some machines and not others.
    pub fn matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        const MEANINGFUL: KeyModifiers = KeyModifiers::CONTROL
            .union(KeyModifiers::ALT)
            .union(KeyModifiers::SHIFT);
        // A `Char` binding compares case-insensitively: `Ctrl+B` and `Ctrl+b`
        // are the same chord, and which one arrives depends on the terminal.
        let same_code = match (self.code, code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(&b),
            (a, b) => a == b,
        };
        // `BackTab` already *is* the shifted Tab, so terminals disagree about
        // whether to also set SHIFT. Ignore the modifier entirely for it.
        if self.code == KeyCode::BackTab {
            return same_code;
        }
        same_code && (modifiers & MEANINGFUL) == (self.modifiers & MEANINGFUL)
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            write!(f, "Ctrl+")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            write!(f, "Alt+")?;
        }
        match self.code {
            KeyCode::Char(c) => write!(f, "{}", c.to_ascii_uppercase()),
            KeyCode::BackTab => write!(f, "Shift+Tab"),
            KeyCode::Tab => write!(f, "Tab"),
            KeyCode::Enter => write!(f, "Enter"),
            KeyCode::F(n) => write!(f, "F{n}"),
            other => write!(f, "{other:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMapError {
    UnknownAction(String),
    UnparseableKey {
        action: String,
        key: String,
    },
    Conflict {
        first: String,
        second: String,
        key: String,
    },
}

impl fmt::Display for KeyMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyMapError::UnknownAction(name) => write!(
                f,
                "unknown key action `{name}` — valid actions are: {}",
                KeyAction::ALL
                    .iter()
                    .map(|a| a.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            KeyMapError::UnparseableKey { action, key } => write!(
                f,
                "cannot parse key `{key}` for `{action}` — expected something \
                 like `ctrl+b`, `alt+enter`, `shift+tab` or `f5`"
            ),
            KeyMapError::Conflict { first, second, key } => write!(
                f,
                "`{first}` and `{second}` are both bound to `{key}` — one of \
                 them would never fire"
            ),
        }
    }
}

impl std::error::Error for KeyMapError {}

/// Every action's current binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMap {
    bindings: BTreeMap<KeyAction, Binding>,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self {
            bindings: KeyAction::ALL
                .into_iter()
                .map(|a| (a, a.default_binding()))
                .collect(),
        }
    }
}

impl KeyMap {
    /// Builds a map from the `[keys]` table, defaulting anything unlisted.
    ///
    /// A binding that would shadow another is refused rather than accepted
    /// with the loser silently dead — two actions on one key is always a
    /// mistake, and the one that stops working is not predictable from the
    /// file.
    pub fn from_overrides<'a, I>(overrides: I) -> Result<Self, KeyMapError>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut map = Self::default();
        for (name, key) in overrides {
            let action = KeyAction::from_name(name)
                .ok_or_else(|| KeyMapError::UnknownAction(name.to_string()))?;
            let binding = parse_binding(key).ok_or_else(|| KeyMapError::UnparseableKey {
                action: name.to_string(),
                key: key.to_string(),
            })?;
            map.bindings.insert(action, binding);
        }

        // Conflicts are checked after every override is applied: a config that
        // *swaps* two bindings passes through an intermediate state where they
        // collide, and refusing that would make a swap impossible to express.
        let mut seen: Vec<(KeyAction, Binding)> = Vec::new();
        for (action, binding) in &map.bindings {
            if let Some((other, _)) = seen
                .iter()
                .find(|(_, b)| b.matches(binding.code, binding.modifiers))
            {
                return Err(KeyMapError::Conflict {
                    first: other.name().to_string(),
                    second: action.name().to_string(),
                    key: binding.to_string(),
                });
            }
            seen.push((*action, *binding));
        }
        Ok(map)
    }

    pub fn binding(&self, action: KeyAction) -> Binding {
        self.bindings
            .get(&action)
            .copied()
            .unwrap_or_else(|| action.default_binding())
    }

    /// The action a keystroke triggers, if any.
    pub fn action_for(&self, code: KeyCode, modifiers: KeyModifiers) -> Option<KeyAction> {
        self.bindings
            .iter()
            .find(|(_, b)| b.matches(code, modifiers))
            .map(|(a, _)| *a)
    }
}

/// Parses `ctrl+b`, `alt+enter`, `shift+tab`, `f5`, `esc`.
fn parse_binding(text: &str) -> Option<Binding> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
    // `+` is both the separator and a key, so the tail needs disambiguating.
    // `ctrl++` splits to ["ctrl", "", ""] and means Ctrl and the plus key;
    // `ctrl+` splits to ["ctrl", ""] and means a dangling separator, which is
    // a typo rather than a binding.
    let key = if parts.last() == Some(&"") {
        if parts.len() >= 2 && parts[parts.len() - 2].is_empty() {
            parts.pop();
            parts.pop();
            "+"
        } else {
            return None;
        }
    } else {
        parts.pop()?
    };

    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }

    let lowered = key.to_ascii_lowercase();
    let code = match lowered.as_str() {
        "tab" if modifiers.contains(KeyModifiers::SHIFT) => {
            // `shift+tab` is `BackTab` on the wire, not Tab with a modifier.
            modifiers.remove(KeyModifiers::SHIFT);
            KeyCode::BackTab
        }
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        "delete" | "del" => KeyCode::Delete,
        "backspace" => KeyCode::Backspace,
        f if f.starts_with('f') && f.len() > 1 => KeyCode::F(f[1..].parse().ok()?),
        other => {
            let mut chars = other.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    };

    Some(Binding { code, modifiers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_keys_that_used_to_be_hardcoded() {
        let map = KeyMap::default();
        assert!(map
            .binding(KeyAction::ToggleSidebar)
            .matches(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(map
            .binding(KeyAction::ToggleLogs)
            .matches(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert!(map
            .binding(KeyAction::CycleSidebarTab)
            .matches(KeyCode::BackTab, KeyModifiers::SHIFT));
    }

    /// The motivating case: tmux owns Ctrl+B, so move it.
    #[test]
    fn an_override_moves_a_binding_and_the_old_key_stops_firing() {
        let map = KeyMap::from_overrides([("toggle_sidebar", "ctrl+t")]).unwrap();
        assert_eq!(
            map.action_for(KeyCode::Char('t'), KeyModifiers::CONTROL),
            Some(KeyAction::ToggleSidebar)
        );
        assert_eq!(
            map.action_for(KeyCode::Char('b'), KeyModifiers::CONTROL),
            None,
            "the tmux-shadowed key is free again"
        );
    }

    #[test]
    fn unlisted_actions_keep_their_defaults() {
        let map = KeyMap::from_overrides([("toggle_sidebar", "ctrl+t")]).unwrap();
        assert_eq!(
            map.action_for(KeyCode::Char('o'), KeyModifiers::CONTROL),
            Some(KeyAction::ToggleCardFocus)
        );
    }

    #[test]
    fn an_unknown_action_names_itself_and_the_valid_ones() {
        let err = KeyMap::from_overrides([("togle_sidebar", "ctrl+t")]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("togle_sidebar"), "{text}");
        assert!(text.contains("toggle_sidebar"), "{text}");
    }

    #[test]
    fn an_unparseable_key_is_an_error_not_a_silent_default() {
        let err = KeyMap::from_overrides([("toggle_sidebar", "ctrl+")]).unwrap_err();
        assert!(matches!(err, KeyMapError::UnparseableKey { .. }), "{err:?}");
        let err = KeyMap::from_overrides([("toggle_sidebar", "hyper+q")]).unwrap_err();
        assert!(matches!(err, KeyMapError::UnparseableKey { .. }), "{err:?}");
    }

    /// Two actions on one key means one of them silently never fires.
    #[test]
    fn binding_two_actions_to_one_key_is_refused() {
        let err = KeyMap::from_overrides([("toggle_sidebar", "ctrl+o")]).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("toggle_card_focus"), "{text}");
        assert!(text.contains("toggle_sidebar"), "{text}");
    }

    /// A swap passes through a colliding intermediate state; it must still be
    /// expressible.
    #[test]
    fn swapping_two_bindings_is_allowed() {
        let map = KeyMap::from_overrides([
            ("toggle_sidebar", "ctrl+o"),
            ("toggle_card_focus", "ctrl+b"),
        ])
        .unwrap();
        assert_eq!(
            map.action_for(KeyCode::Char('o'), KeyModifiers::CONTROL),
            Some(KeyAction::ToggleSidebar)
        );
        assert_eq!(
            map.action_for(KeyCode::Char('b'), KeyModifiers::CONTROL),
            Some(KeyAction::ToggleCardFocus)
        );
    }

    #[test]
    fn parsing_covers_the_shapes_a_user_would_actually_write() {
        let cases = [
            ("ctrl+b", KeyCode::Char('b'), KeyModifiers::CONTROL),
            ("Ctrl+B", KeyCode::Char('b'), KeyModifiers::CONTROL),
            ("alt+enter", KeyCode::Enter, KeyModifiers::ALT),
            ("f5", KeyCode::F(5), KeyModifiers::NONE),
            ("esc", KeyCode::Esc, KeyModifiers::NONE),
            ("pgup", KeyCode::PageUp, KeyModifiers::NONE),
            ("ctrl++", KeyCode::Char('+'), KeyModifiers::CONTROL),
        ];
        for (text, code, modifiers) in cases {
            let parsed = parse_binding(text).unwrap_or_else(|| panic!("failed to parse {text}"));
            assert_eq!(parsed.code, code, "{text}");
            assert_eq!(parsed.modifiers, modifiers, "{text}");
        }
    }

    /// `shift+tab` is `BackTab` on the wire — a binding written the obvious
    /// way has to match the event that actually arrives.
    #[test]
    fn shift_tab_parses_to_backtab_and_matches_the_real_event() {
        let binding = parse_binding("shift+tab").unwrap();
        assert_eq!(binding.code, KeyCode::BackTab);
        assert!(binding.matches(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(
            binding.matches(KeyCode::BackTab, KeyModifiers::NONE),
            "terminals disagree about reporting SHIFT for BackTab"
        );
    }

    #[test]
    fn a_char_binding_ignores_the_case_the_terminal_reports() {
        let binding = parse_binding("ctrl+b").unwrap();
        assert!(binding.matches(KeyCode::Char('B'), KeyModifiers::CONTROL));
    }

    #[test]
    fn a_binding_prints_the_way_a_hint_line_should_read() {
        assert_eq!(parse_binding("ctrl+b").unwrap().to_string(), "Ctrl+B");
        assert_eq!(parse_binding("shift+tab").unwrap().to_string(), "Shift+Tab");
        assert_eq!(parse_binding("alt+enter").unwrap().to_string(), "Alt+Enter");
    }
}
