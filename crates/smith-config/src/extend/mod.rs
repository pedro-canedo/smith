//! User-authored extension files — custom slash commands, skills, personas.
//!
//! All three are the same shape: markdown on disk, discovered in a project
//! directory and a global one, with optional `---` front matter carrying a
//! little metadata and a body that ends up in a prompt. This module holds the
//! half they share; the three submodules hold what differs.
//!
//! # Where the files live
//!
//! | kind     | project                    | global                       |
//! |----------|----------------------------|------------------------------|
//! | commands | `.smith/commands/**.md`    | `~/.smith/commands/**.md`    |
//! | skills   | `.smith/skills/*/SKILL.md` | `~/.smith/skills/*/SKILL.md` |
//! | personas | `.smith/personas/*.md`     | `~/.smith/personas/*.md`     |
//!
//! # Trust
//!
//! Every one of these is a file the user may have cloned along with a
//! repository, so the standard is the one [`crate::memory`] already set: a
//! path jail that resolves symlinks *before* the prefix check (a committed
//! symlink is otherwise a side door), a size cap, a file-count cap, and — where
//! the content reaches the model as anything other than the user's own words —
//! a header saying where it came from and that it ranks below the conversation.
//!
//! The jail is enforced on *discovery* rather than only on an `@import`-style
//! directive, because that is where the escape lives here: nothing in a
//! commands directory says "read this other file", but
//! `.smith/commands/deploy.md -> ~/.ssh/id_rsa` is a single `ln -s` in a
//! repository, and reading it would put the key straight into a prompt.

pub mod commands;
pub mod persona;
pub mod skills;

use std::path::{Path, PathBuf};

use crate::config_dir;
use crate::memory::{real_path, truncate_at_boundary};

/// Byte cap on one extension file's body.
///
/// Larger than a `SMITH.md` layer ([`crate::memory::MAX_MEMORY_BYTES`]) on
/// purpose: memory is paid on *every* request for the life of the session,
/// whereas a command body is paid once when it is invoked and a skill body
/// only if the model asks for it. A cap is still needed — an accidental
/// `cat`ted logfile in `.smith/commands/` should not be able to fill a context
/// window — but it can afford to be four times as generous.
pub const MAX_EXTENSION_BYTES: usize = 64 * 1024;

/// Files admitted per kind, per root. Bounds both the prompt cost of the
/// autocomplete/skill index and the number of `stat`s startup does.
pub const MAX_FILES: usize = 128;

/// How deep a kind's directory tree is walked. Commands are namespaced by
/// directory, so some nesting is the feature; four levels is already
/// `/a:b:c:d`, which nobody is typing.
pub const MAX_TREE_DEPTH: usize = 4;

/// Which of the two roots a file came from — or neither, for content
/// compiled into the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Embedded in smith itself via `include_str!`. Least specific of all:
    /// a global or project file of the same name displaces it.
    Builtin,
    /// `~/.smith/...` — the user's own, present in every project.
    Global,
    /// `<project>/.smith/...` — came with the repository.
    Project,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::Builtin => "built-in",
            Origin::Global => "global",
            Origin::Project => "project",
        }
    }
}

/// One searched directory and the jail its files may not escape.
#[derive(Debug, Clone)]
pub struct Root {
    pub dir: PathBuf,
    /// Resolved paths must stay under this. `~/.smith` for the global root,
    /// the project directory for the project root — the same two jails
    /// `crate::memory` uses, for the same reason.
    pub jail: PathBuf,
    pub origin: Origin,
}

/// The roots for one kind, least specific first (global, then project).
///
/// Ordering is load-bearing for every caller: the later root is the more
/// specific one, so "last wins" is a uniform rule rather than a per-kind
/// decision.
pub fn roots(kind: &str, project_root: &Path) -> Vec<Root> {
    roots_in(kind, config_dir().ok().as_deref(), project_root)
}

/// The same, with an explicit global directory — for tests, and for any
/// caller that already knows both boundaries.
pub fn roots_in(kind: &str, global_dir: Option<&Path>, project_root: &Path) -> Vec<Root> {
    let mut roots = Vec::new();
    if let Some(global) = global_dir {
        roots.push(Root {
            dir: global.join(kind),
            jail: global.to_path_buf(),
            origin: Origin::Global,
        });
    }
    roots.push(Root {
        dir: project_root.join(".smith").join(kind),
        jail: project_root.to_path_buf(),
        origin: Origin::Project,
    });
    roots
}

/// A markdown file found under a root: its path, and the `/`-joined path
/// relative to the root with the extension stripped (`db/migrate`).
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub rel: String,
    pub origin: Origin,
}

/// Every `.md` file under `root.dir`, recursively, that stays inside the jail.
///
/// Returns the hits in a deterministic order plus one problem line per file
/// that was refused, because a command that silently does not exist is
/// indistinguishable from one the user mistyped.
pub fn walk_markdown(root: &Root) -> (Vec<Found>, Vec<String>) {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    walk(root, &root.dir, "", 0, &mut found, &mut problems);
    (found, problems)
}

fn walk(
    root: &Root,
    dir: &Path,
    prefix: &str,
    depth: usize,
    found: &mut Vec<Found>,
    problems: &mut Vec<String>,
) {
    if depth > MAX_TREE_DEPTH {
        problems.push(format!(
            "{}: nested deeper than {MAX_TREE_DEPTH} directories, so it was not searched",
            dir.display()
        ));
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        // Absent is the overwhelmingly common case and is not a problem.
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        if found.len() >= MAX_FILES {
            problems.push(format!(
                "{}: more than {MAX_FILES} files here, so the rest were ignored",
                root.dir.display()
            ));
            return;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        // A dotfile is a backup or an editor artefact far more often than it
        // is a deliberate extension, and `.` is not a legal name segment
        // anyway (see `commands::valid_segment`).
        if name.starts_with('.') {
            continue;
        }

        // `is_dir`/`is_file` follow symlinks, which is deliberate: a symlinked
        // directory *inside* the jail is a perfectly reasonable way to share
        // commands between two checkouts. The jail check below is what stops
        // one pointing out.
        if path.is_dir() {
            let child = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            walk(root, &path, &child, depth + 1, found, problems);
            continue;
        }
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        if let Err(reason) = check_jail(&path, &root.jail) {
            problems.push(format!("{}: {reason}", path.display()));
            continue;
        }
        let stem = match name.rsplit_once('.') {
            Some((stem, _)) => stem.to_string(),
            None => name.clone(),
        };
        let rel = if prefix.is_empty() {
            stem
        } else {
            format!("{prefix}/{stem}")
        };
        found.push(Found {
            path,
            rel,
            origin: root.origin,
        });
    }
}

/// Immediate subdirectories of `root.dir` that stay inside the jail — the
/// shape skills use (`<dir>/SKILL.md`).
pub fn walk_dirs(root: &Root) -> (Vec<(String, PathBuf)>, Vec<String>) {
    let mut found = Vec::new();
    let mut problems = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root.dir) else {
        return (found, problems);
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        if found.len() >= MAX_FILES {
            problems.push(format!(
                "{}: more than {MAX_FILES} entries here, so the rest were ignored",
                root.dir.display()
            ));
            break;
        }
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if let Err(reason) = check_jail(&path, &root.jail) {
            problems.push(format!("{}: {reason}", path.display()));
            continue;
        }
        found.push((name, path));
    }
    (found, problems)
}

/// Refuses a path that resolves outside `jail`.
///
/// Delegates the hard part to [`crate::memory::real_path`], which
/// canonicalises as much of the path as exists — so a symlink is followed
/// before the prefix check rather than after it. The lexical `..` pass that
/// accompanies it in `memory::resolve_in_jail` is not needed here: these paths
/// come from `read_dir`, which never yields a `..` component.
pub fn check_jail(path: &Path, jail: &Path) -> Result<(), String> {
    let root = real_path(jail);
    if !real_path(path).starts_with(&root) {
        return Err(format!("it resolves outside {}", root.display()));
    }
    Ok(())
}

/// Reads a file, capped. Over the cap the tail is dropped and a notice takes
/// its place — never silently, for the same reason `memory::load` names the
/// files it dropped: instructions that vanish without a word are worse than
/// instructions that were never there.
pub fn read_capped(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("could not be read ({e})"))?;
    if text.len() <= MAX_EXTENSION_BYTES {
        return Ok(text);
    }
    let keep = truncate_at_boundary(&text, MAX_EXTENSION_BYTES);
    let mut out = text[..keep].to_string();
    out.push_str(&format!(
        "\n\n[smith: this file was cut off here — it is over the {MAX_EXTENSION_BYTES} byte \
         limit. The rest of it was NOT loaded and is not in effect.]\n"
    ));
    Ok(out)
}

/// A file split into its front matter and its body.
#[derive(Debug, Clone, Default)]
pub struct FrontMatter {
    keys: Vec<(String, String)>,
    pub body: String,
}

impl FrontMatter {
    /// Splits `---`-fenced front matter off the top of `source`.
    ///
    /// Front matter is **optional** here, unlike in
    /// `smith_core::SubagentDefinition::parse` — a subagent is unusable
    /// without a `name`, but a command is perfectly usable as a bare prompt in
    /// a file whose own name identifies it. A source with no opening `---`, or
    /// with one that is never closed, is therefore all body rather than an
    /// error: the failure mode of guessing wrong is a command that does not
    /// run, and that is worse than a command with no description.
    ///
    /// The grammar accepted is the same `key: value` subset, and unknown keys
    /// are ignored for the same reason: a file written for a later version of
    /// smith should degrade, not fail.
    pub fn parse(source: &str) -> Self {
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let all_body = || Self {
            keys: Vec::new(),
            body: source.trim().to_string(),
        };
        let Some(rest) = source
            .trim_start()
            .strip_prefix("---")
            .and_then(|r| r.trim_start_matches(['\r', ' ']).strip_prefix('\n'))
        else {
            return all_body();
        };

        let mut front = String::new();
        let mut body = String::new();
        let mut closed = false;
        for line in rest.split_inclusive('\n') {
            if !closed && line.trim_end() == "---" {
                closed = true;
                continue;
            }
            if closed {
                body.push_str(line);
            } else {
                front.push_str(line);
            }
        }
        if !closed {
            return all_body();
        }

        let mut keys = Vec::new();
        for line in front.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("- ") {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once(':') {
                keys.push((key.trim().to_ascii_lowercase(), unquote(value)));
            }
        }
        Self {
            keys,
            body: body.trim().to_string(),
        }
    }

    /// First value for `key`, or `None` when absent or empty.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
}

fn unquote(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .to_string()
}

/// First non-empty line of `body`, clipped — the fallback description for a
/// file whose front matter did not give one.
pub fn first_line(body: &str, max_chars: usize) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("---"))
        .unwrap_or("")
        .trim_start_matches('#')
        .trim();
    if line.chars().count() <= max_chars {
        return line.to_string();
    }
    format!("{}…", line.chars().take(max_chars).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_is_split_from_the_body() {
        let parsed = FrontMatter::parse("---\ndescription: Run migrations\n---\nDo the thing.\n");
        assert_eq!(parsed.get("description"), Some("Run migrations"));
        assert_eq!(parsed.body, "Do the thing.");
    }

    /// The opposite call from `SubagentDefinition::parse`, and the comment on
    /// `FrontMatter::parse` says why.
    #[test]
    fn a_file_with_no_front_matter_is_all_body() {
        let parsed = FrontMatter::parse("Just a prompt.\n");
        assert_eq!(parsed.body, "Just a prompt.");
        assert_eq!(parsed.get("description"), None);
    }

    #[test]
    fn unterminated_front_matter_is_all_body_rather_than_an_error() {
        let parsed = FrontMatter::parse("---\ndescription: x\nstill going\n");
        assert!(parsed.body.contains("still going"));
        assert_eq!(parsed.get("description"), None);
    }

    #[test]
    fn quoted_values_and_unknown_keys_are_tolerated() {
        let parsed = FrontMatter::parse("---\ndescription: \"a b\"\ncolour: teal\n---\nbody\n");
        assert_eq!(parsed.get("description"), Some("a b"));
        assert_eq!(parsed.body, "body");
    }

    #[test]
    fn an_empty_front_matter_value_reads_as_absent() {
        let parsed = FrontMatter::parse("---\ndescription:\n---\nbody\n");
        assert_eq!(parsed.get("description"), None);
    }

    #[test]
    fn first_line_falls_back_to_the_opening_heading() {
        assert_eq!(
            first_line("\n# Deploy the app\n\nsteps", 40),
            "Deploy the app"
        );
        assert_eq!(first_line("", 40), "");
        assert_eq!(
            first_line(&"x".repeat(50), 10),
            format!("{}…", "x".repeat(10))
        );
    }

    // --- discovery / jail -------------------------------------------------

    pub(super) struct Fixture {
        _tmp: tempfile::TempDir,
        pub global: PathBuf,
        pub project: PathBuf,
    }

    impl Fixture {
        pub fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let global = tmp.path().join("global");
            let project = tmp.path().join("project");
            std::fs::create_dir_all(&global).unwrap();
            std::fs::create_dir_all(&project).unwrap();
            Self {
                _tmp: tmp,
                global,
                project,
            }
        }

        /// Writes `rel` under the project directory.
        pub fn write(&self, rel: &str, body: &str) -> PathBuf {
            Self::write_at(&self.project, rel, body)
        }

        /// Writes `rel` under the global (`~/.smith` stand-in) directory.
        pub fn write_global(&self, rel: &str, body: &str) -> PathBuf {
            Self::write_at(&self.global, rel, body)
        }

        fn write_at(base: &Path, rel: &str, body: &str) -> PathBuf {
            let path = base.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            path
        }

        fn project_root(&self, kind: &str) -> Root {
            roots_in(kind, Some(&self.global), &self.project)
                .pop()
                .unwrap()
        }
    }

    #[test]
    fn the_walk_finds_nested_files_and_names_them_by_relative_path() {
        let fx = Fixture::new();
        fx.write(".smith/commands/deploy.md", "a");
        fx.write(".smith/commands/db/migrate.md", "b");
        fx.write(".smith/commands/notes.txt", "not markdown");

        let (found, problems) = walk_markdown(&fx.project_root("commands"));
        let rels: Vec<&str> = found.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["db/migrate", "deploy"]);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_project_is_refused_and_reported() {
        let fx = Fixture::new();
        let secret = fx.project.parent().unwrap().join("secret.md");
        std::fs::write(&secret, "SSH KEY MATERIAL").unwrap();
        std::fs::create_dir_all(fx.project.join(".smith/commands")).unwrap();
        std::os::unix::fs::symlink(&secret, fx.project.join(".smith/commands/leak.md")).unwrap();

        let (found, problems) = walk_markdown(&fx.project_root("commands"));
        assert!(found.is_empty(), "jail escaped: {found:?}");
        assert!(problems[0].contains("resolves outside"), "{problems:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_inside_the_project_is_followed() {
        let fx = Fixture::new();
        fx.write("shared/deploy.md", "shared body");
        std::fs::create_dir_all(fx.project.join(".smith/commands")).unwrap();
        std::os::unix::fs::symlink(
            fx.project.join("shared"),
            fx.project.join(".smith/commands/team"),
        )
        .unwrap();

        let (found, _) = walk_markdown(&fx.project_root("commands"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].rel, "team/deploy");
    }

    #[test]
    fn the_walk_stops_at_the_depth_cap() {
        let fx = Fixture::new();
        let deep = (0..MAX_TREE_DEPTH + 3).fold(String::from(".smith/commands"), |acc, i| {
            format!("{acc}/d{i}")
        });
        fx.write(&format!("{deep}/x.md"), "too deep");
        fx.write(".smith/commands/shallow.md", "fine");

        let (found, problems) = walk_markdown(&fx.project_root("commands"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rel, "shallow");
        assert!(problems.iter().any(|p| p.contains("nested deeper")));
    }

    #[test]
    fn the_file_count_is_capped_and_says_so() {
        let fx = Fixture::new();
        for i in 0..MAX_FILES + 5 {
            fx.write(&format!(".smith/commands/c{i:04}.md"), "body");
        }
        let (found, problems) = walk_markdown(&fx.project_root("commands"));
        assert_eq!(found.len(), MAX_FILES);
        assert!(problems.iter().any(|p| p.contains("more than")));
    }

    #[test]
    fn a_missing_directory_produces_nothing_and_no_complaint() {
        let fx = Fixture::new();
        let (found, problems) = walk_markdown(&fx.project_root("commands"));
        assert!(found.is_empty());
        assert!(problems.is_empty());
    }

    #[test]
    fn an_oversized_file_is_truncated_with_a_notice_rather_than_dropped() {
        let fx = Fixture::new();
        let path = fx.write(
            ".smith/commands/big.md",
            &format!(
                "opening line\n{}\nLAST",
                "y".repeat(MAX_EXTENSION_BYTES * 2)
            ),
        );
        let text = read_capped(&path).unwrap();
        assert!(text.contains("opening line"));
        assert!(!text.contains("LAST"));
        assert!(text.contains("was cut off here"));
    }

    #[test]
    fn walk_dirs_yields_immediate_subdirectories_only() {
        let fx = Fixture::new();
        fx.write(".smith/skills/pdf/SKILL.md", "a");
        fx.write(".smith/skills/pdf/reference.md", "b");
        fx.write(".smith/skills/loose.md", "c");

        let (dirs, problems) = walk_dirs(&fx.project_root("skills"));
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].0, "pdf");
        assert!(problems.is_empty());
    }

    #[test]
    fn the_global_root_comes_before_the_project_root() {
        let fx = Fixture::new();
        let roots = roots_in("commands", Some(&fx.global), &fx.project);
        assert_eq!(roots[0].origin, Origin::Global);
        assert_eq!(roots[1].origin, Origin::Project);
        // Each root's jail is its own tree, so a global file cannot reach into
        // the project and vice versa.
        assert_eq!(roots[0].jail, fx.global);
        assert_eq!(roots[1].jail, fx.project);
    }
}
