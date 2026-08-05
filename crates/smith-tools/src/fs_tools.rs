use async_trait::async_trait;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use std::path::{Component, Path, PathBuf};

/// Default and hard ceiling on lines returned by one `read_file` call.
///
/// Reading a 40k-line generated file into the transcript costs more context
/// than the whole rest of the turn, and the model almost never needs all of
/// it. An explicit `limit` can ask for less but not for more — the cap is the
/// point of the cap.
const MAX_READ_LINES: usize = 2_000;
/// Per line, for both reading and globbing. Minified bundles and lockfiles
/// routinely have single lines longer than a whole hand-written source file.
const MAX_LINE_CHARS: usize = 2_000;
/// Overall ceiling on one `read_file` body, enforced on top of the line caps.
const MAX_READ_CHARS: usize = 80_000;
/// Files larger than this are refused outright rather than buffered.
const MAX_READ_BYTES: u64 = 20 * 1024 * 1024;
/// How much of a file is sniffed for NUL bytes before deciding it is binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Paths returned by one `glob` call.
const MAX_GLOB_RESULTS: usize = 500;

/// Resolves `path` for a file tool and refuses anything that escapes the
/// session directory.
///
/// The jail root is `ctx.cwd` — the directory smith was started in. Before
/// this existed, `read_file` happily returned `/etc/passwd` and `write_file`
/// would overwrite `../../.ssh/authorized_keys`; the staging layer looked
/// like a defence but only sanitised its own mirror before copying to the
/// unsanitised target.
///
/// Two escapes have to be closed, and they need different treatment:
///
/// - `..` is normalised away *lexically* first. `starts_with` is
///   component-wise, so `<root>/a/../../etc/passwd` would otherwise pass a
///   naive prefix check.
/// - Symlinks are resolved by canonicalising, so a link inside the project
///   pointing outside it isn't a side door. Canonicalising fails on paths
///   that don't exist yet (every `write_file` creating a new file), so we
///   canonicalise the deepest existing ancestor and re-append the rest.
pub(crate) fn resolve(ctx: &ToolContext, path: &str) -> Result<PathBuf, String> {
    let requested = Path::new(path);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        ctx.cwd.join(requested)
    };

    let root = jail_root(ctx);
    let resolved = real_path(&lexical_normalize(&candidate));

    if !resolved.starts_with(&root) {
        return Err(format!(
            "{path} is outside the project directory ({}). smith only reads and \
             writes below the directory it was started in.",
            root.display()
        ));
    }
    Ok(resolved)
}

/// The jail root, resolved exactly the way `resolve` resolves a candidate —
/// so a prefix comparison between the two is meaningful.
pub(crate) fn jail_root(ctx: &ToolContext) -> PathBuf {
    real_path(&lexical_normalize(&ctx.cwd))
}

/// Whether `path` lands inside `root` once `..` and symlinks are resolved.
///
/// The tools that enumerate files (`glob`, `grep`) need this on every result
/// and not just on their argument: a wildcard can expand *through* a symlink
/// that points out of the project.
pub(crate) fn path_is_inside(path: &Path, root: &Path) -> bool {
    real_path(&lexical_normalize(path)).starts_with(root)
}

/// `path` as the model should see it — relative to the project root, so the
/// string can be handed straight back to `read_file`/`edit_file`.
pub(crate) fn relative_to(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    #[allow(unused_mut)]
    let mut out = rel.to_string_lossy().into_owned();
    // Glob patterns and the model both speak `/`. A backslash is a legal
    // filename character on Unix, so this only applies where it cannot be one.
    #[cfg(windows)]
    {
        out = out.replace('\\', "/");
    }
    if out.is_empty() {
        ".".to_string()
    } else {
        out
    }
}

/// Drops `.` and resolves `..` textually, without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Popping past the root is a no-op, which is what we want:
                // `/../..` is `/`.
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalises as much of `path` as exists, keeping the rest verbatim.
/// Expects `path` to already be lexically normalised — re-appending a `..`
/// after canonicalising would reintroduce the escape this is meant to close.
fn real_path(path: &Path) -> PathBuf {
    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = path.to_path_buf();

    loop {
        if let Ok(real) = probe.canonicalize() {
            let mut out = real;
            for part in trailing.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (probe.file_name(), probe.parent()) {
            (Some(name), Some(parent)) => {
                trailing.push(name.to_os_string());
                probe = parent.to_path_buf();
            }
            // Nothing along the path exists (or we hit the filesystem root
            // and even that didn't canonicalise) — fall back to the lexical
            // form, which is still `..`-free.
            _ => return path.to_path_buf(),
        }
    }
}

/// Compiles one glob into a matcher with `.gitignore`/`rg -g` semantics: a
/// pattern with no `/` matches a file *name* at any depth (`*.rs`), one with a
/// `/` is anchored to the project root (`src/**/*.rs`).
///
/// That rule rather than the `glob` crate's (everything anchored) because it
/// is the one every developer already has from `.gitignore`, and `*.rs`
/// meaning "only the ones in the root directory" is a silent wrong answer for
/// a model that meant "the Rust files".
///
/// `literal_separator(true)` keeps `*` from crossing a `/`, so `src/*.rs` is
/// still one level and only `**` recurses.
pub(crate) fn build_globset(pattern: &str) -> Result<GlobSet, String> {
    let trimmed = pattern.trim_start_matches("./");
    let anchored = if trimmed.contains('/') || trimmed.starts_with("**") {
        trimmed.to_string()
    } else {
        format!("**/{trimmed}")
    };
    let glob = GlobBuilder::new(&anchored)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("invalid glob pattern '{pattern}': {e}"))?;
    GlobSetBuilder::new()
        .add(glob)
        .build()
        .map_err(|e| format!("invalid glob pattern '{pattern}': {e}"))
}

/// Keeps the first `max` characters of `text`, cut on a `char` boundary, and
/// says so in the text itself. Returns whether anything was dropped.
///
/// Indexing by `char` rather than byte offset for the same reason
/// `shell_tool::truncate_tail` does: a byte cut lands mid-character on
/// accented Latin, CJK or emoji and panics.
pub(crate) fn clip_line(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        None => (text.to_string(), false),
        Some((end, _)) => (
            format!(
                "{} … [line clipped, {} chars total]",
                &text[..end],
                text.chars().count()
            ),
            true,
        ),
    }
}

fn field_str<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

fn field_bool(input: &serde_json::Value, key: &str) -> Option<bool> {
    input.get(key).and_then(|v| v.as_bool())
}

/// The single file a `write_file`/`edit_file`/`multi_edit` call will change,
/// resolved exactly the way `execute` will resolve it.
///
/// Resolving here (rather than handing the raw string to the checkpointer)
/// keeps the snapshot keyed on the same absolute path the write lands on, so a
/// call spelled `./src/a.rs` and one spelled `src/a.rs` share one entry. A
/// path the jail refuses yields nothing, because that call is about to fail
/// without touching anything.
fn snapshot_target(input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
    field_str(input, "path")
        .and_then(|path| resolve(ctx, path).ok())
        .into_iter()
        .collect()
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file, optionally a line range. Output is line-numbered (`   12\\tcode`) so you can target edit_file precisely — the numbers are display only, never include them in old_str. \
Binary files are reported, not dumped. Long files and long lines are capped and the truncation is stated in the last line. \
Args: path (required), offset (1-based first line, optional), limit (max lines, optional, capped at 2000), line_numbers (optional, default true)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "minimum": 1},
                "line_numbers": {"type": "boolean"}
            },
            "required": ["path"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        let Some(path) = field_str(&input, "path") else {
            return ToolResult::error("missing required field: path");
        };
        let full = match resolve(ctx, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        let meta = match tokio::fs::metadata(&full).await {
            Ok(m) => m,
            Err(e) => return ToolResult::error(format!("failed to read {path}: {e}")),
        };
        if meta.is_dir() {
            return ToolResult::error(format!("{path} is a directory — use list_dir or glob"));
        }
        if meta.len() > MAX_READ_BYTES {
            return ToolResult::error(format!(
                "{path} is {} bytes, over the {MAX_READ_BYTES}-byte read limit — \
                 use grep to search it",
                meta.len()
            ));
        }

        let bytes = match tokio::fs::read(&full).await {
            Ok(b) => b,
            Err(e) => return ToolResult::error(format!("failed to read {path}: {e}")),
        };
        // A NUL byte is the same signal ripgrep uses. Answering with a
        // description rather than an error or a wall of bytes: "it is a 4 MiB
        // binary" is the true answer to "read this file", and both of the
        // alternatives (a UTF-8 error, or the bytes themselves) leave the
        // model worse off.
        if bytes.iter().take(BINARY_SNIFF_BYTES).any(|&byte| byte == 0) {
            return ToolResult::ok(format!(
                "{path} is a binary file ({} bytes) — not shown",
                meta.len()
            ));
        }

        let content = String::from_utf8_lossy(&bytes);
        let lossy = matches!(content, std::borrow::Cow::Owned(_));
        let numbered = field_bool(&input, "line_numbers").unwrap_or(true);

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return ToolResult::ok(format!("({path} is empty)"));
        }

        let offset = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let window = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(MAX_READ_LINES)
            .min(MAX_READ_LINES);

        let start = offset.saturating_sub(1);
        if start >= total {
            return ToolResult::error(format!(
                "offset {offset} is past the end of {path}, which has {total} line(s)"
            ));
        }
        let end = start.saturating_add(window).min(total);

        let mut out = String::new();
        let mut used = 0usize;
        let mut emitted = 0usize;
        let mut budget_hit = false;
        let mut clipped = false;
        for (index, line) in lines[start..end].iter().enumerate() {
            let (text, cut) = clip_line(line, MAX_LINE_CHARS);
            clipped |= cut;
            let rendered = if numbered {
                format!("{:>6}\t{text}", start + index + 1)
            } else {
                text
            };
            let cost = rendered.chars().count() + 1;
            if used + cost > MAX_READ_CHARS {
                budget_hit = true;
                break;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&rendered);
            used += cost;
            emitted += 1;
        }

        let shown_to = start + emitted;
        let mut notes: Vec<String> = Vec::new();
        if lossy {
            notes.push("file is not valid UTF-8; undecodable bytes shown as U+FFFD".to_string());
        }
        if clipped {
            notes.push(format!("some lines clipped to {MAX_LINE_CHARS} chars"));
        }
        if shown_to < total {
            notes.push(format!(
                "TRUNCATED: showing lines {}-{shown_to} of {total}{} — call read_file again \
                 with offset={}",
                start + 1,
                if budget_hit {
                    " (output cap reached)"
                } else {
                    ""
                },
                shown_to + 1
            ));
        }
        if !notes.is_empty() {
            out.push_str(&format!("\n[{}]", notes.join("; ")));
        }
        ToolResult::ok(out)
    }
}

pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List entries in a directory (non-recursive). Args: path (required)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "path": {"type": "string"} },
            "required": ["path"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        let Some(path) = field_str(&input, "path") else {
            return ToolResult::error("missing required field: path");
        };
        let full = match resolve(ctx, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        let mut read_dir = match tokio::fs::read_dir(&full).await {
            Ok(rd) => rd,
            Err(e) => return ToolResult::error(format!("failed to list {path}: {e}")),
        };

        let mut entries = Vec::new();
        loop {
            match read_dir.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    entries.push(if is_dir { format!("{name}/") } else { name });
                }
                Ok(None) => break,
                Err(e) => return ToolResult::error(format!("failed to list {path}: {e}")),
            }
        }
        entries.sort();
        ToolResult::ok(entries.join("\n"))
    }
}

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by name, most-recently-modified first. Respects .gitignore and skips hidden files, \
so target/, .git/ and node_modules/ never show up. A pattern without `/` matches a file name at any depth (`*.rs`); \
one with a `/` is anchored to the project root (`src/**/*.rs`). Paths come back relative to the project root. \
Args: pattern (required), include_hidden (optional, implied when the pattern names a dot-file)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "include_hidden": {"type": "boolean"}
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
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        let Some(pattern) = field_str(&input, "pattern") else {
            return ToolResult::error("missing required field: pattern");
        };
        // Jail the *pattern* first: `../../*.rs` has to be refused outright,
        // not filtered out result by result.
        let resolved = match resolve(ctx, pattern) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let root = jail_root(ctx);

        // The matcher only ever sees a root-relative pattern. That is what
        // retires the `glob` crate's Windows special case: `resolve` produces
        // a `\\?\C:\…` verbatim path, and the old engine only worked because
        // it explicitly whitelisted that prefix. Stripping the root means no
        // prefix — verbatim or otherwise — reaches the glob engine at all.
        let rel_pattern = match resolved.strip_prefix(&root) {
            Ok(rel) if rel.as_os_str().is_empty() => "**".to_string(),
            Ok(rel) => relative_to(Path::new(""), rel),
            Err(_) => return ToolResult::error("glob pattern escaped the project directory"),
        };

        let globs = match build_globset(&rel_pattern) {
            Ok(g) => g,
            Err(e) => return ToolResult::error(e),
        };
        let include_hidden =
            field_bool(&input, "include_hidden").unwrap_or(false) || names_a_dotfile(&rel_pattern);

        let walk_root = root.clone();
        let found = tokio::task::spawn_blocking(move || {
            let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
            for entry in crate::grep::walk_builder(&walk_root, include_hidden)
                .build()
                .flatten()
            {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                // Checking the pattern isn't enough: a wildcard can expand
                // *through* a symlink that points out of the project, so every
                // match is re-checked against the jail and silently skipped if
                // it escapes.
                if !path_is_inside(entry.path(), &walk_root) {
                    continue;
                }
                let display = relative_to(&walk_root, entry.path());
                if !globs.is_match(&display) {
                    continue;
                }
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified().map_err(Into::into))
                    .unwrap_or(std::time::UNIX_EPOCH);
                found.push((modified, display));
            }
            found
        })
        .await;

        let mut found = match found {
            Ok(f) => f,
            Err(e) => return ToolResult::error(format!("glob task failed: {e}")),
        };
        if found.is_empty() {
            return ToolResult::ok("(no matches)");
        }

        // Most recent first, because "what did I touch lately" is the question
        // this tool is usually standing in for. Ties break on path so two
        // identical calls can never disagree.
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        let total = found.len();
        let shown = total.min(MAX_GLOB_RESULTS);
        let mut out = found
            .into_iter()
            .take(shown)
            .map(|(_, path)| path)
            .collect::<Vec<_>>()
            .join("\n");
        if shown < total {
            out.push_str(&format!(
                "\n[TRUNCATED: showing the {shown} most recently modified of {total} matches — \
                 narrow the pattern]"
            ));
        }
        ToolResult::ok(out)
    }
}

/// Whether the pattern explicitly reaches for a dot-prefixed name, in which
/// case skipping hidden files would guarantee zero results. `.` and `..` are
/// already gone — `resolve` normalised them away.
fn names_a_dotfile(pattern: &str) -> bool {
    pattern.split('/').any(|part| part.starts_with('.'))
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file with the given content. Args: path (required), content (required)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Mutating
    }

    fn snapshot_paths(&self, input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
        snapshot_target(input, ctx)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        let (Some(path), Some(content)) = (field_str(&input, "path"), field_str(&input, "content"))
        else {
            return ToolResult::error("missing required fields: path, content");
        };
        let full = match resolve(ctx, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        let staged = match crate::staging::write_staged(ctx, path, content).await {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        if let Err(e) = crate::staging::apply_staged(&staged, &full).await {
            return ToolResult::error(e);
        }

        ToolResult::ok(format!("wrote {} bytes to {path}", content.len()))
    }
}

/// One `old_str` -> `new_str` substitution, checked before it is applied.
///
/// `label` names the edit in error messages so a failing batch says *which*
/// edit failed; a single `edit_file` passes an empty label.
fn substitute(
    text: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
    label: &str,
) -> Result<(String, usize), String> {
    if old_str.is_empty() {
        return Err(format!("{label}old_str must not be empty"));
    }
    if old_str == new_str {
        return Err(format!("{label}old_str and new_str are identical"));
    }
    let occurrences = text.matches(old_str).count();
    if occurrences == 0 {
        return Err(format!("{label}old_str not found"));
    }
    // Keeping the uniqueness error is deliberate: silently editing the first
    // of several identical snippets is the failure mode that costs an hour of
    // debugging. `replace_all` is how the caller says it meant all of them.
    if occurrences > 1 && !replace_all {
        return Err(format!(
            "{label}old_str is not unique ({occurrences} occurrences) — include more \
             surrounding context, or set replace_all if every occurrence should change"
        ));
    }
    let updated = if replace_all {
        text.replace(old_str, new_str)
    } else {
        text.replacen(old_str, new_str, 1)
    };
    Ok((updated, occurrences))
}

/// Writes `updated` to `path` through the staging area, then reports the diff.
async fn apply_and_diff(
    ctx: &ToolContext,
    path: &str,
    full: &Path,
    original: &str,
    updated: &str,
    summary: String,
) -> ToolResult {
    let staged = match crate::staging::write_staged(ctx, path, updated).await {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };
    if let Err(e) = crate::staging::apply_staged(&staged, full).await {
        return ToolResult::error(e);
    }

    let diff = similar::TextDiff::from_lines(original, updated)
        .unified_diff()
        .context_radius(1)
        .header("before", "after")
        .to_string();
    ToolResult::ok(format!("{summary}\n{diff}"))
}

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact occurrence of old_str with new_str in a file. Fails if old_str is missing, or if it appears more than once and replace_all is not set. \
Never include read_file's line-number prefixes in old_str. Args: path, old_str, new_str (all required), replace_all (optional, default false)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_str": {"type": "string"},
                "new_str": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "old_str", "new_str"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Mutating
    }

    fn snapshot_paths(&self, input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
        snapshot_target(input, ctx)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        let (Some(path), Some(old_str), Some(new_str)) = (
            field_str(&input, "path"),
            field_str(&input, "old_str"),
            field_str(&input, "new_str"),
        ) else {
            return ToolResult::error("missing required fields: path, old_str, new_str");
        };
        let replace_all = field_bool(&input, "replace_all").unwrap_or(false);

        let full = match resolve(ctx, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let original = match tokio::fs::read_to_string(&full).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("failed to read {path}: {e}")),
        };

        let (updated, occurrences) = match substitute(&original, old_str, new_str, replace_all, "")
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("{e} in {path}")),
        };

        let summary = if occurrences > 1 {
            format!("edited {path} ({occurrences} occurrences replaced)")
        } else {
            format!("edited {path}")
        };
        apply_and_diff(ctx, path, &full, &original, &updated, summary).await
    }
}

pub struct MultiEditTool;

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multi_edit"
    }

    fn description(&self) -> &str {
        "Apply several edits to one file atomically: all of them land or none do, and a failure leaves the file untouched. \
Edits apply in order, each against the result of the previous one — so write old_str as the text will look at that point, not as it looks now. \
Args: path (required), edits (required array of {old_str, new_str, replace_all?})."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_str": {"type": "string"},
                            "new_str": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old_str", "new_str"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Mutating
    }

    fn snapshot_paths(&self, input: &serde_json::Value, ctx: &ToolContext) -> Vec<PathBuf> {
        snapshot_target(input, ctx)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> ToolResult {
        let Some(path) = field_str(&input, "path") else {
            return ToolResult::error("missing required field: path");
        };
        let Some(edits) = input.get("edits").and_then(|v| v.as_array()) else {
            return ToolResult::error("missing required field: edits (an array)");
        };
        if edits.is_empty() {
            return ToolResult::error("edits must contain at least one edit");
        }

        let full = match resolve(ctx, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let original = match tokio::fs::read_to_string(&full).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("failed to read {path}: {e}")),
        };

        // The whole batch is composed in memory and only reaches the disk once
        // every edit has succeeded. That, not a rollback path, is what makes a
        // failure leave the file byte-identical.
        //
        // Edits compose *sequentially* rather than each applying to the
        // original. Both are defensible, but applying every edit to the
        // original needs an overlap detector to be safe: two edits touching
        // the same region would otherwise be spliced together into text
        // neither of them asked for, or one would be silently dropped —
        // wrong, and invisible in the result. Sequential composition has no
        // such case, and it keeps the uniqueness check meaningful: an edit
        // that only became ambiguous *because* of an earlier one fails loudly
        // here instead of quietly picking an occurrence.
        let mut current = original.clone();
        let mut replaced = 0usize;
        for (index, edit) in edits.iter().enumerate() {
            let (Some(old_str), Some(new_str)) =
                (field_str(edit, "old_str"), field_str(edit, "new_str"))
            else {
                return ToolResult::error(format!(
                    "edit {} is missing old_str or new_str; {path} is unchanged",
                    index + 1
                ));
            };
            let replace_all = field_bool(edit, "replace_all").unwrap_or(false);
            let label = format!("edit {}: ", index + 1);
            match substitute(&current, old_str, new_str, replace_all, &label) {
                Ok((next, occurrences)) => {
                    current = next;
                    replaced += occurrences;
                }
                Err(e) => {
                    return ToolResult::error(format!(
                        "{e} — no edits were applied, {path} is unchanged"
                    ))
                }
            }
        }

        let summary = format!(
            "applied {} edit(s) to {path} ({replaced} replacement(s))",
            edits.len()
        );
        apply_and_diff(ctx, path, &full, &original, &current, summary).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &tempfile::TempDir) -> ToolContext {
        ToolContext::new(dir.path(), "test-session")
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
    }

    /// `read_file` numbers lines by default; tests that care about the bytes
    /// rather than the presentation turn it off.
    async fn read_raw(ctx: &ToolContext, path: &str) -> String {
        ReadFileTool
            .execute(
                serde_json::json!({"path": path, "line_numbers": false}),
                ctx,
                cancel(),
            )
            .await
            .content
    }

    #[tokio::test]
    async fn write_file_leaves_no_staging_residue_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);

        let write = WriteFileTool
            .execute(
                serde_json::json!({"path": "nested/out.txt", "content": "data"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(!write.is_error, "{}", write.content);
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("nested/out.txt"))
                .await
                .unwrap(),
            "data"
        );
        let staged = dir
            .path()
            .join(".smith/staging/test-session/nested/out.txt");
        assert!(
            !staged.is_file(),
            "staging file should be removed after apply"
        );
    }

    #[tokio::test]
    async fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);

        let write = WriteFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "content": "hello\nworld"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(!write.is_error, "{}", write.content);
        assert_eq!(read_raw(&ctx, "a.txt").await, "hello\nworld");
    }

    #[tokio::test]
    async fn read_file_numbers_lines_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta").unwrap();

        let read = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), &ctx, cancel())
            .await;
        assert!(!read.is_error, "{}", read.content);
        assert_eq!(read.content, "     1\talpha\n     2\tbeta");
    }

    #[tokio::test]
    async fn read_file_respects_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("a.txt"), "1\n2\n3\n4").unwrap();

        let read = ReadFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "offset": 2, "limit": 2, "line_numbers": false}),
                &ctx,
                cancel(),
            )
            .await;
        // Truncation is stated: the model must not read "2\n3" as the file.
        assert!(read.content.starts_with("2\n3\n["), "{}", read.content);
        assert!(read.content.contains("TRUNCATED: showing lines 2-3 of 4"));
        assert!(read.content.contains("offset=4"));
    }

    #[tokio::test]
    async fn read_file_caps_a_long_file_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        let body: String = (1..=MAX_READ_LINES + 500)
            .map(|n| format!("line {n}\n"))
            .collect();
        std::fs::write(dir.path().join("long.txt"), body).unwrap();

        let read = ReadFileTool
            .execute(serde_json::json!({"path": "long.txt"}), &ctx, cancel())
            .await;
        assert!(!read.is_error, "{}", read.content);
        assert!(read.content.contains("TRUNCATED"), "{}", read.content);
        assert!(read
            .content
            .contains(&format!("of {}", MAX_READ_LINES + 500)));
        assert!(!read.content.contains("line 2001\n"));
    }

    #[tokio::test]
    async fn read_file_clips_a_pathological_line() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("min.js"), "x".repeat(MAX_LINE_CHARS * 3)).unwrap();

        let read = ReadFileTool
            .execute(serde_json::json!({"path": "min.js"}), &ctx, cancel())
            .await;
        assert!(read.content.contains("line clipped"), "{}", read.content);
        assert!(read.content.contains("some lines clipped"));
    }

    #[tokio::test]
    async fn read_file_reports_a_binary_file_instead_of_dumping_it() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(
            dir.path().join("blob.bin"),
            b"\x89PNG\r\n\x1a\n\x00\x00secret",
        )
        .unwrap();

        let read = ReadFileTool
            .execute(serde_json::json!({"path": "blob.bin"}), &ctx, cancel())
            .await;
        // Not an error: "it is a binary file" is the true answer, and it is
        // more useful than a UTF-8 decode failure.
        assert!(!read.is_error, "{}", read.content);
        assert!(read.content.contains("binary file"), "{}", read.content);
        assert!(!read.content.contains("secret"), "{}", read.content);
    }

    #[tokio::test]
    async fn read_file_missing_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        let read = ReadFileTool
            .execute(serde_json::json!({"path": "missing.txt"}), &ctx, cancel())
            .await;
        assert!(read.is_error);
    }

    #[tokio::test]
    async fn read_file_refuses_a_directory_with_a_pointer_to_list_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let read = ReadFileTool
            .execute(serde_json::json!({"path": "sub"}), &ctx, cancel())
            .await;
        assert!(read.is_error);
        assert!(read.content.contains("list_dir"), "{}", read.content);
    }

    #[tokio::test]
    async fn list_dir_marks_directories() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        tokio::fs::write(dir.path().join("file.txt"), "x")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("sub")).await.unwrap();

        let listing = ListDirTool
            .execute(serde_json::json!({"path": "."}), &ctx, cancel())
            .await;
        assert!(!listing.is_error);
        assert!(listing.content.contains("file.txt"));
        assert!(listing.content.contains("sub/"));
    }

    #[tokio::test]
    async fn edit_file_requires_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("a.txt"), "foo foo").unwrap();

        let ambiguous = EditFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "old_str": "foo", "new_str": "bar"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(ambiguous.is_error);
        assert!(ambiguous.content.contains("replace_all"));

        std::fs::write(dir.path().join("a.txt"), "foo baz").unwrap();
        let edited = EditFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "old_str": "foo", "new_str": "bar"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(!edited.is_error, "{}", edited.content);
        assert_eq!(read_raw(&ctx, "a.txt").await, "bar baz");
    }

    #[tokio::test]
    async fn edit_file_replace_all_changes_every_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("a.txt"), "foo foo foo").unwrap();

        let edited = EditFileTool
            .execute(
                serde_json::json!({
                    "path": "a.txt", "old_str": "foo", "new_str": "bar", "replace_all": true
                }),
                &ctx,
                cancel(),
            )
            .await;
        assert!(!edited.is_error, "{}", edited.content);
        assert!(
            edited.content.contains("3 occurrences"),
            "{}",
            edited.content
        );
        assert_eq!(read_raw(&ctx, "a.txt").await, "bar bar bar");
    }

    #[tokio::test]
    async fn edit_file_errors_when_old_str_missing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();

        let result = EditFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "old_str": "nope", "new_str": "x"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(result.is_error);
    }

    // --- multi_edit ------------------------------------------------------

    #[tokio::test]
    async fn multi_edit_applies_every_edit() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("a.rs"), "let a = 1;\nlet b = 2;\n").unwrap();

        let result = MultiEditTool
            .execute(
                serde_json::json!({"path": "a.rs", "edits": [
                    {"old_str": "let a = 1;", "new_str": "let a = 10;"},
                    {"old_str": "let b = 2;", "new_str": "let b = 20;"}
                ]}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(read_raw(&ctx, "a.rs").await, "let a = 10;\nlet b = 20;");
    }

    /// The semantics decision, pinned: edit 2 sees edit 1's output. Applying
    /// both against the original would either lose one of these or splice
    /// them, and would do it silently.
    #[tokio::test]
    async fn multi_edit_applies_edits_sequentially_not_against_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();

        let result = MultiEditTool
            .execute(
                serde_json::json!({"path": "a.txt", "edits": [
                    {"old_str": "alpha", "new_str": "beta"},
                    {"old_str": "beta", "new_str": "gamma"}
                ]}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(read_raw(&ctx, "a.txt").await, "gamma");
    }

    #[tokio::test]
    async fn a_failing_edit_leaves_the_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        let before = "one\ntwo\nthree\n";
        std::fs::write(dir.path().join("a.txt"), before).unwrap();

        let result = MultiEditTool
            .execute(
                serde_json::json!({"path": "a.txt", "edits": [
                    {"old_str": "one", "new_str": "1"},
                    {"old_str": "not present", "new_str": "x"},
                    {"old_str": "three", "new_str": "3"}
                ]}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("edit 2"), "{}", result.content);
        assert_eq!(
            std::fs::read(dir.path().join("a.txt")).unwrap(),
            before.as_bytes(),
            "a failed batch modified the file"
        );
    }

    #[tokio::test]
    async fn multi_edit_reports_which_edit_became_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        // After edit 1 there are two `x`s, so edit 2 is ambiguous — a case
        // that only exists because edits compose sequentially, and one that
        // has to fail loudly rather than pick one.
        let before = "x\ny\n";
        std::fs::write(dir.path().join("a.txt"), before).unwrap();

        let result = MultiEditTool
            .execute(
                serde_json::json!({"path": "a.txt", "edits": [
                    {"old_str": "y", "new_str": "x"},
                    {"old_str": "x", "new_str": "z"}
                ]}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("not unique"), "{}", result.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn multi_edit_rejects_an_empty_batch() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let result = MultiEditTool
            .execute(
                serde_json::json!({"path": "a.txt", "edits": []}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn multi_edit_refuses_to_escape_the_project() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("target.txt"), "untouched").unwrap();
        let dir = tempfile::tempdir().unwrap();

        let escape = format!("../{}/target.txt", {
            let name = outside.path().file_name().unwrap();
            name.to_string_lossy().into_owned()
        });
        let result = MultiEditTool
            .execute(
                serde_json::json!({"path": escape, "edits": [
                    {"old_str": "untouched", "new_str": "touched"}
                ]}),
                &ctx(&dir),
                cancel(),
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert_eq!(
            std::fs::read_to_string(outside.path().join("target.txt")).unwrap(),
            "untouched"
        );
    }

    // --- glob ------------------------------------------------------------

    #[tokio::test]
    async fn glob_finds_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        tokio::fs::write(dir.path().join("a.rs"), "").await.unwrap();
        tokio::fs::write(dir.path().join("b.txt"), "")
            .await
            .unwrap();

        let result = GlobTool
            .execute(serde_json::json!({"pattern": "*.rs"}), &ctx, cancel())
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content, "a.rs");
    }

    #[tokio::test]
    async fn glob_matches_a_bare_name_pattern_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::write(dir.path().join("src/deep/a.rs"), "").unwrap();

        let result = GlobTool
            .execute(serde_json::json!({"pattern": "*.rs"}), &ctx, cancel())
            .await;
        assert_eq!(result.content, "src/deep/a.rs");
    }

    #[tokio::test]
    async fn glob_anchors_a_pattern_containing_a_slash() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        std::fs::write(dir.path().join("src/deep/a.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/b.rs"), "").unwrap();
        std::fs::write(dir.path().join("top.rs"), "").unwrap();

        let result = GlobTool
            .execute(
                serde_json::json!({"pattern": "src/**/*.rs"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(
            result.content.contains("src/deep/a.rs"),
            "{}",
            result.content
        );
        assert!(result.content.contains("src/b.rs"), "{}", result.content);
        assert!(!result.content.contains("top.rs"), "{}", result.content);
    }

    /// The whole point of moving off the `glob` crate: `target/` and friends
    /// used to flood every result.
    #[tokio::test]
    async fn glob_respects_gitignore_and_skips_hidden_directories() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::write(dir.path().join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        for rel in [
            "src/main.rs",
            "target/debug/build.rs",
            "node_modules/pkg/index.rs",
            ".git/hooks/pre-commit.rs",
        ] {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }

        let result = GlobTool
            .execute(serde_json::json!({"pattern": "**/*.rs"}), &ctx, cancel())
            .await;
        assert_eq!(result.content, "src/main.rs", "{}", result.content);
    }

    #[tokio::test]
    async fn glob_orders_by_modification_time_most_recent_first() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        for (name, ago) in [("old.rs", 900), ("middle.rs", 600), ("new.rs", 60)] {
            let path = dir.path().join(name);
            let file = std::fs::File::create(&path).unwrap();
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(ago);
            file.set_modified(when).unwrap();
        }

        let result = GlobTool
            .execute(serde_json::json!({"pattern": "*.rs"}), &ctx, cancel())
            .await;
        assert_eq!(result.content, "new.rs\nmiddle.rs\nold.rs");
    }

    #[tokio::test]
    async fn glob_truncates_visibly() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        for n in 0..MAX_GLOB_RESULTS + 20 {
            std::fs::write(dir.path().join(format!("f{n:04}.rs")), "").unwrap();
        }

        let result = GlobTool
            .execute(serde_json::json!({"pattern": "*.rs"}), &ctx, cancel())
            .await;
        assert!(result.content.contains("TRUNCATED"), "no marker");
        assert!(result
            .content
            .contains(&format!("of {} matches", MAX_GLOB_RESULTS + 20)));
    }

    #[tokio::test]
    async fn glob_can_reach_dotfiles_when_the_pattern_asks_for_them() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        std::fs::create_dir_all(dir.path().join(".github/workflows")).unwrap();
        std::fs::write(dir.path().join(".github/workflows/ci.yml"), "").unwrap();

        let result = GlobTool
            .execute(
                serde_json::json!({"pattern": ".github/**/*.yml"}),
                &ctx,
                cancel(),
            )
            .await;
        assert_eq!(result.content, ".github/workflows/ci.yml");
    }

    // --- path jail -------------------------------------------------------
    //
    // Each of these used to succeed. They are the concrete escapes an agent
    // (or a prompt-injected instruction inside a file it read) could use to
    // reach outside the project it was pointed at.

    #[tokio::test]
    async fn read_file_refuses_an_absolute_path_outside_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let result = ReadFileTool
            .execute(
                serde_json::json!({"path": "/etc/passwd"}),
                &ctx(&dir),
                cancel(),
            )
            .await;
        assert!(result.is_error, "got: {}", result.content);
        assert!(result.content.contains("outside the project directory"));
    }

    #[tokio::test]
    async fn read_file_refuses_a_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("inside.txt"), "ok").unwrap();
        let result = ReadFileTool
            .execute(
                serde_json::json!({"path": "../../../../etc/passwd"}),
                &ctx(&dir),
                cancel(),
            )
            .await;
        assert!(result.is_error, "got: {}", result.content);
    }

    /// `starts_with` is component-wise, so a traversal that dips back inside
    /// the root would pass a naive prefix check: `<root>/a/../../etc/passwd`
    /// literally begins with `<root>`.
    #[tokio::test]
    async fn read_file_refuses_a_traversal_that_re_enters_the_root_textually() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let result = ReadFileTool
            .execute(
                serde_json::json!({"path": "a/../../etc/passwd"}),
                &ctx(&dir),
                cancel(),
            )
            .await;
        assert!(result.is_error, "got: {}", result.content);
    }

    #[tokio::test]
    async fn write_file_refuses_to_escape_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let result = WriteFileTool
            .execute(
                serde_json::json!({"path": "../escaped.txt", "content": "nope"}),
                &ctx(&dir),
                cancel(),
            )
            .await;
        assert!(result.is_error, "got: {}", result.content);
        assert!(!dir.path().parent().unwrap().join("escaped.txt").exists());
    }

    #[tokio::test]
    async fn glob_refuses_a_pattern_that_escapes_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let result = GlobTool
            .execute(
                serde_json::json!({"pattern": "../../*.rs"}),
                &ctx(&dir),
                cancel(),
            )
            .await;
        assert!(result.is_error, "got: {}", result.content);
        assert!(result.content.contains("outside the project directory"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_refuses_a_symlink_pointing_out_of_the_project() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "classified").unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            dir.path().join("link.txt"),
        )
        .unwrap();

        let result = ReadFileTool
            .execute(
                serde_json::json!({"path": "link.txt"}),
                &ctx(&dir),
                cancel(),
            )
            .await;
        assert!(result.is_error, "got: {}", result.content);
        assert!(!result.content.contains("classified"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_skips_matches_that_escape_through_a_symlink() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "classified").unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mine.rs"), "ok").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("elsewhere")).unwrap();

        let result = GlobTool
            .execute(
                serde_json::json!({"pattern": "**/*.rs"}),
                &ctx(&dir),
                cancel(),
            )
            .await;
        assert!(!result.is_error, "got: {}", result.content);
        assert!(result.content.contains("mine.rs"));
        assert!(
            !result.content.contains("secret.rs"),
            "glob leaked outside the project: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn normal_paths_inside_the_project_still_work() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();

        // Relative, nested-relative, `.`-prefixed and absolute-inside all
        // have to keep working — the jail must not cost normal usage.
        for path in [
            "src/main.rs",
            "./src/main.rs",
            "src/../src/main.rs",
            dir.path().join("src/main.rs").to_str().unwrap(),
        ] {
            let result = ReadFileTool
                .execute(serde_json::json!({"path": path}), &ctx(&dir), cancel())
                .await;
            assert!(!result.is_error, "{path} was rejected: {}", result.content);
            assert!(result.content.contains("fn main"));
        }
    }

    #[test]
    fn lexical_normalize_resolves_parent_segments() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            lexical_normalize(Path::new("/a/./b")),
            PathBuf::from("/a/b")
        );
        // Climbing past the root stays at the root rather than going negative.
        assert_eq!(lexical_normalize(Path::new("/../..")), PathBuf::from("/"));
    }

    #[test]
    fn clip_line_cuts_on_a_char_boundary() {
        // `€` is three bytes wide: a byte-indexed cut lands mid-character and
        // panics — the same bug `shell_tool::truncate_tail` was written for.
        let (out, clipped) = clip_line(&"€".repeat(10), 4);
        assert!(clipped);
        assert_eq!(out.matches('€').count(), 4);
        assert!(out.contains("line clipped"));

        let (out, clipped) = clip_line("short", 400);
        assert!(!clipped);
        assert_eq!(out, "short");
    }

    #[test]
    fn substitute_refuses_a_no_op_edit() {
        assert!(substitute("abc", "a", "a", false, "").is_err());
        assert!(substitute("abc", "", "x", false, "").is_err());
    }
}
