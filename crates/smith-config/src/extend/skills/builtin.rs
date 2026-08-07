//! The skills smith ships with, compiled in via `include_str!`.
//!
//! These are the harness's standard workflows — one file per activity, in the
//! same `SKILL.md` format user skills use, so the description lives in the
//! asset's own front matter and there is exactly one source of truth. They are
//! seeded into [`super::SkillCatalog`] as the least specific origin: a global
//! or project skill of the same name displaces one silently, which is the
//! designed customization path, not an error.
//!
//! An asset that fails to parse is skipped here and caught by a test instead —
//! the content is fixed at compile time, so a runtime error path would be dead
//! code the day it compiles.

use std::path::PathBuf;

use super::super::{FrontMatter, Origin};
use super::{clip, Skill, MAX_DESCRIPTION_CHARS};

/// Name and content of every embedded skill. The name must match what the
/// file would have been called as a directory: lowercase, `[a-z0-9-_]`.
const ASSETS: &[(&str, &str)] = &[
    ("choose-stack", include_str!("builtin/choose-stack.md")),
    ("code-review", include_str!("builtin/code-review.md")),
    ("commit", include_str!("builtin/commit.md")),
    ("debug", include_str!("builtin/debug.md")),
    ("delegate", include_str!("builtin/delegate.md")),
    ("fix-bug", include_str!("builtin/fix-bug.md")),
    ("goal", include_str!("builtin/goal.md")),
    ("loop", include_str!("builtin/loop.md")),
    ("new-feature", include_str!("builtin/new-feature.md")),
    ("new-project", include_str!("builtin/new-project.md")),
    ("plan", include_str!("builtin/plan.md")),
    ("refactor", include_str!("builtin/refactor.md")),
    ("research", include_str!("builtin/research.md")),
];

/// Every embedded skill, parsed. `dir` and `source` are synthetic — nothing
/// does I/O on them; they exist for display, and `Skill::rendered` branches on
/// the origin instead of naming a path that is not on disk.
pub(super) fn all() -> Vec<Skill> {
    ASSETS
        .iter()
        .filter_map(|(name, text)| {
            let parsed = FrontMatter::parse(text);
            let description = parsed.get("description")?;
            if parsed.body.trim().is_empty() {
                return None;
            }
            Some(Skill {
                name: (*name).to_string(),
                description: clip(description, MAX_DESCRIPTION_CHARS),
                body: parsed.body,
                dir: PathBuf::from("<built-in>"),
                source: PathBuf::from(format!("<built-in>/{name}")),
                origin: Origin::Builtin,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compile-time contract: every asset parses, carries a description,
    /// and stays within the same bounds a user skill is held to. A mangled
    /// front matter would otherwise silently drop the skill via `filter_map`.
    #[test]
    fn every_builtin_skill_parses_with_a_description_and_body() {
        let skills = all();
        assert_eq!(skills.len(), ASSETS.len(), "an asset failed to parse");
        for skill in &skills {
            assert!(
                skill.description.chars().count() <= MAX_DESCRIPTION_CHARS,
                "{}: description over the index budget",
                skill.name
            );
            assert!(!skill.body.trim().is_empty(), "{}: empty body", skill.name);
            assert!(
                skill.body.len() <= 16 * 1024,
                "{}: body over the sanity bound",
                skill.name
            );
            assert!(
                skill
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{}: not a usable skill name",
                skill.name
            );
            assert_eq!(skill.origin, Origin::Builtin);
        }
    }

    #[test]
    fn a_builtin_body_does_not_claim_to_be_loaded_from_a_file() {
        for skill in all() {
            let rendered = skill.rendered();
            assert!(
                rendered.contains("built into smith"),
                "{}: missing the built-in framing",
                skill.name
            );
            assert!(
                !rendered.contains("Loaded from"),
                "{}: claims a filesystem source",
                skill.name
            );
            assert!(
                !rendered.contains("Supporting files"),
                "{}: promises files read_file cannot reach",
                skill.name
            );
        }
    }
}
