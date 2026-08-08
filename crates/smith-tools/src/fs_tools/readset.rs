//! What this session has actually read — the read-before-overwrite gate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What the model has actually been shown of a file, so `write_file` cannot
/// replace a file it has never seen.
///
/// # Where this state lives, and why
///
/// It cannot live on `ToolContext`: that is cloned for every call
/// (`with_progress` stamps the current call onto a copy), so anything a tool
/// recorded through its clone would die with the call. It cannot be a plain
/// field on `ToolRegistry` either — the registry is held behind an `Arc` and
/// `ToolExecutor::execute` takes `&self`, so there is no `&mut` to record
/// through. What does outlive a call is the tool objects themselves, so the
/// set is built once in `ToolRegistry::with_builtin_tools` and `Arc`-shared
/// between the four file tools, with the mutation behind a `Mutex`.
///
/// A `std::sync::Mutex` rather than tokio's, deliberately: no lock here is
/// ever held across an `.await` — every method is synchronous, does one map
/// lookup and merges a short vector — so an async mutex would only add a
/// scheduling point. That matters now that `ReadOnly` calls run concurrently
/// (`Agent::run_concurrent_group`): several `read_file`s really do record at
/// the same instant, and each one is a whole critical section, so no
/// interleaving can produce a half-updated entry. A poisoned lock is
/// recovered from rather than propagated — a panic in some other tool must
/// not make every later file operation fail.
///
/// Keyed by session as well as path, so a second session sharing one registry
/// (a subagent, a resumed run) starts out knowing nothing rather than
/// inheriting another session's reads.
#[derive(Debug, Default)]
pub struct ReadSet {
    seen: Mutex<HashMap<(String, PathBuf), Seen>>,
}

/// What was shown of one file, pinned to the exact bytes it was shown from.
#[derive(Debug)]
struct Seen {
    /// sha256 of the file's contents at the moment those lines were read.
    /// Knowledge is only knowledge of *these* bytes.
    hash: String,
    /// `0` means "known in full without reference to lines" — an empty file,
    /// a binary `read_file` answered for, or content the model just wrote
    /// itself.
    total_lines: usize,
    /// Half-open, 0-based line ranges, sorted and merged.
    covered: Vec<(usize, usize)>,
    /// Set when a read of these bytes happened but was not a faithful view.
    /// It contributes no coverage — it exists only so the refusal can say
    /// *that*, instead of claiming the file was never read.
    unfaithful: Option<Unfaithful>,
}

/// Why a read showed the model something the file does not contain.
///
/// Re-reading cannot fix either of these — the same call produces the same
/// view — which is exactly why they need their own refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unfaithful {
    /// At least one line ran past the per-line cap and was cut.
    ClippedLines,
    /// The bytes are not valid UTF-8, so what was shown is a reconstruction.
    LossyDecode,
}

/// How much of a file the model can be said to know right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knowledge {
    /// Read in full, and the bytes on disk are still those bytes.
    Whole,
    /// Read from the top but not to the end.
    Partial { read_to: usize, total: usize },
    /// Read, but the file has changed since.
    Stale,
    /// Read, but what was shown was not what the file contains.
    Unfaithful(Unfaithful),
    /// Never read this session, or read only from the middle.
    Unread,
}

impl Seen {
    fn new(hash: &str, total_lines: usize) -> Self {
        Self {
            hash: hash.to_string(),
            total_lines,
            covered: Vec::new(),
            unfaithful: None,
        }
    }

    fn is_whole(&self) -> bool {
        self.total_lines == 0
            || matches!(self.covered.first(), Some(&(0, end)) if end >= self.total_lines)
    }

    /// Adds one range and re-merges. Kept merged on every insert so
    /// `is_whole` only ever has to look at the first range.
    fn insert(&mut self, range: (usize, usize)) {
        self.covered.push(range);
        self.covered.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(self.covered.len());
        for (start, end) in self.covered.drain(..) {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        self.covered = merged;
    }
}

impl ReadSet {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<(String, PathBuf), Seen>> {
        // A poisoned lock still holds a consistent map — every critical
        // section here is a single infallible mutation — so the guard is
        // taken back rather than turned into a panic that would spread to
        // every subsequent file tool call.
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records that lines `range` (half-open, 0-based) of the content hashing
    /// to `hash` were shown to the model.
    pub fn record_read(
        &self,
        session: &str,
        path: &Path,
        hash: &str,
        total_lines: usize,
        range: (usize, usize),
    ) {
        let mut seen = self.entries();
        let entry = seen
            .entry((session.to_string(), path.to_path_buf()))
            .or_insert_with(|| Seen::new(hash, total_lines));
        // Different bytes than last time: ranges recorded against the old
        // content describe lines that may no longer exist, so they are
        // dropped rather than merged into the new content's coverage.
        if entry.hash != hash {
            *entry = Seen::new(hash, total_lines);
        }
        entry.insert(range);
    }

    /// Records that `path` was read but that the view was not faithful, so
    /// the read counts for nothing *and the refusal can say why*.
    ///
    /// Before this existed such a read simply recorded nothing, and
    /// `write_file` then answered "has not been read this session" — to a
    /// model that had just read it. The advice in that message is to call
    /// `read_file`, which produces the identical clipped view, so the pair
    /// loops until the turn's tool budget runs out. Observed in the wild on a
    /// dashboard component with one very long line.
    ///
    /// An existing faithful reading is never downgraded: coverage already
    /// recorded stays, and `knowledge` only consults this flag when there is
    /// no coverage to consult.
    pub fn record_unfaithful(
        &self,
        session: &str,
        path: &Path,
        hash: &str,
        total_lines: usize,
        reason: Unfaithful,
    ) {
        let mut seen = self.entries();
        let entry = seen
            .entry((session.to_string(), path.to_path_buf()))
            .or_insert_with(|| Seen::new(hash, total_lines));
        if entry.hash != hash {
            *entry = Seen::new(hash, total_lines);
        }
        entry.unfaithful = Some(reason);
    }

    /// Records `path` as known in full, for content there is nothing left to
    /// learn about: what `write_file` just wrote, or what `read_file`
    /// described in full (an empty or binary file).
    pub fn record_whole(&self, session: &str, path: &Path, hash: &str) {
        self.entries().insert(
            (session.to_string(), path.to_path_buf()),
            Seen::new(hash, 0),
        );
    }

    /// Moves knowledge of a file across a change the model itself made, and
    /// only across that: an `edit_file` that matched `old_str` proves the
    /// model knew *that snippet*, never the rest of the file, so this
    /// refreshes an existing whole-file reading and never creates one.
    pub fn carry_forward(&self, session: &str, path: &Path, from_hash: &str, to_hash: &str) {
        let mut seen = self.entries();
        let key = (session.to_string(), path.to_path_buf());
        if seen
            .get(&key)
            .is_some_and(|e| e.hash == from_hash && e.is_whole())
        {
            seen.insert(key, Seen::new(to_hash, 0));
        }
    }

    /// What the model knows about the file whose current contents hash to
    /// `current_hash`.
    pub fn knowledge(&self, session: &str, path: &Path, current_hash: &str) -> Knowledge {
        let seen = self.entries();
        let Some(entry) = seen.get(&(session.to_string(), path.to_path_buf())) else {
            return Knowledge::Unread;
        };
        if entry.hash != current_hash {
            return Knowledge::Stale;
        }
        if entry.is_whole() {
            return Knowledge::Whole;
        }
        match entry.covered.first() {
            Some(&(0, read_to)) => Knowledge::Partial {
                read_to,
                total: entry.total_lines,
            },
            // No coverage from the top. Say *why* if we know — a clipped or
            // lossy read is a dead end that another read_file will not open,
            // and telling the model to read again is what put it in a loop.
            _ => match entry.unfaithful {
                Some(reason) => Knowledge::Unfaithful(reason),
                // Read only from the middle: no more use than never having
                // read it, and there is no single offset to recommend.
                None => Knowledge::Unread,
            },
        }
    }
}
