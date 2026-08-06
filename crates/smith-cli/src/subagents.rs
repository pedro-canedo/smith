//! Loads subagent definitions from `~/.smith/agents/*.md`.
//!
//! The parsing lives in `smith_core::subagent` (it is pure, and the agent is
//! what consumes the result); only the *where* lives here, because knowing
//! that a user has a home directory is exactly the kind of environment
//! knowledge `smith-core` is kept free of.
//!
//! Global rather than per-project, deliberately. A subagent is a way of
//! working — "search like this, report like that" — and a user who wrote one
//! wants it in every repository they open. A project-local override would also
//! mean a cloned repository could ship a prompt that runs against the user's
//! model on their key, in a directory listing nobody thinks to read.

use std::path::{Path, PathBuf};

use smith_core::SubagentDefinition;

/// `~/.smith/agents`.
pub fn agents_dir() -> Option<PathBuf> {
    smith_config::config_dir().ok().map(|d| d.join("agents"))
}

/// Every definition in `dir`, plus a line per file that could not be read or
/// parsed.
///
/// **A broken definition never fails startup.** It costs its own subagent and
/// nothing else: the other definitions load, and the built-in general-purpose
/// child is always there regardless. A typo in an optional convenience file
/// that stopped smith from starting would be a far worse bug than the typo.
///
/// Duplicate names are resolved by filename order, first one wins, and the
/// loser is reported — silently picking one of two same-named subagents makes
/// `task` non-deterministic in a way that is very hard to notice.
pub fn load_from(dir: &Path) -> (Vec<SubagentDefinition>, Vec<String>) {
    let mut definitions: Vec<SubagentDefinition> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    let Ok(entries) = std::fs::read_dir(dir) else {
        // No directory at all is the overwhelmingly common case, and it is not
        // a problem — it means the user never wrote one.
        return (definitions, problems);
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")))
        .collect();
    paths.sort();

    for path in paths {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(e) => {
                problems.push(format!("{name}: could not be read ({e})"));
                continue;
            }
        };
        match SubagentDefinition::parse(&source) {
            Ok(def) => {
                if let Some(clash) = definitions.iter().find(|d| d.name == def.name) {
                    problems.push(format!(
                        "{name}: a subagent named `{}` is already defined, so this file is \
                         ignored",
                        clash.name
                    ));
                    continue;
                }
                definitions.push(def);
            }
            Err(e) => problems.push(format!("{name}: {e}")),
        }
    }

    (definitions, problems)
}

/// `load_from` against the real `~/.smith/agents`.
pub fn load() -> (Vec<SubagentDefinition>, Vec<String>) {
    match agents_dir() {
        Some(dir) => load_from(&dir),
        None => (Vec::new(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_missing_directory_is_not_a_problem() {
        let (defs, problems) = load_from(Path::new("/definitely/not/here"));
        assert!(defs.is_empty());
        assert!(problems.is_empty());
    }

    #[test]
    fn every_markdown_definition_is_loaded_and_nothing_else_is() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "doc-finder.md",
            "---\nname: doc-finder\ndescription: finds docs\ntools: read_file\n---\nQuote it.\n",
        );
        write(
            dir.path(),
            "notes.txt",
            "---\nname: nope\ndescription: nope\n---\n",
        );

        let (defs, problems) = load_from(dir.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "doc-finder");
        assert_eq!(defs[0].instructions, "Quote it.");
        assert!(problems.is_empty());
    }

    /// One bad file must not take the good ones down with it.
    #[test]
    fn a_broken_definition_costs_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "good.md",
            "---\nname: good\ndescription: d\n---\n",
        );
        write(dir.path(), "broken.md", "no front matter here\n");

        let (defs, problems) = load_from(dir.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "good");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].starts_with("broken.md:"), "{:?}", problems);
    }

    #[test]
    fn a_duplicate_name_is_reported_rather_than_silently_shadowing() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a.md",
            "---\nname: twin\ndescription: first\n---\n",
        );
        write(
            dir.path(),
            "b.md",
            "---\nname: twin\ndescription: second\n---\n",
        );

        let (defs, problems) = load_from(dir.path());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].description, "first");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("already defined"), "{:?}", problems);
    }
}
