//! Turn checkpoints — the recording contract `/rewind` restores from.
//!
//! A checkpoint is exactly one thing: **the bytes of the files a turn touched,
//! as they were before the turn**. Nothing about git, nothing about the
//! working tree's status. The implementation (`smith_tools::CheckpointStore`)
//! is content-addressed, so repeatedly snapshotting an unchanged file costs
//! one hash and no bytes.
//!
//! Only the *recording* half lives behind a trait, because that is the half
//! `Agent` has to reach during a turn and `smith-core` must not know where the
//! bytes end up. Planning and applying a rewind happen outside the turn loop,
//! in the orchestrator, against the concrete store — there is nothing for the
//! agent to abstract over there.

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Records what a turn is about to overwrite.
///
/// **Every method is best-effort and must never be treated as fatal.** The
/// agent logs a failure and runs the tool anyway: losing the ability to undo a
/// write is bad, but refusing to do the work because we couldn't prepare to
/// undo it is worse.
#[async_trait]
pub trait Checkpointer: Send + Sync {
    /// Allocates the sequence number for a turn that is about to start.
    ///
    /// Infallible by signature because there is nothing useful a caller could
    /// do with an error at this point — a store that cannot allocate will
    /// simply fail every snapshot that follows, which the caller already
    /// tolerates.
    async fn begin_turn(&self, session_id: &str) -> u64;

    /// Captures `path` as it is *now*, before a tool modifies it. Called once
    /// per path per turn; later calls for a path already captured in this turn
    /// are ignored, because the turn's "before" is the state at its start.
    ///
    /// A path that does not exist is still recorded — as
    /// `existed_before = false`, which is what makes "delete it again" a
    /// restorable state rather than a missing one.
    async fn snapshot_before(&self, session_id: &str, turn: u64, path: &Path)
        -> Result<(), String>;

    /// Captures `path` as the tool left it.
    ///
    /// This is not redundant with `snapshot_before`: it is the only way to
    /// later tell "the user hand-edited this since" from "this is exactly what
    /// the turn wrote". A tool call that changed nothing (a failed
    /// `edit_file`, say) drops out of the checkpoint entirely here, so a
    /// preview never offers to restore a file that was never touched.
    async fn snapshot_after(&self, session_id: &str, turn: u64, path: &Path) -> Result<(), String>;

    /// Records that `tool` ran and could have changed things no snapshot
    /// covers — `run_bash` above all, and any MCP tool.
    ///
    /// The gap is real and unfixable; recording it is what lets `/rewind` say
    /// so out loud instead of implying it undid everything.
    async fn note_uncovered(&self, session_id: &str, turn: u64, tool: &str) -> Result<(), String>;
}

/// Why a file in a rewind plan is not in the state the turn left it in.
///
/// Every one of these means restoring would destroy something the checkpoint
/// does not have a copy of, which is why they block a rewind by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConflictKind {
    /// The bytes on disk are neither what the turn wrote nor what any later
    /// turn wrote — so a human, or a `run_bash` command, changed them.
    EditedOutsideSmith,
    /// A later checkpointed turn wrote this file too. Rewinding the earlier
    /// turn would silently undo the later one as well.
    OverwrittenByTurn { turn: u64 },
    /// The turn created or modified this file and it is now gone.
    Deleted,
}

impl ConflictKind {
    /// One clause, ready to append to a path in a user-facing line.
    pub fn describe(&self) -> String {
        match self {
            ConflictKind::EditedOutsideSmith => {
                "changed outside smith since that turn (a hand edit, or a run_bash command)".into()
            }
            ConflictKind::OverwrittenByTurn { turn } => {
                format!("also written by the later turn {turn}")
            }
            ConflictKind::Deleted => "has been deleted since that turn".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindConflict {
    /// Project-relative where possible, for display.
    pub path: String,
    pub kind: ConflictKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindStatus {
    /// Nothing was touched; this is what the turn *would* do.
    Preview,
    /// The files listed were restored/deleted.
    Applied,
    /// Refused, and nothing was touched — see `conflicts` and `notes`.
    Blocked,
    /// There is no checkpoint to rewind, or it has nothing to undo.
    Nothing,
}

/// What a rewind will do, or did. The same shape serves the preview and the
/// result deliberately: a user comparing "what you said you'd do" against
/// "what you did" should not have to compare two different renderings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindReport {
    /// The turn being rewound. `None` when there was no checkpoint at all.
    pub turn: Option<u64>,
    pub status: RewindStatus,
    /// Files that existed before the turn and will be (or were) put back.
    pub restore: Vec<String>,
    /// Files the turn created, which will be (or were) deleted — the whole
    /// reason `existed_before` is recorded per file.
    pub delete: Vec<String>,
    pub conflicts: Vec<RewindConflict>,
    /// Tools in that turn whose effects no checkpoint covers, with a count
    /// each. `run_bash` is the one that matters: it can write, move or delete
    /// anything, and nothing here undoes it.
    pub uncovered: Vec<(String, u32)>,
    /// Free-form lines to show verbatim: per-file failures, and the id of the
    /// safety checkpoint an apply took of the pre-rewind state.
    pub notes: Vec<String>,
}

impl RewindReport {
    /// A report for "there is nothing to rewind".
    pub fn nothing(turn: Option<u64>) -> Self {
        Self {
            turn,
            status: RewindStatus::Nothing,
            restore: Vec::new(),
            delete: Vec::new(),
            conflicts: Vec::new(),
            uncovered: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Whether this plan would actually change a file. A checkpoint can exist
    /// and still have nothing to do — every file already back at its pre-turn
    /// bytes, or a turn that only ran `run_bash`.
    pub fn touches_files(&self) -> bool {
        !self.restore.is_empty() || !self.delete.is_empty()
    }

    /// The report rendered as the lines a frontend should show, in order.
    ///
    /// Lives here rather than in each frontend because the wording is a safety
    /// property, not decoration: the `run_bash` caveat and the conflict list
    /// have to appear the same way in the TUI and in `stream-json`, and two
    /// copies drift.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let turn = match self.turn {
            Some(turn) => turn,
            None => {
                out.push("nothing to rewind — this session has no checkpoints yet".to_string());
                return out;
            }
        };

        match self.status {
            RewindStatus::Nothing => {
                out.push(format!("turn {turn} has nothing to undo — no file it wrote differs from its state before the turn"));
            }
            RewindStatus::Preview => {
                out.push(format!("rewind of turn {turn} would:"));
            }
            RewindStatus::Applied => {
                out.push(format!("rewound turn {turn}:"));
            }
            RewindStatus::Blocked => {
                out.push(format!(
                    "refusing to rewind turn {turn} — nothing was changed:"
                ));
            }
        }

        let verb_restore = if self.status == RewindStatus::Applied {
            "restored"
        } else {
            "restore"
        };
        for path in &self.restore {
            out.push(format!("  {verb_restore} {path}"));
        }
        let verb_delete = if self.status == RewindStatus::Applied {
            "deleted"
        } else {
            "delete"
        };
        for path in &self.delete {
            out.push(format!("  {verb_delete} {path} (created by that turn)"));
        }

        for conflict in &self.conflicts {
            out.push(format!(
                "  ! {} — {}",
                conflict.path,
                conflict.kind.describe()
            ));
        }
        if !self.conflicts.is_empty() && self.status != RewindStatus::Applied {
            out.push(
                "  those files hold work this checkpoint has no copy of; rewinding would \
                 destroy it. Re-run with --force to overwrite them anyway."
                    .to_string(),
            );
        }

        for (tool, count) in &self.uncovered {
            out.push(format!(
                "  NOT COVERED: that turn called {tool} {count}x. Shell commands can write, \
                 move or delete anything and smith cannot snapshot that — whatever those \
                 commands changed is untouched by this rewind."
            ));
        }

        for note in &self.notes {
            out.push(format!("  {note}"));
        }

        if self.status == RewindStatus::Preview && self.touches_files() {
            out.push(format!(
                "run `/rewind {turn} confirm` to apply this. Nothing has changed yet."
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(status: RewindStatus) -> RewindReport {
        RewindReport {
            turn: Some(4),
            status,
            restore: vec!["src/main.rs".into()],
            delete: vec!["src/new.rs".into()],
            conflicts: Vec::new(),
            uncovered: Vec::new(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn a_preview_says_nothing_has_changed_and_how_to_apply() {
        let text = report(RewindStatus::Preview).lines().join("\n");
        assert!(text.contains("would:"), "{text}");
        assert!(text.contains("restore src/main.rs"), "{text}");
        assert!(text.contains("delete src/new.rs"), "{text}");
        assert!(text.contains("Nothing has changed yet"), "{text}");
    }

    /// The applied rendering must be past tense throughout — a user scrolling
    /// back has to be able to tell a preview from a thing that happened.
    #[test]
    fn an_applied_report_reads_as_past_tense_and_offers_no_confirmation() {
        let text = report(RewindStatus::Applied).lines().join("\n");
        assert!(text.contains("rewound turn 4"), "{text}");
        assert!(text.contains("restored src/main.rs"), "{text}");
        assert!(text.contains("deleted src/new.rs"), "{text}");
        assert!(!text.contains("confirm"), "{text}");
    }

    /// The `run_bash` gap has to be stated in the user's face, not implied by
    /// its absence from the file list.
    #[test]
    fn an_uncovered_tool_is_called_out_in_plain_words() {
        let mut r = report(RewindStatus::Preview);
        r.uncovered = vec![("run_bash".into(), 2)];
        let text = r.lines().join("\n");
        assert!(text.contains("NOT COVERED"), "{text}");
        assert!(text.contains("run_bash 2x"), "{text}");
        assert!(text.contains("cannot snapshot"), "{text}");
    }

    #[test]
    fn a_blocked_report_says_nothing_was_changed_and_names_the_escape_hatch() {
        let mut r = report(RewindStatus::Blocked);
        r.conflicts = vec![RewindConflict {
            path: "src/main.rs".into(),
            kind: ConflictKind::EditedOutsideSmith,
        }];
        let text = r.lines().join("\n");
        assert!(text.contains("nothing was changed"), "{text}");
        assert!(text.contains("changed outside smith"), "{text}");
        assert!(text.contains("--force"), "{text}");
    }

    #[test]
    fn a_session_with_no_checkpoints_says_so_instead_of_rendering_an_empty_plan() {
        let text = RewindReport::nothing(None).lines().join("\n");
        assert_eq!(
            text,
            "nothing to rewind — this session has no checkpoints yet"
        );
    }

    #[test]
    fn a_conflict_naming_a_later_turn_names_it() {
        let kind = ConflictKind::OverwrittenByTurn { turn: 9 };
        assert!(kind.describe().contains("turn 9"));
    }
}
