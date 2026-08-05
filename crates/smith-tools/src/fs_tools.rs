use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use std::path::{Path, PathBuf};

fn resolve(ctx: &ToolContext, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.cwd.join(p)
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
        let full = resolve(ctx, path);

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
        let full = resolve(ctx, path);

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
        let full_pattern = resolve(ctx, pattern);
        let full_pattern = full_pattern.to_string_lossy().into_owned();

        let paths = match glob::glob(&full_pattern) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("invalid glob pattern: {e}")),
        };

        let mut matches = Vec::new();
        for entry in paths {
            match entry {
                Ok(p) => matches.push(p.to_string_lossy().into_owned()),
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
        let full = resolve(ctx, path);

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
        let full = resolve(ctx, path);

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
}
