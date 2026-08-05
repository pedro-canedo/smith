//! Content search, built on ripgrep's own libraries rather than on `rg`.
//!
//! Before this existed the only way to search file *contents* was `run_bash`
//! with `grep`/`rg` — `PermissionClass::Dangerous`, so the cheapest and safest
//! operation in the whole agent was also the one that interrupted the user
//! most. `grep` is `ReadOnly` and never prompts.
//!
//! Using `ignore` + `grep-regex` + `grep-searcher` in-process rather than
//! shelling out buys three things: no dependency on a binary the user may not
//! have installed, `.gitignore`/hidden-file handling that matches what a
//! developer expects without reimplementing it, and — the one that matters
//! most for an LLM — full control over how much output comes back.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use globset::GlobSet;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkBuilder;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

use crate::fs_tools::{build_globset, clip_line, jail_root, path_is_inside, relative_to, resolve};

/// Matching lines carried back in `content` mode.
///
/// Search results are the single easiest way to burn a context window: one
/// unlucky pattern over a vendored directory is tens of thousands of lines.
/// Every cap below is paired with a *visible* marker, because a silently
/// truncated search reads as "there are only 3 matches" — a wrong answer, not
/// a shortened one.
const MAX_MATCH_LINES: usize = 200;
/// Files named in `files_with_matches`/`count` mode.
const MAX_FILES: usize = 200;
/// Per line. Minified JS and lockfiles routinely have single lines longer than
/// a whole source file.
const MAX_LINE_CHARS: usize = 400;
/// Overall ceiling, enforced after the per-line and per-match ones because
/// 200 x 400 chars is still more than anyone wants in a transcript.
const MAX_TOTAL_CHARS: usize = 30_000;
/// Largest `-A`/`-B`/`-C` we honour: context is a multiplier on output size.
const MAX_CONTEXT: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Content,
    FilesWithMatches,
    Count,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "content" => Some(Mode::Content),
            "files_with_matches" | "files" => Some(Mode::FilesWithMatches),
            "count" => Some(Mode::Count),
            _ => None,
        }
    }
}

/// Everything the blocking search needs, owned — the walk and the searcher are
/// synchronous, so they run on `spawn_blocking` and cannot borrow `ToolContext`.
struct SearchOpts {
    /// Jail root; every emitted path is displayed relative to this.
    root: PathBuf,
    /// Where the walk starts. A file searches just that file.
    scope: PathBuf,
    pattern: String,
    literal: bool,
    case_insensitive: bool,
    mode: Mode,
    globs: Option<GlobSet>,
    types: Option<ignore::types::Types>,
    include_hidden: bool,
    before: usize,
    after: usize,
}

#[derive(Default)]
struct Report {
    /// Formatted output lines, already capped.
    lines: Vec<String>,
    /// Total matches found, *not* capped — this is what makes a truncated
    /// result honest ("200 of 4213") instead of misleading ("200").
    total_matches: u64,
    /// Files with at least one match, not capped.
    total_files: usize,
    /// Files listed/quoted in `lines`.
    shown_files: usize,
    /// Matching lines quoted in `lines`.
    shown_matches: usize,
    /// Files skipped because they turned out to be binary.
    binary_skipped: usize,
    /// At least one quoted line was cut mid-line.
    clipped_line: bool,
    cancelled: bool,
}

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regular expression (ripgrep engine; respects .gitignore and skips hidden files and binaries). \
Args: pattern (required, Rust regex — set literal=true to search it as plain text), path (file or directory to search, default the project root), \
glob (filter: a pattern without `/` matches a file name at any depth like `*.rs`, one with `/` is anchored to the project root like `src/**/*.rs`), \
type (ripgrep file type, e.g. \"rust\", \"js\", \"py\"), mode (\"content\" default, \"files_with_matches\", or \"count\"), \
case_insensitive, include_hidden, and before_context / after_context / context (-B/-A/-C, max 20). \
Output is capped and any truncation is stated in the last line — re-run narrower rather than assuming you saw everything."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "glob": {"type": "string"},
                "type": {"type": "string"},
                "mode": {"type": "string", "enum": ["content", "files_with_matches", "count"]},
                "literal": {"type": "boolean"},
                "case_insensitive": {"type": "boolean"},
                "include_hidden": {"type": "boolean"},
                "before_context": {"type": "integer", "minimum": 0},
                "after_context": {"type": "integer", "minimum": 0},
                "context": {"type": "integer", "minimum": 0}
            },
            "required": ["pattern"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        let opts = match build_opts(&input, ctx) {
            Ok(o) => o,
            Err(e) => return ToolResult::error(e),
        };
        let mode = opts.mode;

        // The walk and the searcher are blocking and can run for a while on a
        // big tree; keeping them off the async runtime is what lets the
        // cancellation token actually be observed.
        let report =
            match tokio::task::spawn_blocking(move || run_search(opts, &cancel.clone())).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return ToolResult::error(e),
                Err(e) => return ToolResult::error(format!("search task failed: {e}")),
            };

        ToolResult::ok(format_report(&report, mode))
    }
}

fn build_opts(input: &serde_json::Value, ctx: &ToolContext) -> Result<SearchOpts, String> {
    let pattern = input
        .get("pattern")
        .and_then(|v| v.as_str())
        .ok_or("missing required field: pattern")?
        .to_string();
    if pattern.is_empty() {
        return Err("pattern must not be empty".to_string());
    }

    let root = jail_root(ctx);
    let scope = resolve(
        ctx,
        input.get("path").and_then(|v| v.as_str()).unwrap_or("."),
    )?;
    if !scope.exists() {
        return Err(format!("{} does not exist", relative_to(&root, &scope)));
    }

    let mode = match input.get("mode").and_then(|v| v.as_str()) {
        None => Mode::Content,
        Some(m) => Mode::parse(m)
            .ok_or_else(|| format!("unknown mode '{m}' (content, files_with_matches, count)"))?,
    };

    let globs = match input.get("glob").and_then(|v| v.as_str()) {
        Some(g) if !g.is_empty() => Some(build_globset(g)?),
        _ => None,
    };

    let types = match input.get("type").and_then(|v| v.as_str()) {
        Some(t) if !t.is_empty() => {
            let mut builder = ignore::types::TypesBuilder::new();
            builder.add_defaults();
            builder.select(t);
            Some(
                builder
                    .build()
                    .map_err(|e| format!("unknown file type '{t}': {e}"))?,
            )
        }
        _ => None,
    };

    let num = |key: &str| -> usize {
        input
            .get(key)
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(MAX_CONTEXT as u64) as usize
    };
    let both = num("context");

    Ok(SearchOpts {
        root,
        scope,
        pattern,
        literal: input
            .get("literal")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        case_insensitive: input
            .get("case_insensitive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        mode,
        globs,
        types,
        include_hidden: input
            .get("include_hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        before: num("before_context").max(both),
        after: num("after_context").max(both),
    })
}

/// A walker configured the way every read-only tool in this crate wants it.
///
/// `follow_links(false)` is load-bearing rather than a performance choice: a
/// symlink inside the project pointing out of it is exactly the escape the
/// path jail exists to close, and not descending it means the walk can never
/// produce a path outside the root in the first place.
///
/// `require_git(false)` because `.gitignore` is the user's statement of what is
/// noise, and it is just as true in a tarball, a worktree or a test fixture as
/// it is in a checkout — the default of only honouring it inside a repo makes
/// the tool behave differently for reasons the model cannot see.
pub(crate) fn walk_builder(root: &Path, include_hidden: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!include_hidden)
        .follow_links(false)
        .require_git(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Directory order is arbitrary; two identical searches returning
        // results in different orders would look like the tree changed.
        .sort_by_file_path(|a, b| a.cmp(b));
    builder
}

fn run_search(opts: SearchOpts, cancel: &CancellationToken) -> Result<Report, String> {
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .fixed_strings(opts.literal)
        .line_terminator(Some(b'\n'))
        .build(&opts.pattern)
        .map_err(|e| format!("invalid regex '{}': {e}", opts.pattern))?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        // Stop reading a file at the first NUL. Without this a match near the
        // top of a `.png` would splice raw bytes into the transcript.
        .binary_detection(BinaryDetection::quit(0))
        .before_context(if opts.mode == Mode::Content {
            opts.before
        } else {
            0
        })
        .after_context(if opts.mode == Mode::Content {
            opts.after
        } else {
            0
        })
        .build();

    let mut walk = walk_builder(&opts.scope, opts.include_hidden);
    if let Some(types) = opts.types {
        walk.types(types);
    }

    let mut report = Report::default();
    let mut last_line: Option<u64> = None;
    let mut last_file: Option<String> = None;

    for entry in walk.build() {
        if cancel.is_cancelled() {
            report.cancelled = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        // Skips directories and, because links are not followed, every
        // symlink — including one aimed outside the project.
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        // Belt and braces with `follow_links(false)`: the walk root itself is
        // resolved by `resolve`, but this is the same re-check `glob` does, and
        // it costs one `stat` per matching file.
        if !path_is_inside(path, &opts.root) {
            continue;
        }
        let display = relative_to(&opts.root, path);
        if let Some(globs) = &opts.globs {
            if !globs.is_match(&display) {
                continue;
            }
        }

        let mut sink = FileSink {
            cancel,
            mode: opts.mode,
            room: MAX_MATCH_LINES.saturating_sub(report.shown_matches),
            lines: Vec::new(),
            matches: 0,
            binary: false,
            clipped_line: false,
        };
        if searcher.search_path(&matcher, path, &mut sink).is_err() {
            // An unreadable file is not worth failing the whole search over.
            continue;
        }

        if sink.binary {
            // Deliberately dropped whole rather than reported as a hit: the
            // model can do nothing useful with "matched inside a .so".
            report.binary_skipped += 1;
            continue;
        }
        if sink.matches == 0 {
            continue;
        }

        report.total_matches += sink.matches;
        report.total_files += 1;
        report.clipped_line |= sink.clipped_line;

        match opts.mode {
            Mode::FilesWithMatches => {
                if report.shown_files < MAX_FILES {
                    report.lines.push(display);
                    report.shown_files += 1;
                }
            }
            Mode::Count => {
                if report.shown_files < MAX_FILES {
                    report.lines.push(format!("{display}:{}", sink.matches));
                    report.shown_files += 1;
                }
            }
            Mode::Content => {
                if sink.lines.is_empty() {
                    continue;
                }
                report.shown_files += 1;
                for line in sink.lines {
                    // ripgrep's `--` separator: without it two runs of context
                    // read as one contiguous block of the file.
                    let contiguous = last_file.as_deref() == Some(display.as_str())
                        && last_line.is_some_and(|n| n + 1 == line.number);
                    if !contiguous && last_file.is_some() {
                        report.lines.push("--".to_string());
                    }
                    let sep = if line.is_match { ':' } else { '-' };
                    report
                        .lines
                        .push(format!("{display}{sep}{}{sep}{}", line.number, line.text));
                    if line.is_match {
                        report.shown_matches += 1;
                    }
                    last_line = Some(line.number);
                    last_file = Some(display.clone());
                }
            }
        }
    }

    Ok(report)
}

struct MatchLine {
    number: u64,
    text: String,
    is_match: bool,
}

/// Collects one file's hits. Counting continues past `room` so the summary can
/// report the true total rather than the truncated one.
struct FileSink<'a> {
    cancel: &'a CancellationToken,
    mode: Mode,
    room: usize,
    lines: Vec<MatchLine>,
    matches: u64,
    binary: bool,
    clipped_line: bool,
}

impl FileSink<'_> {
    fn push(&mut self, number: u64, raw: &[u8], is_match: bool) {
        let text = String::from_utf8_lossy(raw);
        let text = text.trim_end_matches(['\n', '\r']);
        let (text, clipped) = clip_line(text, MAX_LINE_CHARS);
        self.clipped_line |= clipped;
        self.lines.push(MatchLine {
            number,
            text,
            is_match,
        });
    }

    /// Whether this file may still contribute quoted lines. Context lines get
    /// the same budget as matches so a capped result can't end mid-group.
    fn has_room(&self) -> bool {
        self.mode == Mode::Content && self.lines.len() < self.room.saturating_mul(2).max(2)
    }
}

impl Sink for FileSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.cancel.is_cancelled() {
            return Ok(false);
        }
        self.matches += 1;
        if self.mode == Mode::FilesWithMatches {
            // The file's name is the whole answer; reading the rest of it is
            // wasted work.
            return Ok(false);
        }
        if self.has_room() && self.room > 0 {
            let first = m.line_number().unwrap_or(0);
            for (offset, line) in m.lines().enumerate() {
                self.push(first + offset as u64, line, true);
            }
        }
        Ok(true)
    }

    fn context(&mut self, _searcher: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if matches!(c.kind(), SinkContextKind::Before | SinkContextKind::After)
            && self.has_room()
            && self.room > 0
        {
            self.push(c.line_number().unwrap_or(0), c.bytes(), false);
        }
        Ok(true)
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

fn format_report(report: &Report, mode: Mode) -> String {
    if report.total_matches == 0 {
        let mut out = "(no matches)".to_string();
        if report.binary_skipped > 0 {
            out.push_str(&format!(
                "\n[{} binary file(s) skipped]",
                report.binary_skipped
            ));
        }
        return out;
    }

    let mut out = String::new();
    let mut budget_hit = false;
    for line in &report.lines {
        if out.chars().count() + line.chars().count() + 1 > MAX_TOTAL_CHARS {
            budget_hit = true;
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }

    let mut notes = Vec::new();
    match mode {
        Mode::Content => {
            let complete = !budget_hit && report.shown_matches as u64 == report.total_matches;
            if complete {
                notes.push(format!(
                    "{} match(es) in {} file(s)",
                    report.total_matches, report.total_files
                ));
            } else {
                notes.push(format!(
                    "TRUNCATED: showing {} of {} match(es) across {} file(s) — \
                     narrow `path`, add a `glob`/`type` filter, make the pattern more \
                     specific, or use mode=\"files_with_matches\"",
                    report.shown_matches, report.total_matches, report.total_files
                ));
            }
        }
        Mode::FilesWithMatches | Mode::Count => {
            if budget_hit || report.shown_files < report.total_files {
                notes.push(format!(
                    "TRUNCATED: showing {} of {} file(s) with matches — narrow `path` or \
                     add a `glob`/`type` filter",
                    report.shown_files, report.total_files
                ));
            } else {
                notes.push(format!(
                    "{} file(s) with matches, {} match(es) total",
                    report.total_files, report.total_matches
                ));
            }
        }
    }
    if report.clipped_line {
        notes.push("some lines were clipped to 400 chars".to_string());
    }
    if report.binary_skipped > 0 {
        notes.push(format!("{} binary file(s) skipped", report.binary_skipped));
    }
    if report.cancelled {
        notes.push("search cancelled before it finished".to_string());
    }

    out.push_str("\n[");
    out.push_str(&notes.join("; "));
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use smith_core::ToolContext;

    fn ctx(dir: &tempfile::TempDir) -> ToolContext {
        ToolContext::new(dir.path(), "test-session")
    }

    async fn grep(ctx: &ToolContext, input: serde_json::Value) -> ToolResult {
        GrepTool.execute(input, ctx, CancellationToken::new()).await
    }

    fn write(dir: &tempfile::TempDir, rel: &str, content: &str) {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn finds_a_regex_match_with_path_and_line_number() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/main.rs", "fn main() {}\nfn helper() {}\n");

        let result = grep(&ctx(&dir), serde_json::json!({"pattern": r"fn \w+"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("src/main.rs:1:fn main() {}"),
            "{}",
            result.content
        );
        assert!(result.content.contains("2 match(es) in 1 file(s)"));
    }

    #[tokio::test]
    async fn literal_mode_searches_regex_metacharacters_as_text() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "cost is a[0] dollars\n");

        let broken = grep(&ctx(&dir), serde_json::json!({"pattern": "a[0]"})).await;
        assert!(!broken.content.contains("cost is"), "{}", broken.content);

        let literal = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "a[0]", "literal": true}),
        )
        .await;
        assert!(literal.content.contains("cost is"), "{}", literal.content);
    }

    #[tokio::test]
    async fn context_lines_are_returned_and_marked() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "one\ntwo\nTARGET\nfour\nfive\n");

        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "TARGET", "context": 1}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        // Context uses `-` as separator, matches use `:` — same convention as
        // ripgrep, so the model can tell which line actually matched.
        assert!(result.content.contains("a.txt-2-two"), "{}", result.content);
        assert!(
            result.content.contains("a.txt:3:TARGET"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("a.txt-4-four"),
            "{}",
            result.content
        );
        assert!(!result.content.contains("one"), "{}", result.content);
    }

    #[tokio::test]
    async fn before_and_after_context_can_differ() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "one\ntwo\nTARGET\nfour\nfive\n");

        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "TARGET", "before_context": 2, "after_context": 0}),
        )
        .await;
        assert!(result.content.contains("a.txt-1-one"), "{}", result.content);
        assert!(result.content.contains("a.txt-2-two"), "{}", result.content);
        assert!(
            !result.content.contains("four"),
            "after_context=0 still emitted a trailing line: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_gitignored_file_is_not_searched() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, ".gitignore", "target/\nsecret.txt\n");
        write(&dir, "kept.txt", "needle\n");
        write(&dir, "secret.txt", "needle\n");
        write(&dir, "target/build.txt", "needle\n");

        let result = grep(&ctx(&dir), serde_json::json!({"pattern": "needle"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("kept.txt"), "{}", result.content);
        assert!(
            !result.content.contains("secret.txt"),
            "gitignored file was searched: {}",
            result.content
        );
        assert!(
            !result.content.contains("target/"),
            "gitignored directory was searched: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_binary_file_is_counted_but_never_dumped() {
        let dir = tempfile::tempdir().unwrap();
        // Text first, then a NUL: proves the guard is not just "the first byte
        // looked binary" — the match is found before the detector fires.
        std::fs::write(
            dir.path().join("blob.bin"),
            b"needle\x00\xff\xfe binary junk needle\n",
        )
        .unwrap();
        write(&dir, "plain.txt", "needle\n");

        let result = grep(&ctx(&dir), serde_json::json!({"pattern": "needle"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("plain.txt"), "{}", result.content);
        assert!(
            !result.content.contains("blob.bin"),
            "binary file leaked into the transcript: {}",
            result.content
        );
        assert!(
            result.content.contains("binary file(s) skipped"),
            "the skip was silent: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn truncation_reports_the_true_total_not_the_shown_one() {
        let dir = tempfile::tempdir().unwrap();
        let body = "needle\n".repeat(MAX_MATCH_LINES * 3);
        write(&dir, "many.txt", &body);

        let result = grep(&ctx(&dir), serde_json::json!({"pattern": "needle"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("TRUNCATED"),
            "no truncation marker: {}",
            result.content
        );
        assert!(
            result
                .content
                .contains(&format!("of {} match", MAX_MATCH_LINES * 3)),
            "truncated output under-reported the match count: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn a_very_long_line_is_clipped_with_a_marker() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "min.js", &format!("needle{}\n", "x".repeat(5_000)));

        let result = grep(&ctx(&dir), serde_json::json!({"pattern": "needle"})).await;
        assert!(
            result.content.contains("line clipped"),
            "{}",
            result.content
        );
        assert!(
            result.content.chars().count() < 2_000,
            "clipped line was still huge: {} chars",
            result.content.chars().count()
        );
    }

    #[tokio::test]
    async fn files_with_matches_mode_lists_paths_only() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "needle needle\n");
        write(&dir, "b.txt", "needle\n");

        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "needle", "mode": "files_with_matches"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.starts_with("a.txt\nb.txt"),
            "{}",
            result.content
        );
        assert!(result.content.contains("2 file(s) with matches"));
    }

    #[tokio::test]
    async fn count_mode_reports_per_file_totals() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "needle\nneedle\n");
        write(&dir, "b.txt", "needle\n");

        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "needle", "mode": "count"}),
        )
        .await;
        assert!(result.content.contains("a.txt:2"), "{}", result.content);
        assert!(result.content.contains("b.txt:1"), "{}", result.content);
        assert!(result.content.contains("3 match(es) total"));
    }

    #[tokio::test]
    async fn glob_filter_narrows_by_extension_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "src/deep/a.rs", "needle\n");
        write(&dir, "notes.md", "needle\n");

        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "needle", "glob": "*.rs"}),
        )
        .await;
        assert!(
            result.content.contains("src/deep/a.rs"),
            "{}",
            result.content
        );
        assert!(!result.content.contains("notes.md"), "{}", result.content);
    }

    #[tokio::test]
    async fn type_filter_uses_ripgrep_type_names() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.rs", "needle\n");
        write(&dir, "b.py", "needle\n");

        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "needle", "type": "rust"}),
        )
        .await;
        assert!(result.content.contains("a.rs"), "{}", result.content);
        assert!(!result.content.contains("b.py"), "{}", result.content);
    }

    #[tokio::test]
    async fn case_insensitive_is_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "Needle\n");

        assert!(grep(&ctx(&dir), serde_json::json!({"pattern": "needle"}))
            .await
            .content
            .contains("(no matches)"));
        assert!(grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "needle", "case_insensitive": true})
        )
        .await
        .content
        .contains("Needle"));
    }

    #[tokio::test]
    async fn scoping_to_a_single_file_searches_only_that_file() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "a.txt", "needle\n");
        write(&dir, "b.txt", "needle\n");

        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "needle", "path": "a.txt"}),
        )
        .await;
        assert!(result.content.contains("a.txt"), "{}", result.content);
        assert!(!result.content.contains("b.txt"), "{}", result.content);
    }

    #[tokio::test]
    async fn an_invalid_regex_is_an_error_not_an_empty_result() {
        let dir = tempfile::tempdir().unwrap();
        let result = grep(&ctx(&dir), serde_json::json!({"pattern": "("})).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("invalid regex"),
            "{}",
            result.content
        );
    }

    // --- path jail -------------------------------------------------------

    #[tokio::test]
    async fn refuses_a_path_outside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "root", "path": "/etc"}),
        )
        .await;
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("outside the project directory"));
    }

    #[tokio::test]
    async fn refuses_a_parent_traversal_that_re_enters_the_root_textually() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let result = grep(
            &ctx(&dir),
            serde_json::json!({"pattern": "root", "path": "a/../../etc"}),
        )
        .await;
        assert!(result.is_error, "{}", result.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_read_through_a_symlink_out_of_the_project() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "classified needle").unwrap();

        let dir = tempfile::tempdir().unwrap();
        write(&dir, "mine.txt", "ordinary needle\n");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("direct.txt"),
        )
        .unwrap();

        let result = grep(&ctx(&dir), serde_json::json!({"pattern": "needle"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("ordinary"), "{}", result.content);
        assert!(
            !result.content.contains("classified"),
            "grep read through a symlink out of the project: {}",
            result.content
        );
    }
}
