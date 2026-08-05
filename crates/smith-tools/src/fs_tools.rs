use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use std::path::{Component, Path, PathBuf};

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
fn resolve(ctx: &ToolContext, path: &str) -> Result<PathBuf, String> {
    let requested = Path::new(path);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        ctx.cwd.join(requested)
    };

    let root = real_path(&lexical_normalize(&ctx.cwd));
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

fn field_str<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file, optionally a line range. Args: path (required), offset (1-based line number, optional), limit (max lines, optional)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "minimum": 1},
                "limit": {"type": "integer", "minimum": 1}
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

        let content = match tokio::fs::read_to_string(&full).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("failed to read {path}: {e}")),
        };

        let offset = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.saturating_sub(1).min(lines.len());
        let end = match limit {
            Some(l) => (start + l).min(lines.len()),
            None => lines.len(),
        };

        ToolResult::ok(lines[start..end].join("\n"))
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
        "Find files matching a glob pattern (e.g. \"src/**/*.rs\"), relative to the project directory. Args: pattern (required)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "pattern": {"type": "string"} },
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
        let full_pattern = match resolve(ctx, pattern) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let full_pattern = full_pattern.to_string_lossy().into_owned();

        let paths = match glob::glob(&full_pattern) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("invalid glob pattern: {e}")),
        };

        // Checking the pattern isn't enough: a wildcard can expand *through*
        // a symlink that points out of the project, so every match is
        // re-checked against the jail and silently skipped if it escapes.
        let root = real_path(&lexical_normalize(&ctx.cwd));
        let mut matches = Vec::new();
        for entry in paths {
            match entry {
                Ok(p) => {
                    if real_path(&lexical_normalize(&p)).starts_with(&root) {
                        matches.push(p.to_string_lossy().into_owned());
                    }
                }
                Err(e) => return ToolResult::error(format!("glob error: {e}")),
            }
        }

        if matches.is_empty() {
            ToolResult::ok("(no matches)")
        } else {
            ToolResult::ok(matches.join("\n"))
        }
    }
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

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace an exact, unique occurrence of old_str with new_str in a file. Fails if old_str is missing or appears more than once. Args: path, old_str, new_str (all required)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_str": {"type": "string"},
                "new_str": {"type": "string"}
            },
            "required": ["path", "old_str", "new_str"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::Mutating
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
        let full = match resolve(ctx, path) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        let original = match tokio::fs::read_to_string(&full).await {
            Ok(c) => c,
            Err(e) => return ToolResult::error(format!("failed to read {path}: {e}")),
        };

        let occurrences = original.matches(old_str).count();
        if occurrences == 0 {
            return ToolResult::error(format!("old_str not found in {path}"));
        }
        if occurrences > 1 {
            return ToolResult::error(format!(
                "old_str is not unique in {path} ({occurrences} occurrences) — include more context"
            ));
        }

        let updated = original.replacen(old_str, new_str, 1);

        let staged = match crate::staging::write_staged(ctx, path, &updated).await {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };

        if let Err(e) = crate::staging::apply_staged(&staged, &full).await {
            return ToolResult::error(e);
        }

        let diff = similar::TextDiff::from_lines(&original, &updated)
            .unified_diff()
            .context_radius(1)
            .header("before", "after")
            .to_string();

        ToolResult::ok(format!("edited {path}\n{diff}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &tempfile::TempDir) -> ToolContext {
        ToolContext {
            cwd: dir.path().to_path_buf(),
            session_id: "test-session".into(),
        }
    }

    fn cancel() -> CancellationToken {
        CancellationToken::new()
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

        let read = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), &ctx, cancel())
            .await;
        assert!(!read.is_error);
        assert_eq!(read.content, "hello\nworld");
    }

    #[tokio::test]
    async fn read_file_respects_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        WriteFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "content": "1\n2\n3\n4"}),
                &ctx,
                cancel(),
            )
            .await;

        let read = ReadFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "offset": 2, "limit": 2}),
                &ctx,
                cancel(),
            )
            .await;
        assert_eq!(read.content, "2\n3");
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
        WriteFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "content": "foo foo"}),
                &ctx,
                cancel(),
            )
            .await;

        let ambiguous = EditFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "old_str": "foo", "new_str": "bar"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(ambiguous.is_error);

        WriteFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "content": "foo baz"}),
                &ctx,
                cancel(),
            )
            .await;
        let edited = EditFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "old_str": "foo", "new_str": "bar"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(!edited.is_error, "{}", edited.content);

        let read = ReadFileTool
            .execute(serde_json::json!({"path": "a.txt"}), &ctx, cancel())
            .await;
        assert_eq!(read.content, "bar baz");
    }

    #[tokio::test]
    async fn edit_file_errors_when_old_str_missing() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir);
        WriteFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "content": "hello"}),
                &ctx,
                cancel(),
            )
            .await;

        let result = EditFileTool
            .execute(
                serde_json::json!({"path": "a.txt", "old_str": "nope", "new_str": "x"}),
                &ctx,
                cancel(),
            )
            .await;
        assert!(result.is_error);
    }

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
        assert!(result.content.ends_with("a.rs"));
        assert!(!result.content.contains("b.txt"));
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
}
