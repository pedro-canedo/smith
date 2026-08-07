//! Resolving a model-supplied path, and refusing one that leaves the project.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use smith_core::ToolContext;
use std::path::{Component, Path, PathBuf};

use super::*;

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

/// Whether a call's `path` argument lands inside the session's scratch
/// directory (`ToolContext::scratch_dir`) once `..` and symlinks are resolved.
///
/// The answer the write tools give for `Tool::scratch_scoped`, shared so the
/// three of them cannot drift. It leans on `resolve` for exactly the same
/// escape-closing the jail check does: a lexical `..` is normalised away
/// before the prefix comparison, and a symlink inside scratch pointing
/// elsewhere resolves to its target — which then fails the prefix check, so
/// the call falls back to an ordinary permission prompt rather than being
/// waived. Failing closed is the whole contract: any doubt costs one prompt,
/// never one file.
pub(crate) fn scratch_confined(input: &serde_json::Value, ctx: &ToolContext) -> bool {
    let Some(path) = field_str(input, "path") else {
        return false;
    };
    let Ok(resolved) = resolve(ctx, path) else {
        return false;
    };
    let scratch_root = real_path(&lexical_normalize(&ctx.scratch_dir()));
    resolved.starts_with(&scratch_root)
}

/// Drops `.` and resolves `..` textually, without touching the filesystem.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
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
