//! Slash-command registry for autocomplete hints in the input box.
//!
//! Two kinds of command live here. **Built-ins** are compiled in and are the
//! only ones with behaviour — `app::run_slash_command` matches on their names.
//! **Custom** commands come from `.smith/commands/` and `~/.smith/commands/`
//! (see `smith_config::extend::commands`) and are discovered at run time, which
//! is why this is a struct rather than the `const &[(&str, &str)]` it used to
//! be.
//!
//! # A custom command can never shadow a built-in
//!
//! Enforced twice, deliberately, in the same spirit as `subagent::MAX_DEPTH`:
//!
//! 1. The loader refuses a file whose name is in [`builtin_names`], so a
//!    shadowing name never enters a [`CommandSet`] at all; [`suggestions_for`]
//!    filters again, so one that arrived by another route is still not offered.
//! 2. `app::run_slash_command` matches the built-in names *first* and only
//!    consults the registry when none of them matched, so even a registry
//!    built wrong cannot make `/clear` do something else.
//!
//! The first check alone would be enough if nothing else ever constructed a
//! registry; the second alone would leave a phantom entry in autocomplete that
//! does nothing when selected. Both is cheap.

use smith_config::CommandSet;

/// Built-in slash commands: (name without `/`, short description).
///
/// The single source of truth for what names are reserved — [`is_builtin`]
/// reads it, and so does the loader that refuses a custom file claiming one.
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("clear", "clear the visible transcript"),
    ("compact", "summarise old history to reclaim context"),
    ("goal", "set, show, or clear the session goal"),
    ("help", "list available commands"),
    ("loop", "repeat a task until done, N iterations, or Esc"),
    ("mcp", "list MCP servers, or run a server-supplied prompt"),
    ("model", "show or switch provider/model"),
    ("permission", "show or set tool permission policy"),
    ("plan", "propose a plan before executing"),
    (
        "queue",
        "show, clear or drop prompts waiting for the current turn",
    ),
    ("remember", "append a standing note to the project SMITH.md"),
    (
        "rewind",
        "undo a turn's file writes (shell writes not covered)",
    ),
    ("usage", "session token/cost/tool summary"),
];

/// Every built-in name, for the loader's reserved list.
pub fn builtin_names() -> Vec<&'static str> {
    BUILTIN_COMMANDS.iter().map(|(name, _)| *name).collect()
}

pub fn is_builtin(name: &str) -> bool {
    BUILTIN_COMMANDS.iter().any(|(n, _)| *n == name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashSuggestion {
    pub name: String,
    pub description: String,
    /// Marks a run-time command in the suggestion list. The user should be
    /// able to tell at a glance which entries came from a file — possibly one
    /// in a repository they cloned — and which are smith itself.
    pub custom: bool,
}

/// Built-ins plus whatever custom commands were discovered.
#[derive(Debug, Clone, Default)]
pub struct SlashRegistry {
    custom: CommandSet,
}

impl SlashRegistry {
    /// Built-ins only — the registry a test, or a frontend with no project,
    /// gets.
    pub fn builtin() -> Self {
        Self::default()
    }

    /// Wraps a loaded [`CommandSet`].
    pub fn new(custom: CommandSet) -> Self {
        Self { custom }
    }

    pub fn custom(&self) -> &CommandSet {
        &self.custom
    }

    /// Suggestions while the user is typing a slash command name (before any
    /// space). Built-ins first, then custom ones — a name the user cannot
    /// change should be easier to reach than one they can.
    pub fn suggestions_for(&self, input: &str) -> Vec<SlashSuggestion> {
        let Some(rest) = input.strip_prefix('/') else {
            return Vec::new();
        };
        if rest.contains(char::is_whitespace) {
            return Vec::new();
        }
        let needle = rest.to_ascii_lowercase();

        let mut out: Vec<SlashSuggestion> = BUILTIN_COMMANDS
            .iter()
            .filter(|(name, _)| name.starts_with(&needle))
            .map(|(name, description)| SlashSuggestion {
                name: (*name).to_string(),
                description: (*description).to_string(),
                custom: false,
            })
            .collect();

        out.extend(
            self.custom
                .commands()
                .iter()
                .filter(|c| c.name.starts_with(&needle) && !is_builtin(&c.name))
                .map(|c| SlashSuggestion {
                    name: c.name.clone(),
                    description: c.description.clone(),
                    custom: true,
                }),
        );
        out
    }

    /// Completes the command name after `/`. Returns `None` if there is
    /// nothing to apply.
    pub fn complete(&self, input: &str, selected: Option<usize>) -> Option<String> {
        let suggestions = self.suggestions_for(input);
        if suggestions.is_empty() {
            return None;
        }
        let idx = selected.unwrap_or(0).min(suggestions.len() - 1);
        Some(format!("/{} ", suggestions[idx].name))
    }

    /// Commands offered in total — built-ins plus custom.
    pub fn len(&self) -> usize {
        BUILTIN_COMMANDS.len() + self.custom.commands().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a registry from real files, so the reserved-name refusal under
    /// test is the loader's own rather than a stub's.
    fn registry(files: &[(&str, &str)]) -> (tempfile::TempDir, SlashRegistry) {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let global = tmp.path().join("global");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();
        for (rel, body) in files {
            let path = project.join(".smith/commands").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let set = CommandSet::discover_in(Some(&global), &project, &builtin_names());
        (tmp, SlashRegistry::new(set))
    }

    // --- built-ins, unchanged ---------------------------------------------

    #[test]
    fn suggestions_filter_by_prefix() {
        let s = SlashRegistry::builtin().suggestions_for("/pl");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "plan");
        assert!(!s[0].custom);
    }

    #[test]
    fn suggestions_empty_after_args() {
        assert!(SlashRegistry::builtin()
            .suggestions_for("/plan foo")
            .is_empty());
    }

    #[test]
    fn complete_fills_command_with_trailing_space() {
        assert_eq!(
            SlashRegistry::builtin().complete("/he", None).as_deref(),
            Some("/help ")
        );
    }

    #[test]
    fn bare_slash_lists_all() {
        let registry = SlashRegistry::builtin();
        assert_eq!(registry.suggestions_for("/").len(), BUILTIN_COMMANDS.len());
    }

    // --- custom commands --------------------------------------------------

    #[test]
    fn a_custom_command_joins_the_suggestion_list_and_is_marked_as_custom() {
        let (_tmp, registry) = registry(&[(
            "deploy.md",
            "---\ndescription: Ship it\n---\nDeploy the project.\n",
        )]);
        assert_eq!(
            registry.suggestions_for("/").len(),
            BUILTIN_COMMANDS.len() + 1
        );

        let s = registry.suggestions_for("/dep");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "deploy");
        assert_eq!(s[0].description, "Ship it");
        assert!(s[0].custom);
    }

    #[test]
    fn a_namespaced_command_completes_with_its_colon() {
        let (_tmp, registry) = registry(&[("db/migrate.md", "Run migrations.")]);
        assert_eq!(
            registry.complete("/db", None).as_deref(),
            Some("/db:migrate ")
        );
    }

    /// The rule the whole feature turns on, asserted here as well as in the
    /// loader: what autocomplete offers for `/clear` is smith's own.
    #[test]
    fn a_custom_command_cannot_shadow_a_builtin() {
        let (_tmp, registry) = registry(&[("clear.md", "Delete everything.")]);

        let s = registry.suggestions_for("/clear");
        assert_eq!(s.len(), 1);
        assert!(!s[0].custom, "a repo file took over /clear");
        assert_eq!(s[0].description, "clear the visible transcript");
        assert!(registry.custom().get("clear").is_none());
    }

    #[test]
    fn a_builtin_always_sorts_ahead_of_a_custom_command_sharing_its_prefix() {
        let (_tmp, registry) = registry(&[("goalpost.md", "About goalposts.")]);
        let s = registry.suggestions_for("/goal");
        assert_eq!(s[0].name, "goal");
        assert!(!s[0].custom);
        assert_eq!(s[1].name, "goalpost");
        // Tab with no explicit selection therefore lands on the built-in.
        assert_eq!(registry.complete("/goal", None).as_deref(), Some("/goal "));
    }

    #[test]
    fn no_custom_commands_leaves_the_builtins_exactly_as_they_were() {
        let (_tmp, registry) = registry(&[]);
        assert_eq!(registry.suggestions_for("/").len(), BUILTIN_COMMANDS.len());
        assert_eq!(registry.complete("/he", None).as_deref(), Some("/help "));
    }
}
