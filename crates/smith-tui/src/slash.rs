//! Slash-command registry for autocomplete hints in the input box.

/// Known slash commands: (name without `/`, short description).
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("clear", "clear the visible transcript"),
    ("goal", "set, show, or clear the session goal"),
    ("help", "list available commands"),
    ("loop", "repeat a task until done, N iterations, or Esc"),
    ("model", "show or switch provider/model"),
    ("permission", "show or set tool permission policy"),
    ("plan", "propose a plan before executing"),
    ("usage", "session token/cost/tool summary"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashSuggestion {
    pub name: &'static str,
    pub description: &'static str,
}

/// Suggestions while the user is typing a slash command name (before any space).
pub fn suggestions_for(input: &str) -> Vec<SlashSuggestion> {
    let Some(rest) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if rest.contains(char::is_whitespace) {
        return Vec::new();
    }
    let needle = rest.to_ascii_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(&needle))
        .map(|(name, description)| SlashSuggestion { name, description })
        .collect()
}

/// Completes the command name after `/`. Returns `None` if there is nothing to apply.
pub fn complete(input: &str, selected: Option<usize>) -> Option<String> {
    let suggestions = suggestions_for(input);
    if suggestions.is_empty() {
        return None;
    }
    let idx = selected.unwrap_or(0).min(suggestions.len() - 1);
    Some(format!("/{} ", suggestions[idx].name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_filter_by_prefix() {
        let s = suggestions_for("/pl");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "plan");
    }

    #[test]
    fn suggestions_empty_after_args() {
        assert!(suggestions_for("/plan foo").is_empty());
    }

    #[test]
    fn complete_fills_command_with_trailing_space() {
        assert_eq!(complete("/he", None).as_deref(), Some("/help "));
    }

    #[test]
    fn bare_slash_lists_all() {
        assert_eq!(suggestions_for("/").len(), SLASH_COMMANDS.len());
    }
}
