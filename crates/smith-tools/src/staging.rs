//! Per-session staging area for mutating file tools.
//!
//! Writes/edits materialize under `.smith/staging/<session_id>/…` first, then
//! are copied to the real project path. Staging stays invisible to the user
//! (permission modals only show the target path summary).
//!
//! **This is not the undo buffer — see [`crate::checkpoint`].** The two are
//! easy to confuse and do opposite jobs. Staging holds the *new* content of a
//! write for the moment between "composed" and "applied", and deletes itself
//! the instant the copy succeeds; nothing here survives the tool call, so
//! there is nothing here to rewind to. Checkpoints hold the *old* bytes,
//! content-addressed, and persist for as long as undoing the turn is still
//! plausible. A `/rewind` built on staging would find an empty directory.

use std::path::{Component, Path, PathBuf};

use smith_core::ToolContext;

/// Absolute path where a proposed write/edit for `rel_or_abs` is staged.
pub fn staging_path(ctx: &ToolContext, path: &str) -> PathBuf {
    let rel = staging_relative_key(&ctx.cwd, path);
    ctx.cwd
        .join(".smith")
        .join("staging")
        .join(&ctx.session_id)
        .join(rel)
}

/// Resolve a user-facing path to a stable relative key under the staging root.
/// Absolute paths outside cwd are flattened to a safe `_abs/…` tree so they
/// cannot escape the staging directory via `..`.
fn staging_relative_key(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    let full = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };

    if let Ok(stripped) = full.strip_prefix(cwd) {
        return normalize_rel(stripped);
    }

    let mut out = PathBuf::from("_abs");
    for c in full.components() {
        match c {
            Component::RootDir | Component::Prefix(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                out.push("__up__");
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

fn normalize_rel(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.push("__up__");
            }
            Component::Normal(s) => out.push(s),
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("_root")
    } else {
        out
    }
}

/// Write `content` to the staging path for `path`, creating parents as needed.
pub async fn write_staged(ctx: &ToolContext, path: &str, content: &str) -> Result<PathBuf, String> {
    let staged = staging_path(ctx, path);
    if let Some(parent) = staged.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("failed to create staging dir: {e}"))?;
    }
    tokio::fs::write(&staged, content)
        .await
        .map_err(|e| format!("failed to write staging file: {e}"))?;
    Ok(staged)
}

/// Copy a staged file to `target`, creating parent dirs. Removes the staged
/// file on success. Leaves staging intact if the copy fails.
pub async fn apply_staged(staged: &Path, target: &Path) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("failed to create parent directories: {e}"))?;
    }
    tokio::fs::copy(staged, target)
        .await
        .map_err(|e| format!("failed to apply staged file to {}: {e}", target.display()))?;
    let _ = tokio::fs::remove_file(staged).await;
    // Best-effort: prune empty staging parents (ignore errors).
    if let Some(parent) = staged.parent() {
        let _ = tokio::fs::remove_dir(parent).await;
    }
    Ok(())
}

/// Delete a staged file if it exists (e.g. after a failed apply we may call
/// this explicitly; apply_staged already removes on success).
pub async fn discard_staged(staged: &Path) -> Result<(), String> {
    match tokio::fs::remove_file(staged).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to discard staging file: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ctx(dir: &tempfile::TempDir) -> ToolContext {
        ToolContext::new(dir.path(), "sess-test")
    }

    #[test]
    fn mirrors_relative_paths_under_session() {
        let dir = tempdir().unwrap();
        let c = ctx(&dir);
        let staged = staging_path(&c, "docs/guide.md");
        assert_eq!(
            staged,
            dir.path().join(".smith/staging/sess-test/docs/guide.md")
        );
    }

    #[test]
    fn flattens_absolute_paths_safely() {
        let dir = tempdir().unwrap();
        let c = ctx(&dir);
        let staged = staging_path(&c, "/etc/passwd");
        assert!(staged.starts_with(dir.path().join(".smith/staging/sess-test/_abs")));
        assert!(
            staged.ends_with("etc/passwd")
                || staged.components().any(|c| c.as_os_str() == "passwd")
        );
    }

    #[tokio::test]
    async fn write_apply_cleans_staging_and_updates_target() {
        let dir = tempdir().unwrap();
        let c = ctx(&dir);
        let staged = write_staged(&c, "a.txt", "hello").await.unwrap();
        assert!(staged.is_file());

        let target = dir.path().join("a.txt");
        apply_staged(&staged, &target).await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&target).await.unwrap(), "hello");
        assert!(!staged.is_file());
    }

    #[tokio::test]
    async fn discard_removes_staged_file() {
        let dir = tempdir().unwrap();
        let c = ctx(&dir);
        let staged = write_staged(&c, "b.txt", "x").await.unwrap();
        discard_staged(&staged).await.unwrap();
        assert!(!staged.is_file());
    }
}
