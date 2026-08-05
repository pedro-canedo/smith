//! Content-addressed turn checkpoints, and the `/rewind` that restores them.
//!
//! ```text
//! .smith/checkpoints/
//!   objects/<ab>/<sha256>         the bytes, once each
//!   turns/<session_id>/<seq>.json the manifest: which paths, which hashes
//! ```
//!
//! **Why not git.** The repository belongs to the user. Writing to their
//! index, stash or refs — even under a private namespace — races their own
//! concurrent git commands, and breaks outright in submodules, worktrees, bare
//! repos, and in the very common case of a project that is not a repository at
//! all. A checkpoint needs exactly one thing: the bytes of the files this turn
//! touched, as they were before the turn. Hashing those bytes into a private
//! directory delivers that with no git-state reasoning anywhere, works in a
//! dirty tree, and dedups repeated snapshots of an unchanged file down to
//! nothing for free.
//!
//! **Not to be confused with [`crate::staging`]**, which holds the *new*
//! content of a write for the moment between "composed" and "applied" and
//! deletes itself immediately after. Staging looks forward and is transient;
//! this looks backward and persists. Neither can do the other's job.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smith_core::checkpoint::{
    Checkpointer, ConflictKind, RewindConflict, RewindReport, RewindStatus,
};

/// How long a checkpoint is kept.
///
/// Age rather than a count, because the value of a checkpoint decays with
/// wall-clock time — "undo what just happened" — not with how many turns have
/// gone by since. A count cap set low enough to bound disk use would evict
/// this morning's checkpoint on a busy afternoon, which is exactly when a user
/// reaches for it. Two weeks of deduplicated source files is single-digit
/// megabytes.
pub const DEFAULT_TTL: Duration = Duration::from_secs(14 * 24 * 60 * 60);

// ---- on-disk shapes --------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileEntry {
    /// Project-relative where possible (absolute only for a path outside the
    /// project root, which the tool jail should make impossible). Relative so
    /// the display name and the stored key are the same string.
    path: String,
    /// The flag that makes "delete it again" expressible: false means the turn
    /// created this file, and restoring means removing it.
    existed_before: bool,
    /// `None` exactly when `existed_before` is false.
    hash_before: Option<String>,
    /// What the tool left. Not redundant: it is the only way to distinguish a
    /// file the user hand-edited afterwards from one still holding what the
    /// turn wrote. `None` means the tool deleted it (or we never got to look).
    hash_after: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Manifest {
    seq: u64,
    created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(default)]
    files: Vec<FileEntry>,
    /// Names of tools that ran and could have changed anything — `run_bash`,
    /// every MCP tool. One entry per call, so the report can say "2x".
    #[serde(default)]
    uncovered: Vec<String>,
}

impl Manifest {
    fn new(seq: u64) -> Self {
        Self {
            seq,
            created_at: now_millis(),
            ..Self::default()
        }
    }

    /// A manifest with nothing in it is never written — an empty file per turn
    /// would be pure noise for the sweeper to walk.
    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.uncovered.is_empty() && self.note.is_none()
    }
}

#[derive(Default)]
struct State {
    /// Manifests this process has open. A resumed session's older manifests
    /// live only on disk and are read on demand.
    manifests: HashMap<(String, u64), Manifest>,
    /// Highest sequence number handed out per session this process.
    last_seq: HashMap<String, u64>,
}

// ---- the store -------------------------------------------------------------

pub struct CheckpointStore {
    root: PathBuf,
    state: tokio::sync::Mutex<State>,
}

/// One restore action, resolved against the filesystem.
enum Step {
    Restore {
        path: PathBuf,
        hash: String,
        display: String,
    },
    Delete {
        path: PathBuf,
        display: String,
    },
}

impl Step {
    fn path(&self) -> &Path {
        match self {
            Step::Restore { path, .. } | Step::Delete { path, .. } => path,
        }
    }
}

impl CheckpointStore {
    /// `root` is the project directory — the same one the tool jail uses, so
    /// every path a tool can legally write is expressible relative to it.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            state: tokio::sync::Mutex::new(State::default()),
        }
    }

    fn base(&self) -> PathBuf {
        self.root.join(".smith").join("checkpoints")
    }

    fn objects_dir(&self) -> PathBuf {
        self.base().join("objects")
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.base().join("turns").join(sanitize(session_id))
    }

    fn manifest_path(&self, session_id: &str, seq: u64) -> PathBuf {
        self.session_dir(session_id).join(format!("{seq}.json"))
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        // Two-level fan-out: a flat directory of tens of thousands of entries
        // is slow to walk on every sweep, and unpleasant to look at.
        self.objects_dir().join(&hash[..2]).join(hash)
    }

    fn path_key(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn full_path(&self, key: &str) -> PathBuf {
        // `join` with an absolute path replaces, so this handles both the
        // relative keys and the absolute fallback without branching.
        self.root.join(key)
    }

    // ---- objects -----------------------------------------------------------

    /// Stores `bytes` and returns their hash. Content already present is not
    /// rewritten — this is where two turns snapshotting an identical file
    /// collapse to a single object.
    async fn put_object(&self, bytes: &[u8]) -> Result<String, String> {
        let hash = hash_bytes(bytes);
        let path = self.object_path(&hash);
        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(hash);
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        write_atomic(&path, bytes).await?;
        Ok(hash)
    }

    async fn get_object(&self, hash: &str) -> Result<Vec<u8>, String> {
        tokio::fs::read(self.object_path(hash))
            .await
            .map_err(|e| e.to_string())
    }

    // ---- manifests ---------------------------------------------------------

    async fn write_manifest(&self, session_id: &str, manifest: &Manifest) -> Result<(), String> {
        if manifest.is_empty() {
            // Also covers the un-write: `snapshot_after` can empty a manifest
            // by dropping a no-op entry, and leaving the stale file behind
            // would make a preview offer a restore it already ruled out.
            let _ = tokio::fs::remove_file(self.manifest_path(session_id, manifest.seq)).await;
            return Ok(());
        }
        let dir = self.session_dir(session_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let encoded = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
        write_atomic(&self.manifest_path(session_id, manifest.seq), &encoded).await
    }

    async fn load_manifest(&self, session_id: &str, seq: u64) -> Option<Manifest> {
        {
            let state = self.state.lock().await;
            if let Some(m) = state.manifests.get(&(session_id.to_string(), seq)) {
                return Some(m.clone());
            }
        }
        let bytes = tokio::fs::read(self.manifest_path(session_id, seq))
            .await
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Sequence numbers with a manifest on disk, ascending.
    async fn stored_seqs(&self, session_id: &str) -> Vec<u64> {
        let mut seqs = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(self.session_dir(session_id)).await else {
            return seqs;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(seq) = entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u64>().ok())
            {
                seqs.push(seq);
            }
        }
        seqs.sort_unstable();
        seqs
    }

    /// The most recent turn with something to rewind.
    pub async fn latest_turn(&self, session_id: &str) -> Option<u64> {
        self.stored_seqs(session_id).await.last().copied()
    }

    /// For each path in `manifest`, the earliest *later* turn that also wrote
    /// it — the difference between "you hand-edited this" and "a later turn
    /// overwrote it", which are the same observation but not the same warning.
    async fn later_writers(&self, session_id: &str, seq: u64) -> HashMap<String, u64> {
        let mut out: HashMap<String, u64> = HashMap::new();
        for later in self.stored_seqs(session_id).await {
            if later <= seq {
                continue;
            }
            let Some(manifest) = self.load_manifest(session_id, later).await else {
                continue;
            };
            for file in manifest.files {
                out.entry(file.path).or_insert(later);
            }
        }
        out
    }

    // ---- planning ----------------------------------------------------------

    /// What a rewind of `turn` would do, without touching anything.
    pub async fn plan(&self, session_id: &str, turn: Option<u64>) -> RewindReport {
        self.plan_steps(session_id, turn).await.0
    }

    async fn plan_steps(&self, session_id: &str, turn: Option<u64>) -> (RewindReport, Vec<Step>) {
        let seq = match turn {
            Some(seq) => seq,
            None => match self.latest_turn(session_id).await {
                Some(seq) => seq,
                None => return (RewindReport::nothing(None), Vec::new()),
            },
        };
        let Some(manifest) = self.load_manifest(session_id, seq).await else {
            let mut report = RewindReport::nothing(Some(seq));
            report
                .notes
                .push(format!("no checkpoint was recorded for turn {seq}"));
            return (report, Vec::new());
        };

        let later = self.later_writers(session_id, seq).await;
        let mut report = RewindReport::nothing(Some(seq));
        report.uncovered = tally(&manifest.uncovered);
        if let Some(note) = &manifest.note {
            report.notes.push(note.clone());
        }

        let mut steps = Vec::new();
        for file in &manifest.files {
            let full = self.full_path(&file.path);
            let current = hash_of(&full).await;

            // Already exactly where the rewind would put it — nothing to do,
            // and nothing worth warning about either.
            if current == file.hash_before {
                continue;
            }
            if current != file.hash_after {
                let kind = if let Some(turn) = later.get(&file.path) {
                    ConflictKind::OverwrittenByTurn { turn: *turn }
                } else if current.is_none() {
                    ConflictKind::Deleted
                } else {
                    ConflictKind::EditedOutsideSmith
                };
                report.conflicts.push(RewindConflict {
                    path: file.path.clone(),
                    kind,
                });
            }

            // `existed_before`, not `hash_before.is_some()`: the flag is the
            // record of which of the two restore shapes this file needs, and
            // deriving it from the hash would make a future entry with a hash
            // it could not store silently turn into a deletion.
            match (file.existed_before, &file.hash_before) {
                (true, Some(hash)) => {
                    report.restore.push(file.path.clone());
                    steps.push(Step::Restore {
                        path: full,
                        hash: hash.clone(),
                        display: file.path.clone(),
                    });
                }
                (false, _) => {
                    report.delete.push(file.path.clone());
                    steps.push(Step::Delete {
                        path: full,
                        display: file.path.clone(),
                    });
                }
                // Existed, but we have no bytes for it — a manifest edited by
                // hand, or a truncated write. Restoring "nothing" over a real
                // file would be data loss, so it is reported and skipped.
                (true, None) => report.notes.push(format!(
                    "SKIPPED {}: the checkpoint records that it existed but holds no copy of it",
                    file.path
                )),
            }
        }

        if !steps.is_empty() {
            report.status = RewindStatus::Preview;
        }
        (report, steps)
    }

    // ---- applying ----------------------------------------------------------

    /// Restores the files `turn` touched. Blocked, with nothing changed, if
    /// any of them holds work this checkpoint has no copy of — unless `force`.
    pub async fn apply(&self, session_id: &str, turn: Option<u64>, force: bool) -> RewindReport {
        let (mut report, steps) = self.plan_steps(session_id, turn).await;
        if steps.is_empty() {
            return report;
        }
        if !report.conflicts.is_empty() && !force {
            report.status = RewindStatus::Blocked;
            return report;
        }

        // Pre-flight. Every byte we intend to write is read *before* the first
        // one is written, so a missing or corrupt object aborts with the
        // filesystem still untouched instead of halfway through.
        let mut blobs: HashMap<String, Vec<u8>> = HashMap::new();
        for step in &steps {
            let Step::Restore { hash, display, .. } = step else {
                continue;
            };
            if blobs.contains_key(hash) {
                continue;
            }
            match self.get_object(hash).await {
                Ok(bytes) => {
                    blobs.insert(hash.clone(), bytes);
                }
                Err(e) => {
                    report.status = RewindStatus::Blocked;
                    report.notes.push(format!(
                        "the stored copy of {display} is unreadable ({e}) — refusing to apply \
                         half a rewind; nothing was changed"
                    ));
                    return report;
                }
            }
        }

        // The honest limit on atomicity: N files cannot be replaced as one
        // operation on a POSIX filesystem, and pretending otherwise would be a
        // lie. What we can guarantee is that the *current* state is never
        // lost — it goes into the object store as its own checkpoint first, so
        // an interrupted rewind is recorded on both sides and reversible,
        // rather than being half-applied with no record.
        let safety = self.begin_turn(session_id).await;
        let rewound = report.turn.unwrap_or_default();
        self.set_note(
            session_id,
            safety,
            format!("the state of the project just before /rewind {rewound}"),
        )
        .await;
        for step in &steps {
            let _ = self.snapshot_before(session_id, safety, step.path()).await;
        }

        let mut restored = Vec::new();
        let mut deleted = Vec::new();
        for step in &steps {
            let outcome = match step {
                Step::Restore { path, hash, .. } => {
                    restore_file(path, blobs.get(hash).map(Vec::as_slice).unwrap_or_default()).await
                }
                Step::Delete { path, .. } => self.remove_file(path).await,
            };
            match (outcome, step) {
                (Ok(()), Step::Restore { display, .. }) => restored.push(display.clone()),
                (Ok(()), Step::Delete { display, .. }) => deleted.push(display.clone()),
                (Err(e), step) => {
                    // Listed as a failure rather than silently dropped: a
                    // rewind that half worked has to say which half.
                    let display = match step {
                        Step::Restore { display, .. } | Step::Delete { display, .. } => display,
                    };
                    report.notes.push(format!("FAILED {display}: {e}"));
                }
            }
        }
        for step in &steps {
            let _ = self.snapshot_after(session_id, safety, step.path()).await;
        }

        // Rewritten from what actually happened, not from what was planned.
        report.restore = restored;
        report.delete = deleted;
        report.status = RewindStatus::Applied;
        report.notes.push(format!(
            "the previous state was checkpointed as turn {safety} — `/rewind {safety} confirm` \
             puts it back"
        ));
        report
    }

    /// Deletes a file the turn created, then prunes directories it left empty.
    /// Never climbs to or above the project root.
    async fn remove_file(&self, path: &Path) -> Result<(), String> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
        let mut dir = path.parent().map(Path::to_path_buf);
        while let Some(current) = dir {
            if current == self.root || !current.starts_with(&self.root) {
                break;
            }
            if tokio::fs::remove_dir(&current).await.is_err() {
                break; // not empty, which is the normal case
            }
            dir = current.parent().map(Path::to_path_buf);
        }
        Ok(())
    }

    async fn set_note(&self, session_id: &str, seq: u64, note: String) {
        let manifest = {
            let mut state = self.state.lock().await;
            let manifest = state
                .manifests
                .entry((session_id.to_string(), seq))
                .or_insert_with(|| Manifest::new(seq));
            manifest.note = Some(note);
            manifest.clone()
        };
        let _ = self.write_manifest(session_id, &manifest).await;
    }

    // ---- garbage collection ------------------------------------------------

    /// Drops checkpoints older than `ttl`, then any object no surviving
    /// manifest still refers to. Returns `(manifests removed, objects removed)`.
    ///
    /// Best effort throughout: every filesystem error is skipped rather than
    /// propagated, because a sweep that fails loudly is a sweep that stops a
    /// session from starting.
    pub async fn sweep(&self, ttl: Duration) -> (usize, usize) {
        let cutoff = now_millis().saturating_sub(ttl.as_millis() as i64);
        let mut manifests_removed = 0usize;

        let turns_root = self.base().join("turns");
        let mut sessions = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&turns_root).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                sessions.push(entry.path());
            }
        }

        let mut live: HashSet<String> = HashSet::new();
        for session in sessions {
            let mut kept = 0usize;
            let Ok(mut entries) = tokio::fs::read_dir(&session).await else {
                continue;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let Ok(bytes) = tokio::fs::read(&path).await else {
                    continue;
                };
                let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes) else {
                    continue;
                };
                if manifest.created_at < cutoff {
                    if tokio::fs::remove_file(&path).await.is_ok() {
                        manifests_removed += 1;
                    }
                    continue;
                }
                kept += 1;
                for file in manifest.files {
                    live.extend(file.hash_before);
                    live.extend(file.hash_after);
                }
            }
            if kept == 0 {
                let _ = tokio::fs::remove_dir(&session).await;
            }
        }

        let mut objects_removed = 0usize;
        let Ok(mut shards) = tokio::fs::read_dir(self.objects_dir()).await else {
            return (manifests_removed, objects_removed);
        };
        while let Ok(Some(shard)) = shards.next_entry().await {
            let Ok(mut objects) = tokio::fs::read_dir(shard.path()).await else {
                continue;
            };
            while let Ok(Some(object)) = objects.next_entry().await {
                let path = object.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if live.contains(name) {
                    continue;
                }
                // The age guard is not belt-and-braces: a turn running right
                // now writes its object *before* it flushes the manifest that
                // references it, so a sweep in that window would see an
                // unreferenced object that is about to be needed. Nothing
                // younger than the TTL is ever a candidate, which closes it.
                if !older_than(&path, ttl).await {
                    continue;
                }
                if tokio::fs::remove_file(&path).await.is_ok() {
                    objects_removed += 1;
                }
            }
            let _ = tokio::fs::remove_dir(shard.path()).await;
        }
        (manifests_removed, objects_removed)
    }

    /// Drops every checkpoint for one session — for the session-delete path.
    /// The objects it referenced are reclaimed by the next `sweep`, since
    /// another session may share them.
    pub async fn purge_session(&self, session_id: &str) {
        let _ = tokio::fs::remove_dir_all(self.session_dir(session_id)).await;
        let mut state = self.state.lock().await;
        state.manifests.retain(|(id, _), _| id != session_id);
    }
}

#[async_trait]
impl Checkpointer for CheckpointStore {
    async fn begin_turn(&self, session_id: &str) -> u64 {
        // Read the disk *before* taking the lock, so a resumed session
        // continues after its stored turns rather than overwriting them.
        let stored = self.latest_turn(session_id).await.unwrap_or(0);
        let mut state = self.state.lock().await;
        let last = state.last_seq.entry(session_id.to_string()).or_insert(0);
        let next = (*last).max(stored) + 1;
        *last = next;
        next
    }

    async fn snapshot_before(
        &self,
        session_id: &str,
        turn: u64,
        path: &Path,
    ) -> Result<(), String> {
        let key = self.path_key(path);
        {
            let state = self.state.lock().await;
            if let Some(manifest) = state.manifests.get(&(session_id.to_string(), turn)) {
                // First write wins: a turn's "before" is the state at the
                // turn's start, not before its second edit to the same file.
                if manifest.files.iter().any(|f| f.path == key) {
                    return Ok(());
                }
            }
        }

        let existing = match tokio::fs::read(path).await {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        let hash_before = match existing {
            Some(bytes) => Some(self.put_object(&bytes).await?),
            None => None,
        };

        let manifest = {
            let mut state = self.state.lock().await;
            let manifest = state
                .manifests
                .entry((session_id.to_string(), turn))
                .or_insert_with(|| Manifest::new(turn));
            if manifest.files.iter().any(|f| f.path == key) {
                return Ok(());
            }
            manifest.files.push(FileEntry {
                path: key,
                existed_before: hash_before.is_some(),
                hash_before,
                hash_after: None,
            });
            manifest.clone()
        };
        self.write_manifest(session_id, &manifest).await
    }

    async fn snapshot_after(&self, session_id: &str, turn: u64, path: &Path) -> Result<(), String> {
        let key = self.path_key(path);
        let current = match tokio::fs::read(path).await {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("could not read {}: {e}", path.display())),
        };
        let hash_after = match current {
            Some(bytes) => Some(self.put_object(&bytes).await?),
            None => None,
        };

        let manifest = {
            let mut state = self.state.lock().await;
            let Some(manifest) = state.manifests.get_mut(&(session_id.to_string(), turn)) else {
                return Ok(());
            };
            let Some(index) = manifest.files.iter().position(|f| f.path == key) else {
                return Ok(());
            };
            if manifest.files[index].hash_before == hash_after {
                // The call changed nothing — a refused `edit_file`, a write of
                // identical content. Dropping the entry keeps a preview from
                // offering to "restore" a file nobody touched.
                manifest.files.remove(index);
            } else {
                manifest.files[index].hash_after = hash_after;
            }
            manifest.clone()
        };
        self.write_manifest(session_id, &manifest).await
    }

    async fn note_uncovered(&self, session_id: &str, turn: u64, tool: &str) -> Result<(), String> {
        let manifest = {
            let mut state = self.state.lock().await;
            let manifest = state
                .manifests
                .entry((session_id.to_string(), turn))
                .or_insert_with(|| Manifest::new(turn));
            manifest.uncovered.push(tool.to_string());
            manifest.clone()
        };
        self.write_manifest(session_id, &manifest).await
    }
}

// ---- helpers ---------------------------------------------------------------

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// The hash of a file's current contents, or `None` if it isn't there. Any
/// other read error also reads as `None`: a file we cannot read is one we
/// cannot compare, and treating that as "unchanged" would be the dangerous
/// direction to guess in.
async fn hash_of(path: &Path) -> Option<String> {
    tokio::fs::read(path).await.ok().map(|b| hash_bytes(&b))
}

/// Write via a temporary file in the *same directory* and a rename, so a
/// reader never observes a half-written file and a crash leaves either the old
/// bytes or the new ones. Same directory because rename is only atomic within
/// a filesystem.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("path has no parent directory")?;
    let temp = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        now_millis() as u64 ^ (bytes.len() as u64)
    ));
    tokio::fs::write(&temp, bytes)
        .await
        .map_err(|e| format!("{}: {e}", temp.display()))?;
    match tokio::fs::rename(&temp, path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp).await;
            Err(format!("{}: {e}", path.display()))
        }
    }
}

async fn restore_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    write_atomic(path, bytes).await
}

async fn older_than(path: &Path, ttl: Duration) -> bool {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|elapsed| elapsed > ttl)
        .unwrap_or(false)
}

/// Counts each tool name, ordered by name so two runs of the same turn render
/// identically.
fn tally(names: &[String]) -> Vec<(String, u32)> {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
    for name in names {
        *counts.entry(name.as_str()).or_default() += 1;
    }
    counts
        .into_iter()
        .map(|(name, count)| (name.to_string(), count))
        .collect()
}

/// Session ids are uuids or `local-<pid>`, but they arrive as a `String` from
/// outside — refusing to let one become a path traversal costs one line.
fn sanitize(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const SESSION: &str = "sess-1";

    fn store(dir: &TempDir) -> CheckpointStore {
        CheckpointStore::new(dir.path())
    }

    async fn write(dir: &TempDir, rel: &str, content: &str) -> PathBuf {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(&path, content).await.unwrap();
        path
    }

    /// One turn's worth of activity: snapshot, mutate, snapshot again.
    async fn turn_edits(store: &CheckpointStore, seq: u64, path: &Path, new_content: &str) {
        store.snapshot_before(SESSION, seq, path).await.unwrap();
        tokio::fs::write(path, new_content).await.unwrap();
        store.snapshot_after(SESSION, seq, path).await.unwrap();
    }

    #[tokio::test]
    async fn a_modified_file_is_restored_to_its_exact_prior_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        // Deliberately not valid UTF-8-friendly text only: the store handles
        // bytes, and a restore that round-trips through a string would corrupt
        // anything that isn't clean text.
        let path = write(&dir, "src/main.rs", "fn main() { /* ünïcode */ }\n").await;

        let seq = store.begin_turn(SESSION).await;
        turn_edits(&store, seq, &path, "fn main() { panic!() }\n").await;

        let report = store.apply(SESSION, Some(seq), false).await;
        assert_eq!(report.status, RewindStatus::Applied, "{report:?}");
        assert_eq!(report.restore, vec!["src/main.rs".to_string()]);
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "fn main() { /* ünïcode */ }\n"
        );
    }

    /// The whole reason `existed_before` is a field: putting back "no file" is
    /// a state, and leaving the file behind is a wrong answer, not a partial
    /// one.
    #[tokio::test]
    async fn a_newly_created_file_is_deleted_on_restore() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = dir.path().join("src/new.rs");

        let seq = store.begin_turn(SESSION).await;
        store.snapshot_before(SESSION, seq, &path).await.unwrap();
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, "brand new").await.unwrap();
        store.snapshot_after(SESSION, seq, &path).await.unwrap();

        let report = store.apply(SESSION, Some(seq), false).await;
        assert_eq!(report.status, RewindStatus::Applied, "{report:?}");
        assert_eq!(report.delete, vec!["src/new.rs".to_string()]);
        assert!(!path.exists(), "the created file survived the rewind");
        // The directory the turn created goes with it, rather than being left
        // as an empty husk.
        assert!(!dir.path().join("src").exists());
    }

    #[tokio::test]
    async fn a_file_hand_edited_after_the_turn_is_detected_and_reported_before_being_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = write(&dir, "notes.md", "original\n").await;

        let seq = store.begin_turn(SESSION).await;
        turn_edits(&store, seq, &path, "agent wrote this\n").await;

        // The user then does an hour of work on top of the agent's edit.
        tokio::fs::write(&path, "agent wrote this\nand I added this by hand\n")
            .await
            .unwrap();

        let preview = store.plan(SESSION, Some(seq)).await;
        assert_eq!(preview.conflicts.len(), 1, "{preview:?}");
        assert_eq!(preview.conflicts[0].path, "notes.md");
        assert_eq!(
            preview.conflicts[0].kind,
            ConflictKind::EditedOutsideSmith,
            "{preview:?}"
        );

        // Default refuses, and the hand-written line is still there.
        let blocked = store.apply(SESSION, Some(seq), false).await;
        assert_eq!(blocked.status, RewindStatus::Blocked);
        assert!(tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .contains("by hand"));
        assert!(blocked.lines().join("\n").contains("--force"));

        // ...and --force is the only way past it.
        let forced = store.apply(SESSION, Some(seq), true).await;
        assert_eq!(forced.status, RewindStatus::Applied);
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "original\n"
        );
    }

    /// A later turn touching the same file is the same observation as a hand
    /// edit but a different cause, and the user needs to be told which.
    #[tokio::test]
    async fn a_file_a_later_turn_rewrote_names_that_turn_instead_of_blaming_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = write(&dir, "a.txt", "v1").await;

        let first = store.begin_turn(SESSION).await;
        turn_edits(&store, first, &path, "v2").await;
        let second = store.begin_turn(SESSION).await;
        turn_edits(&store, second, &path, "v3").await;

        let preview = store.plan(SESSION, Some(first)).await;
        assert_eq!(
            preview.conflicts[0].kind,
            ConflictKind::OverwrittenByTurn { turn: second },
            "{preview:?}"
        );
    }

    /// Content addressing earns its keep here: the same bytes snapshotted by
    /// two turns must occupy the store once.
    #[tokio::test]
    async fn identical_content_across_two_turns_stores_one_object() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let a = write(&dir, "a.txt", "shared body").await;
        let b = write(&dir, "b.txt", "shared body").await;

        let seq = store.begin_turn(SESSION).await;
        turn_edits(&store, seq, &a, "changed a").await;
        turn_edits(&store, seq, &b, "changed b").await;
        let next = store.begin_turn(SESSION).await;
        turn_edits(&store, next, &a, "shared body").await;

        // "shared body" was snapshotted three times (a-before, b-before,
        // a-after) and stored once.
        let objects = count_objects(&store).await;
        let distinct = ["shared body", "changed a", "changed b"].len();
        assert_eq!(
            objects, distinct,
            "expected one object per distinct content, found {objects}"
        );
    }

    async fn count_objects(store: &CheckpointStore) -> usize {
        let mut total = 0;
        let Ok(mut shards) = tokio::fs::read_dir(store.objects_dir()).await else {
            return 0;
        };
        while let Ok(Some(shard)) = shards.next_entry().await {
            let mut objects = tokio::fs::read_dir(shard.path()).await.unwrap();
            while let Ok(Some(_)) = objects.next_entry().await {
                total += 1;
            }
        }
        total
    }

    #[tokio::test]
    async fn restore_of_a_turn_that_touched_nothing_is_a_clean_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        write(&dir, "untouched.txt", "still here").await;

        let seq = store.begin_turn(SESSION).await;
        let report = store.apply(SESSION, Some(seq), false).await;

        assert_eq!(report.status, RewindStatus::Nothing);
        assert!(!report.touches_files());
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("untouched.txt"))
                .await
                .unwrap(),
            "still here"
        );
        // And no checkpoint directory was conjured up for a turn with nothing
        // in it.
        assert!(!store.manifest_path(SESSION, seq).exists());
    }

    /// A session that never ran a mutating tool must not report a mysterious
    /// empty plan.
    #[tokio::test]
    async fn a_session_with_no_checkpoints_reports_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let report = store(&dir).apply("never-ran", None, false).await;
        assert_eq!(report.status, RewindStatus::Nothing);
        assert_eq!(report.turn, None);
    }

    /// A tool call that changes nothing (a refused `edit_file`) must not leave
    /// a phantom entry offering to restore a file nobody touched.
    #[tokio::test]
    async fn a_tool_call_that_changed_nothing_drops_out_of_the_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = write(&dir, "a.txt", "unchanged").await;

        let seq = store.begin_turn(SESSION).await;
        store.snapshot_before(SESSION, seq, &path).await.unwrap();
        // The tool failed; the file is exactly as it was.
        store.snapshot_after(SESSION, seq, &path).await.unwrap();

        let report = store.plan(SESSION, Some(seq)).await;
        assert_eq!(report.status, RewindStatus::Nothing, "{report:?}");
        assert!(!store.manifest_path(SESSION, seq).exists());
    }

    /// A turn's "before" is the state at its start — two edits to one file in
    /// one turn must rewind to the state before the *first*.
    #[tokio::test]
    async fn two_edits_to_one_file_in_a_turn_rewind_to_the_state_before_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = write(&dir, "a.txt", "v0").await;

        let seq = store.begin_turn(SESSION).await;
        turn_edits(&store, seq, &path, "v1").await;
        turn_edits(&store, seq, &path, "v2").await;

        store.apply(SESSION, Some(seq), false).await;
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "v0");
    }

    /// Applying takes a checkpoint of the pre-rewind state, so an unwanted
    /// rewind is itself undoable — the answer to "a failure halfway must not
    /// leave files reverted with no record".
    #[tokio::test]
    async fn a_rewind_is_itself_checkpointed_and_can_be_rewound() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = write(&dir, "a.txt", "before").await;

        let seq = store.begin_turn(SESSION).await;
        turn_edits(&store, seq, &path, "after").await;
        let undo = store.apply(SESSION, Some(seq), false).await;
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "before");

        let safety = store.latest_turn(SESSION).await.unwrap();
        assert!(safety > seq, "the rewind recorded no safety checkpoint");
        assert!(undo.notes.iter().any(|n| n.contains(&safety.to_string())));

        let redo = store.apply(SESSION, Some(safety), false).await;
        assert_eq!(redo.status, RewindStatus::Applied, "{redo:?}");
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "after");
    }

    /// A missing object aborts before the first byte is written, rather than
    /// reverting some files and giving up on the rest.
    #[tokio::test]
    async fn a_missing_object_blocks_the_whole_rewind_instead_of_applying_part_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let a = write(&dir, "a.txt", "a-before").await;
        let b = write(&dir, "b.txt", "b-before").await;

        let seq = store.begin_turn(SESSION).await;
        turn_edits(&store, seq, &a, "a-after").await;
        turn_edits(&store, seq, &b, "b-after").await;

        // Corrupt the store: delete the object holding a.txt's prior bytes.
        let hash = hash_bytes(b"a-before");
        tokio::fs::remove_file(store.object_path(&hash))
            .await
            .unwrap();

        let report = store.apply(SESSION, Some(seq), false).await;
        assert_eq!(report.status, RewindStatus::Blocked, "{report:?}");
        // Neither file moved — not even the one whose object is intact.
        assert_eq!(tokio::fs::read_to_string(&a).await.unwrap(), "a-after");
        assert_eq!(tokio::fs::read_to_string(&b).await.unwrap(), "b-after");
    }

    /// The gap has to reach the user, and the only place it can come from is
    /// the manifest.
    #[tokio::test]
    async fn a_turn_that_ran_run_bash_reports_the_uncovered_calls() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = write(&dir, "a.txt", "v0").await;

        let seq = store.begin_turn(SESSION).await;
        turn_edits(&store, seq, &path, "v1").await;
        store
            .note_uncovered(SESSION, seq, "run_bash")
            .await
            .unwrap();
        store
            .note_uncovered(SESSION, seq, "run_bash")
            .await
            .unwrap();

        let report = store.plan(SESSION, Some(seq)).await;
        assert_eq!(report.uncovered, vec![("run_bash".to_string(), 2)]);
        assert!(report.lines().join("\n").contains("NOT COVERED"));
    }

    /// A turn whose *only* mutating call was `run_bash` has nothing to restore
    /// — and must still warn rather than reporting a tidy success.
    #[tokio::test]
    async fn a_shell_only_turn_has_nothing_to_restore_but_still_warns() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let seq = store.begin_turn(SESSION).await;
        store
            .note_uncovered(SESSION, seq, "run_bash")
            .await
            .unwrap();

        let report = store.apply(SESSION, Some(seq), false).await;
        assert_eq!(report.status, RewindStatus::Nothing);
        assert!(report.lines().join("\n").contains("NOT COVERED"));
    }

    #[tokio::test]
    async fn a_resumed_session_continues_numbering_after_its_stored_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "a.txt", "v0").await;
        {
            let store = store(&dir);
            let seq = store.begin_turn(SESSION).await;
            turn_edits(&store, seq, &path, "v1").await;
            assert_eq!(seq, 1);
        }
        // A new process over the same project directory.
        let store = store(&dir);
        assert_eq!(store.begin_turn(SESSION).await, 2);
    }

    #[tokio::test]
    async fn sweeping_drops_expired_manifests_and_the_objects_only_they_referenced() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = write(&dir, "a.txt", "v0").await;
        let seq = store.begin_turn(SESSION).await;
        turn_edits(&store, seq, &path, "v1").await;

        // Nothing is old enough yet.
        assert_eq!(store.sweep(DEFAULT_TTL).await, (0, 0));
        assert!(store.manifest_path(SESSION, seq).exists());

        // A zero TTL makes everything expired, which is the same code path a
        // fortnight would take.
        let (manifests, objects) = store.sweep(Duration::ZERO).await;
        assert_eq!(manifests, 1);
        assert_eq!(objects, 2, "both v0 and v1 should have been reclaimed");
        assert!(!store.manifest_path(SESSION, seq).exists());
        assert_eq!(count_objects(&store).await, 0);
        // The user's actual file is untouched by garbage collection.
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "v1");
    }

    #[tokio::test]
    async fn purging_a_session_leaves_other_sessions_intact() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = write(&dir, "a.txt", "v0").await;
        let mine = store.begin_turn(SESSION).await;
        turn_edits(&store, mine, &path, "v1").await;
        store.snapshot_before("other", 1, &path).await.unwrap();
        store.snapshot_after("other", 1, &path).await.unwrap();
        store.note_uncovered("other", 1, "run_bash").await.unwrap();

        store.purge_session(SESSION).await;
        assert_eq!(store.latest_turn(SESSION).await, None);
        assert_eq!(store.latest_turn("other").await, Some(1));
    }

    /// A hostile or merely odd session id must not escape the checkpoint
    /// directory.
    #[test]
    fn a_session_id_cannot_traverse_out_of_the_checkpoint_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(&dir);
        let path = store.session_dir("../../etc");
        assert!(path.starts_with(store.base().join("turns")));
        assert!(!path.to_string_lossy().contains(".."));
    }

    #[test]
    fn tally_counts_each_tool_and_orders_stably() {
        let names = vec![
            "run_bash".to_string(),
            "mcp__x__y".to_string(),
            "run_bash".to_string(),
        ];
        assert_eq!(
            tally(&names),
            vec![("mcp__x__y".to_string(), 1), ("run_bash".to_string(), 2)]
        );
    }
}
