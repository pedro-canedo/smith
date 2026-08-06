//! Sweeping of stale per-session scratch directories.
//!
//! `ToolContext::scratch_dir` gives every session a private directory under
//! `.smith/scratch/<session_id>/` for throwaway files — helper scripts,
//! intermediate data — where writes skip the permission prompt. Nothing in
//! there is user work, so nothing in there deserves to outlive its session by
//! much: this module reclaims the directories of sessions that have gone
//! quiet, the same once-per-process, off-the-critical-path lifecycle as
//! `CheckpointStore::sweep`.
//!
//! Like every janitorial job in this codebase, the sweep is best-effort
//! throughout: an unreadable entry is skipped, a failed removal is ignored,
//! and nothing here can fail startup.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A week. Checkpoints keep two (they guard user work); scratch holds
/// throwaways by definition, so it gets half the patience.
pub const DEFAULT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Removes every `.smith/scratch/<session>/` whose contents have not changed
/// for `ttl` — except `keep_session`, the session now starting, which is
/// never swept no matter how old it is (`--resume` legitimately picks up a
/// session older than any TTL). Returns how many directories were removed.
pub async fn sweep(project_root: &Path, ttl: Duration, keep_session: &str) -> usize {
    let root = project_root.join(".smith").join("scratch");
    let Some(cutoff) = SystemTime::now().checked_sub(ttl) else {
        return 0;
    };

    let mut removed = 0usize;
    let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
        return 0; // No scratch root yet — the common case.
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if entry.file_name().to_string_lossy() == keep_session {
            continue;
        }
        let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if !meta.is_dir() {
            continue; // A stray file was never a session's scratch dir.
        }
        let stale = matches!(newest_mtime(&path).await, Some(newest) if newest < cutoff);
        if stale && tokio::fs::remove_dir_all(&path).await.is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The most recent modification time anywhere under `dir`, including `dir`
/// itself.
///
/// The directory's own mtime is not enough: it moves when entries are added
/// or removed, not when a file inside is rewritten, and "the session is still
/// touching its scratch" is exactly the signal that must keep it alive.
/// Symlinks are not followed — a link's own timestamp counts, its target
/// (possibly outside the project) does not.
async fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest = tokio::fs::symlink_metadata(dir)
        .await
        .ok()
        .and_then(|m| m.modified().ok())?;

    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(meta) = tokio::fs::symlink_metadata(entry.path()).await else {
                continue;
            };
            if let Ok(modified) = meta.modified() {
                newest = newest.max(modified);
            }
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Some(newest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_root(project: &Path) -> PathBuf {
        project.join(".smith").join("scratch")
    }

    fn make_session_dir(project: &Path, session: &str) -> PathBuf {
        let dir = scratch_root(project).join(session);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("scratch.txt"), "temp").unwrap();
        dir
    }

    #[tokio::test]
    async fn a_missing_scratch_root_sweeps_nothing_and_does_not_fail() {
        let project = tempfile::tempdir().unwrap();
        assert_eq!(sweep(project.path(), DEFAULT_TTL, "current").await, 0);
    }

    #[tokio::test]
    async fn fresh_directories_survive_a_sweep() {
        let project = tempfile::tempdir().unwrap();
        let other = make_session_dir(project.path(), "other-session");

        // An hour of TTL against directories created milliseconds ago.
        let removed = sweep(project.path(), Duration::from_secs(3600), "current").await;

        assert_eq!(removed, 0);
        assert!(other.exists());
    }

    #[tokio::test]
    async fn stale_directories_are_removed_but_never_the_current_session() {
        let project = tempfile::tempdir().unwrap();
        let stale = make_session_dir(project.path(), "old-session");
        let current = make_session_dir(project.path(), "current");

        // A zero TTL makes everything already-written "stale" — which is the
        // point: the current session must survive on its name, not its age.
        // The pause keeps the newest mtime strictly behind the cutoff even on
        // a filesystem with coarse timestamps.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let removed = sweep(project.path(), Duration::ZERO, "current").await;

        assert_eq!(removed, 1);
        assert!(!stale.exists());
        assert!(current.exists(), "the running session's scratch was swept");
    }

    #[tokio::test]
    async fn a_stray_file_in_the_scratch_root_is_left_alone() {
        let project = tempfile::tempdir().unwrap();
        let root = scratch_root(project.path());
        std::fs::create_dir_all(&root).unwrap();
        let stray = root.join("not-a-directory.txt");
        std::fs::write(&stray, "??").unwrap();

        tokio::time::sleep(Duration::from_millis(25)).await;
        let removed = sweep(project.path(), Duration::ZERO, "current").await;

        assert_eq!(removed, 0);
        assert!(stray.exists());
    }
}
