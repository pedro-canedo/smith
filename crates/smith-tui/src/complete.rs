//! Completion for the prompt box: slash commands, and `@path` file references.
//!
//! Both kinds share one list widget and one key set (Tab accepts, up/down
//! select), so they share one type here. What differs is only where the
//! candidates come from and which character introduces them.

use std::path::Path;

use crate::slash::SlashSuggestion;

/// What the caret is currently completing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionKind {
    #[default]
    Slash,
    File,
}

impl CompletionKind {
    /// Printed before each candidate, so the list reads the way the accepted
    /// text will.
    pub fn prefix(self) -> char {
        match self {
            CompletionKind::Slash => '/',
            CompletionKind::File => '@',
        }
    }
}

/// Entries the file index will hold at most.
///
/// A repository large enough to exceed this is one where scrolling a
/// completion list was never going to be how you find a file — and an
/// unbounded walk of, say, a monorepo would stall the first `@` keystroke for
/// seconds while holding every path in memory.
pub const MAX_INDEXED_FILES: usize = 20_000;

/// Candidates shown at once. The list widget caps its own height at six.
const MAX_SUGGESTIONS: usize = 6;

/// Walks `root` for files a prompt might reference.
///
/// Uses `ignore::WalkBuilder`, so `.gitignore` is honoured for free — the same
/// reason `grep` is built on it rather than on a hand-rolled walk. A prompt
/// that completes to `target/debug/build/...` is worse than no completion at
/// all.
pub fn index_files(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .parents(true)
        .build();

    for entry in walker.flatten() {
        if out.len() >= MAX_INDEXED_FILES {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path().strip_prefix(root).unwrap_or(entry.path());
        // Lossy rather than skipped: a path this crate cannot render as UTF-8
        // is still a path the user may want, and showing it mangled is more
        // useful than pretending it does not exist.
        out.push(path.to_string_lossy().replace('\\', "/"));
    }
    out.sort();
    out
}

/// The `@`-token immediately before the caret, if the caret is in one.
///
/// Returns the text after the `@`. `None` when there is no `@` token — which
/// includes an `@` that is part of a larger word (`user@host`), since that is
/// an email or an SSH target, not a file reference.
pub fn file_token(text: &str) -> Option<&str> {
    let at = text.rfind('@')?;
    // Only at the start of the input or after whitespace.
    if at > 0 && !text[..at].ends_with(char::is_whitespace) {
        return None;
    }
    let token = &text[at + 1..];
    // A space closes the token: the user has moved on to the next word.
    if token.contains(char::is_whitespace) {
        return None;
    }
    Some(token)
}

/// Files matching `token`, best first.
///
/// Ranking, in order: a path whose *file name* starts with the token, then one
/// whose full path starts with it, then any containing it. Substring matching
/// last rather than not at all — `@app.rs` should find
/// `crates/smith-tui/src/app.rs` without typing the three directories first,
/// which is the whole reason to have this.
pub fn file_suggestions(files: &[String], token: &str) -> Vec<SlashSuggestion> {
    let needle = token.to_ascii_lowercase();
    let mut scored: Vec<(u8, &String)> = Vec::new();

    for path in files {
        let lower = path.to_ascii_lowercase();
        let name = lower.rsplit('/').next().unwrap_or(&lower);
        let rank = if needle.is_empty() {
            2
        } else if name.starts_with(&needle) {
            0
        } else if lower.starts_with(&needle) {
            1
        } else if lower.contains(&needle) {
            2
        } else {
            continue;
        };
        scored.push((rank, path));
    }

    // Shorter paths first within a rank: the closer a match is to the root,
    // the more likely it is the one meant.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.len().cmp(&b.1.len())));
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, path)| SlashSuggestion {
            name: path.clone(),
            description: String::new(),
            custom: false,
        })
        .collect()
}

/// Replaces the `@token` under the caret with `path`, leaving a trailing space.
pub fn accept_file(text: &str, path: &str) -> String {
    match text.rfind('@') {
        Some(at) => format!("{}@{path} ", &text[..at]),
        None => format!("@{path} "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        vec![
            "Cargo.toml".to_string(),
            "crates/smith-tui/src/app.rs".to_string(),
            "crates/smith-core/src/agent.rs".to_string(),
            "docs/design-system.md".to_string(),
            "src/app.rs".to_string(),
        ]
    }

    fn names(token: &str) -> Vec<String> {
        file_suggestions(&files(), token)
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    #[test]
    fn a_token_is_the_text_after_an_at_that_starts_a_word() {
        assert_eq!(file_token("@app"), Some("app"));
        assert_eq!(file_token("look at @src/li"), Some("src/li"));
        assert_eq!(file_token("@"), Some(""));
    }

    #[test]
    fn an_at_inside_a_word_is_not_a_file_reference() {
        // An email address or an SSH target must not open a file list.
        assert_eq!(file_token("mail me at pedro@example.com"), None);
        assert_eq!(file_token("git@github.com:me/repo"), None);
    }

    #[test]
    fn a_space_after_the_token_closes_it() {
        assert_eq!(file_token("@app.rs and then"), None);
    }

    #[test]
    fn a_file_name_match_outranks_a_path_match() {
        // `app.rs` is the file name of two entries and a substring of neither
        // directory — the shorter path wins the tie.
        assert_eq!(names("app.rs").first().unwrap(), "src/app.rs");
    }

    #[test]
    fn a_bare_file_name_finds_a_file_nested_anywhere() {
        // The whole point: no need to type the three directories first.
        assert!(names("agent.rs").contains(&"crates/smith-core/src/agent.rs".to_string()));
    }

    #[test]
    fn matching_ignores_case() {
        assert!(names("CARGO").contains(&"Cargo.toml".to_string()));
    }

    #[test]
    fn a_token_matching_nothing_suggests_nothing() {
        assert!(names("zzzz").is_empty());
    }

    #[test]
    fn an_empty_token_lists_something_to_start_from() {
        assert!(!names("").is_empty());
    }

    #[test]
    fn accepting_replaces_the_token_and_leaves_a_trailing_space() {
        assert_eq!(
            accept_file("look at @ap", "src/app.rs"),
            "look at @src/app.rs "
        );
    }

    #[test]
    fn accepting_leaves_the_rest_of_the_prompt_alone() {
        assert_eq!(
            accept_file("explain @a", "Cargo.toml"),
            "explain @Cargo.toml "
        );
    }

    #[test]
    fn indexing_skips_directories_and_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        std::fs::write(root.join("target/junk.rs"), "").unwrap();

        let found = index_files(root);
        assert!(found.contains(&"src/lib.rs".to_string()), "{found:?}");
        assert!(
            !found.iter().any(|f| f.starts_with("target/")),
            "ignored directory was indexed: {found:?}"
        );
        assert!(
            !found.iter().any(|f| f == "src"),
            "a directory was indexed as a file: {found:?}"
        );
    }
}
