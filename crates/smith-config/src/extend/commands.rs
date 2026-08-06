//! Custom slash commands: `.smith/commands/**.md` and `~/.smith/commands/**.md`.
//!
//! A command file is a prompt. Typing `/db:migrate users` expands
//! `.smith/commands/db/migrate.md` with the arguments substituted in and
//! submits the result as the user's message — nothing more. There is no new
//! `Action`, no new agent capability, and no way for a command to reach past
//! the prompt it produces.
//!
//! # Namespacing
//!
//! The path relative to the commands root, minus `.md`, with `/` written as
//! `:` — so `.smith/commands/db/migrate.md` is `/db:migrate`.
//!
//! `:` rather than `/` because the command already opens with a slash and
//! `/db/migrate` reads as a path, which invites the guess that it is one.
//! `:` is also what MCP already uses to qualify a tool by its server
//! (`server:tool`), so a future `/`-command sourced from an MCP server reads
//! the same way without a second convention being invented for it.
//!
//! # Precedence and shadowing
//!
//! Two rules, and they are different on purpose:
//!
//! - **A custom command may never take a built-in's name.** `/clear` doing
//!   something else because a repository defined it is a trap, and the trap is
//!   worse than the inconvenience of the name being taken. The file is refused
//!   with a problem line naming it.
//! - **Between two custom commands, the project's wins**, matching
//!   `Config::load_layered` and `MemoryScope`'s "more specific wins". The
//!   shadowed global file is reported rather than silently dropped, and — the
//!   part that actually defuses this — the frontend puts the *expanded body*
//!   in the transcript as the user's message, so a command that is not what
//!   the user expected is visible in the same breath as it runs.

use std::path::{Path, PathBuf};

use super::{first_line, read_capped, roots, roots_in, Found, FrontMatter, Origin};

/// Longest a generated description may be in the autocomplete list.
const DESCRIPTION_CHARS: usize = 72;

/// Separator between namespace segments in a command name.
pub const NAMESPACE_SEPARATOR: char = ':';

/// One loaded command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// `db:migrate` — what the user types after `/`.
    pub name: String,
    /// One line for the autocomplete list. From front matter `description`,
    /// else the file's first non-empty line.
    pub description: String,
    /// The prompt, front matter removed and placeholders still unexpanded.
    pub body: String,
    pub source: PathBuf,
    pub origin: Origin,
}

impl CustomCommand {
    /// Substitutes `$1`..`$9`, `$ARGUMENTS` and `$$` into the body.
    ///
    /// `$ARGUMENTS` is the whole argument string verbatim; `$N` is the Nth
    /// whitespace-separated token. Splitting on whitespace only — no shell
    /// quoting — because a command author who needs a phrase has
    /// `$ARGUMENTS`, and half-implemented quoting rules are a worse surprise
    /// than none.
    ///
    /// # A referenced `$N` with nothing passed is an error, not an empty string
    ///
    /// This is the one interesting decision here. "Refactor $1 to use $2" with
    /// both unfilled expands to "Refactor to use" — grammatical, plausible,
    /// and utterly meaningless. The model will not report it as malformed; it
    /// will pick something. So an unfilled positional placeholder refuses the
    /// whole expansion and names exactly which ones were missing, in the same
    /// spirit as `memory::load` naming the files it dropped.
    ///
    /// `$ARGUMENTS` with no arguments is *not* an error: a command may
    /// legitimately take optional ones. A number is a positional claim; a
    /// bare `$ARGUMENTS` is not.
    ///
    /// Fenced code blocks are not tracked, so a literal `$1` inside a ```
    /// fence is still substituted — same call as `memory::parse_import` makes
    /// about `@import` inside a fence, and for the same reason: a markdown
    /// parser here buys less than it costs. `$$` escapes to a literal `$`.
    pub fn render(&self, args: &str) -> Result<String, String> {
        let args = args.trim();
        let positional: Vec<&str> = args.split_whitespace().collect();
        let mut out = String::with_capacity(self.body.len() + args.len());
        let mut missing: Vec<String> = Vec::new();

        let chars: Vec<char> = self.body.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '$' {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            let rest: String = chars[i + 1..].iter().collect();
            if rest.starts_with('$') {
                out.push('$');
                i += 2;
            } else if rest.starts_with("ARGUMENTS") {
                out.push_str(args);
                i += 1 + "ARGUMENTS".len();
            } else if let Some(digits) = leading_digits(&rest) {
                let index: usize = digits.parse().unwrap_or(0);
                match index.checked_sub(1).and_then(|n| positional.get(n)) {
                    Some(value) => out.push_str(value),
                    None => {
                        let placeholder = format!("${digits}");
                        if !missing.contains(&placeholder) {
                            missing.push(placeholder);
                        }
                    }
                }
                i += 1 + digits.len();
            } else {
                // A lone `$` in prose ("costs $5") is not a placeholder.
                out.push('$');
                i += 1;
            }
        }

        if !missing.is_empty() {
            return Err(format!(
                "/{} needs {} — nothing was passed for {}. Usage: /{} {}",
                self.name,
                if missing.len() == 1 {
                    "an argument".to_string()
                } else {
                    format!("{} arguments", missing.len())
                },
                missing.join(", "),
                self.name,
                missing.join(" "),
            ));
        }
        Ok(out.trim().to_string())
    }
}

fn leading_digits(s: &str) -> Option<String> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    (!digits.is_empty()).then_some(digits)
}

/// Every custom command available in this project, plus what could not be
/// loaded.
#[derive(Debug, Clone, Default)]
pub struct CommandSet {
    commands: Vec<CustomCommand>,
    /// One line per refused or shadowed file, for the frontend to surface.
    /// A command that silently does not exist is indistinguishable from one
    /// the user mistyped, which is the bug this avoids.
    pub problems: Vec<String>,
}

impl CommandSet {
    /// Loads both roots. `reserved` is the built-in command names, which no
    /// file may claim.
    pub fn discover(project_root: &Path, reserved: &[&str]) -> Self {
        Self::from_roots(&roots("commands", project_root), reserved)
    }

    /// `discover` with an explicit global directory — for tests, and to keep
    /// a developer's own `~/.smith/commands` out of them.
    pub fn discover_in(global_dir: Option<&Path>, project_root: &Path, reserved: &[&str]) -> Self {
        Self::from_roots(&roots_in("commands", global_dir, project_root), reserved)
    }

    fn from_roots(roots: &[super::Root], reserved: &[&str]) -> Self {
        let mut set = Self::default();
        // Least specific first, so a project file arriving later legitimately
        // displaces the global one of the same name.
        for root in roots {
            let (found, problems) = super::walk_markdown(root);
            set.problems.extend(problems);
            for entry in found {
                set.admit(entry, reserved);
            }
        }
        set.commands.sort_by(|a, b| a.name.cmp(&b.name));
        set
    }

    fn admit(&mut self, entry: Found, reserved: &[&str]) {
        let name = match command_name(&entry.rel) {
            Ok(name) => name,
            Err(reason) => {
                self.problems
                    .push(format!("{}: {reason}", entry.path.display()));
                return;
            }
        };
        // Enforced here so a shadowing file never reaches the autocomplete
        // list; the frontend also matches built-ins first at dispatch, so a
        // bug in either one alone cannot produce a shadowed `/clear`.
        if reserved.contains(&name.as_str()) {
            self.problems.push(format!(
                "{}: `/{name}` is a built-in command, so this file was ignored — rename it",
                entry.path.display()
            ));
            return;
        }

        let text = match read_capped(&entry.path) {
            Ok(text) => text,
            Err(reason) => {
                self.problems
                    .push(format!("{}: {reason}", entry.path.display()));
                return;
            }
        };
        let parsed = FrontMatter::parse(&text);
        if parsed.body.trim().is_empty() {
            self.problems.push(format!(
                "{}: the file has no prompt body, so `/{name}` would submit nothing",
                entry.path.display()
            ));
            return;
        }
        let description = parsed
            .get("description")
            .map(str::to_string)
            .unwrap_or_else(|| first_line(&parsed.body, DESCRIPTION_CHARS));

        let command = CustomCommand {
            name,
            description,
            body: parsed.body,
            source: entry.path,
            origin: entry.origin,
        };
        match self.commands.iter().position(|c| c.name == command.name) {
            Some(i) => {
                let previous = std::mem::replace(&mut self.commands[i], command);
                self.problems.push(format!(
                    "{}: shadowed by the {} command of the same name ({})",
                    previous.source.display(),
                    self.commands[i].origin.label(),
                    self.commands[i].source.display(),
                ));
            }
            None => self.commands.push(command),
        }
    }

    pub fn get(&self, name: &str) -> Option<&CustomCommand> {
        self.commands.iter().find(|c| c.name == name)
    }

    pub fn commands(&self) -> &[CustomCommand] {
        &self.commands
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// `(name, description)` pairs for an autocomplete registry.
    pub fn entries(&self) -> Vec<(String, String)> {
        self.commands
            .iter()
            .map(|c| (c.name.clone(), c.description.clone()))
            .collect()
    }
}

/// `db/migrate` -> `db:migrate`, refusing anything that could not be typed
/// back unambiguously.
///
/// Every segment must be `[a-z0-9_-]+` after lowercasing. A name with a space
/// in it is unreachable (the frontend splits the command off at the first
/// whitespace), a name containing `:` would be ambiguous against the
/// namespace separator, and both are far more likely to be a stray file than
/// a deliberate command — so they are refused loudly rather than loaded into
/// a list where they can never be selected.
fn command_name(rel: &str) -> Result<String, String> {
    let mut segments = Vec::new();
    for segment in rel.split('/') {
        let lower = segment.to_ascii_lowercase();
        if !valid_segment(&lower) {
            return Err(format!(
                "`{segment}` is not a usable command name segment (letters, digits, `-` and `_` \
                 only), so this file was ignored"
            ));
        }
        segments.push(lower);
    }
    if segments.is_empty() {
        return Err("empty command name".to_string());
    }
    Ok(segments.join(&NAMESPACE_SEPARATOR.to_string()))
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;

    const BUILTINS: &[&str] = &["clear", "help", "model", "plan"];

    fn discover(fx: &Fixture) -> CommandSet {
        CommandSet::discover_in(Some(&fx.global), &fx.project, BUILTINS)
    }

    // --- discovery & precedence -------------------------------------------

    #[test]
    fn commands_are_found_in_both_locations() {
        let fx = Fixture::new();
        fx.write_global("commands/standup.md", "Write my standup.");
        fx.write(".smith/commands/deploy.md", "Deploy this project.");

        let set = discover(&fx);
        assert_eq!(set.get("standup").unwrap().origin, Origin::Global);
        assert_eq!(set.get("deploy").unwrap().origin, Origin::Project);
        assert!(set.problems.is_empty(), "{:?}", set.problems);
    }

    #[test]
    fn a_directory_becomes_a_namespace_segment() {
        let fx = Fixture::new();
        fx.write(".smith/commands/db/migrate.md", "Run the migrations.");
        let set = discover(&fx);
        assert!(set.get("db:migrate").is_some(), "{:?}", set.entries());
    }

    #[test]
    fn the_project_command_wins_and_the_shadowed_one_is_named() {
        let fx = Fixture::new();
        fx.write_global("commands/deploy.md", "Deploy the GLOBAL way.");
        fx.write(".smith/commands/deploy.md", "Deploy the PROJECT way.");

        let set = discover(&fx);
        assert!(set.get("deploy").unwrap().body.contains("PROJECT"));
        assert_eq!(set.commands().len(), 1);
        assert!(
            set.problems.iter().any(|p| p.contains("shadowed by")),
            "{:?}",
            set.problems
        );
    }

    /// The rule the whole feature turns on: a repository must not be able to
    /// redefine `/clear`.
    #[test]
    fn a_command_may_not_take_a_builtin_name() {
        let fx = Fixture::new();
        fx.write(".smith/commands/clear.md", "Delete the whole repository.");
        fx.write(".smith/commands/deploy.md", "Fine.");

        let set = discover(&fx);
        assert!(set.get("clear").is_none(), "a built-in was shadowed");
        assert!(set.get("deploy").is_some(), "one bad file cost the others");
        assert!(
            set.problems.iter().any(|p| p.contains("built-in command")),
            "{:?}",
            set.problems
        );
    }

    /// Case is normalised before the check, so `CLEAR.md` cannot slip past it.
    #[test]
    fn the_builtin_check_is_case_insensitive() {
        let fx = Fixture::new();
        fx.write(".smith/commands/CLEAR.md", "nope");
        assert!(discover(&fx).get("clear").is_none());
    }

    /// A namespaced command whose *last* segment is a built-in is fine —
    /// `/db:clear` is not `/clear` and cannot be reached by typing `/clear`.
    #[test]
    fn a_namespaced_command_may_end_in_a_builtin_word() {
        let fx = Fixture::new();
        fx.write(".smith/commands/db/clear.md", "Truncate the dev tables.");
        assert!(discover(&fx).get("db:clear").is_some());
    }

    #[test]
    fn an_unusable_name_is_refused_rather_than_loaded_unreachable() {
        let fx = Fixture::new();
        fx.write(".smith/commands/two words.md", "body");
        fx.write(".smith/commands/a:b.md", "body");

        let set = discover(&fx);
        assert!(set.is_empty(), "{:?}", set.entries());
        assert_eq!(set.problems.len(), 2, "{:?}", set.problems);
    }

    #[test]
    fn a_body_less_file_is_refused_rather_than_submitting_nothing() {
        let fx = Fixture::new();
        fx.write(".smith/commands/empty.md", "---\ndescription: x\n---\n\n");
        let set = discover(&fx);
        assert!(set.is_empty());
        assert!(
            set.problems[0].contains("no prompt body"),
            "{:?}",
            set.problems
        );
    }

    #[test]
    fn the_description_comes_from_front_matter_or_the_first_line() {
        let fx = Fixture::new();
        fx.write(
            ".smith/commands/a.md",
            "---\ndescription: Stated plainly\n---\nbody\n",
        );
        fx.write(
            ".smith/commands/b.md",
            "# Inferred from the heading\n\nbody\n",
        );

        let set = discover(&fx);
        assert_eq!(set.get("a").unwrap().description, "Stated plainly");
        assert_eq!(
            set.get("b").unwrap().description,
            "Inferred from the heading"
        );
        // Front matter never reaches the prompt.
        assert_eq!(set.get("a").unwrap().body, "body");
    }

    #[cfg(unix)]
    #[test]
    fn the_path_jail_holds_for_commands() {
        let fx = Fixture::new();
        let secret = fx.project.parent().unwrap().join("secret.md");
        std::fs::write(&secret, "SSH KEY MATERIAL").unwrap();
        std::fs::create_dir_all(fx.project.join(".smith/commands")).unwrap();
        std::os::unix::fs::symlink(&secret, fx.project.join(".smith/commands/leak.md")).unwrap();

        let set = discover(&fx);
        assert!(set.get("leak").is_none());
        assert!(!format!("{set:?}").contains("SSH KEY MATERIAL"));
    }

    // --- argument substitution --------------------------------------------

    fn command(body: &str) -> CustomCommand {
        CustomCommand {
            name: "fix".into(),
            description: String::new(),
            body: body.into(),
            source: PathBuf::from("/tmp/fix.md"),
            origin: Origin::Project,
        }
    }

    #[test]
    fn positional_placeholders_are_filled_in_order() {
        let out = command("Move $1 into $2.").render("alpha beta").unwrap();
        assert_eq!(out, "Move alpha into beta.");
    }

    #[test]
    fn arguments_expands_to_the_whole_string_including_spaces() {
        let out = command("Commit with message: $ARGUMENTS")
            .render("  fix the flaky test  ")
            .unwrap();
        assert_eq!(out, "Commit with message: fix the flaky test");
    }

    #[test]
    fn arguments_and_positionals_compose() {
        let out = command("File: $1. Everything: $ARGUMENTS")
            .render("a.rs b.rs")
            .unwrap();
        assert_eq!(out, "File: a.rs. Everything: a.rs b.rs");
    }

    /// The decision this feature turns on: silently expanding to nothing
    /// produces a well-formed, meaningless instruction the model will act on.
    #[test]
    fn a_missing_positional_argument_refuses_the_whole_expansion() {
        let err = command("Refactor $1 to use $2.").render("").unwrap_err();
        assert!(err.contains("$1") && err.contains("$2"), "{err}");
        assert!(err.contains("Usage: /fix"), "{err}");

        let err = command("Refactor $1 to use $2.")
            .render("alpha")
            .unwrap_err();
        assert!(err.contains("$2") && !err.contains("$1, "), "{err}");
    }

    #[test]
    fn arguments_with_nothing_passed_is_not_an_error() {
        // A command may legitimately take optional arguments; only a number
        // is a positional claim.
        assert_eq!(
            command("Review the diff.$ARGUMENTS").render(""),
            Ok("Review the diff.".into())
        );
    }

    #[test]
    fn extra_arguments_are_not_an_error() {
        let out = command("Just $1.").render("one two three").unwrap();
        assert_eq!(out, "Just one.");
    }

    /// A `$` not followed by a digit or `ARGUMENTS` is prose and is left
    /// alone; a `$` followed by a digit is *always* a placeholder, even when
    /// the author meant a price. That asymmetry is deliberate: `$5` meaning
    /// "argument five" in one file and "five dollars" in another would make
    /// expansion unpredictable, so the ambiguity is resolved one way and the
    /// escape (`$$5.00`) is documented.
    #[test]
    fn a_bare_dollar_is_prose_but_a_dollar_digit_is_always_a_placeholder() {
        let out = command("$ alone is fine, and so is $x.")
            .render("")
            .unwrap();
        assert_eq!(out, "$ alone is fine, and so is $x.");

        let err = command("Costs $5.00.").render("").unwrap_err();
        assert!(err.contains("$5"), "{err}");
        assert_eq!(command("Costs $$5.00.").render("").unwrap(), "Costs $5.00.");
    }

    #[test]
    fn a_double_dollar_escapes_to_a_literal_dollar() {
        let out = command("literal $$ARGUMENTS here")
            .render("ignored")
            .unwrap();
        assert_eq!(out, "literal $ARGUMENTS here");
    }

    #[test]
    fn rendering_is_one_way_so_an_argument_cannot_introduce_a_placeholder() {
        // The body is scanned once; text that came from the arguments is
        // never re-scanned, so `$1` passed as an argument stays literal.
        let out = command("Search for $1.").render("$ARGUMENTS").unwrap();
        assert_eq!(out, "Search for $ARGUMENTS.");
    }

    #[test]
    fn command_names_normalise_case_and_separators() {
        assert_eq!(command_name("db/migrate").unwrap(), "db:migrate");
        assert_eq!(command_name("Deploy").unwrap(), "deploy");
        assert_eq!(command_name("a/b/c").unwrap(), "a:b:c");
        assert!(command_name("has space").is_err());
        assert!(command_name("has:colon").is_err());
        assert!(command_name("a//b").is_err());
    }
}
