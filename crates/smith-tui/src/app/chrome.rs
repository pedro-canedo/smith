//! Frame furniture: config, overlays, sidebar tabs, idle hints.

use smith_core::{PermissionPolicy, Task};

use crate::keymap::KeyMap;
use crate::logbuf::LogBuffer;
use crate::slash::SlashRegistry;
use crate::theme::Theme;

use super::chatline::ChatLine;

/// What the idle screen shows below the hint line: either a generic tip, or —
/// once a project has prior history — a pointer to resume it.
#[derive(Debug, Clone)]
pub enum IdleHint {
    Tip(String),
    NewSession { title: String },
    ContinueSession { title: String, resume_cmd: String },
}

/// Everything the TUI needs to know that isn't part of the live event stream:
/// display labels, environment info, and (when resuming) prior transcript.
pub struct TuiConfig {
    pub banner: String,
    pub provider_label: String,
    pub model_label: String,
    pub cwd_display: String,
    pub git_branch: Option<String>,
    pub idle_hint: IdleHint,
    pub initial_lines: Vec<ChatLine>,
    pub permission_policy: PermissionPolicy,
    /// Theme selected by the CLI before the TUI starts. This is how flags such
    /// as `--ascii` affect glyph capability without mutating process env.
    pub theme: Theme,
    /// Loaded from `.smith/goal.md` at startup (if any).
    pub goal: Option<String>,
    /// Restored from a resumed session's last `write_tasks` call, if any.
    pub tasks: Vec<Task>,
    /// Custom slash commands discovered under `.smith/commands/` and
    /// `~/.smith/commands/`. Empty for a frontend that does not load them.
    pub commands: SlashRegistry,
    /// Key bindings for the remappable commands. `KeyMap::default()` is the
    /// set that used to be hardcoded.
    pub keys: KeyMap,
    /// Prompts already submitted in this project, most recent first. Empty
    /// for a fresh session; on `--resume` it is the resumed conversation's
    /// own user messages.
    pub history: Vec<String>,
    /// Shared ring the `tracing` subscriber writes into; `Ctrl+L` reads it.
    /// Default-constructed for a frontend that installs no subscriber, in
    /// which case the panel simply reports that it is empty.
    pub logs: LogBuffer,
    /// The web console's URL for this session (`--web`), token included.
    /// `None` — the default — draws nothing anywhere.
    pub console_url: Option<String>,
}

/// Prompts kept in the recall ring. Generous — an entry is a short string,
/// and the cost of forgetting the one you wanted is another round of typing.
pub const HISTORY_LIMIT: usize = 200;

/// Title of the `Ctrl+L` panel. A constant because the key toggles on it:
/// pressing `Ctrl+L` with `/usage` open should replace it, not close it.
pub const LOG_PANEL_TITLE: &str = "diagnostics";

/// One table row from string-ish cells.
pub(super) fn row<const N: usize>(cells: [&str; N]) -> Vec<String> {
    cells.iter().map(|c| c.to_string()).collect()
}

/// A read-only panel over the transcript: `/usage`, `/mcp`, the `Ctrl+L` log.
///
/// **Deliberately not a `Modal` variant.** `Modal` is a sum type precisely
/// because each of its variants owns a `oneshot` the agent loop is blocked on;
/// making this a fourth variant would mean `/usage` could replace a pending
/// permission prompt and strand the turn forever. An overlay owns nothing and
/// answers nobody, so it lives in its own field and yields to any real modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlay {
    pub title: String,
    pub body: OverlayBody,
    /// Footer rows, rendered under the body and never scrolled off — the key
    /// hints belong to the panel, not to its contents.
    pub footer: Vec<String>,
    pub scroll: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayBody {
    /// Column headers and rows. Widths are percentages of the inner width and
    /// are expected to sum to 100.
    Table {
        columns: Vec<String>,
        widths: Vec<u16>,
        rows: Vec<Vec<String>>,
    },
    /// Pre-formatted lines, for content that isn't a grid.
    Lines(Vec<String>),
}

impl Overlay {
    pub fn table(
        title: impl Into<String>,
        columns: &[&str],
        widths: &[u16],
        rows: Vec<Vec<String>>,
    ) -> Self {
        Self {
            title: title.into(),
            body: OverlayBody::Table {
                columns: columns.iter().map(|c| c.to_string()).collect(),
                widths: widths.to_vec(),
                rows,
            },
            footer: Vec::new(),
            scroll: 0,
        }
    }

    pub fn lines(title: impl Into<String>, lines: Vec<String>) -> Self {
        Self {
            title: title.into(),
            body: OverlayBody::Lines(lines),
            footer: Vec::new(),
            scroll: 0,
        }
    }

    pub fn with_footer(mut self, footer: Vec<String>) -> Self {
        self.footer = footer;
        self
    }

    /// Rows the body wants, so the panel can size itself and clamp scrolling.
    pub fn row_count(&self) -> usize {
        match &self.body {
            // +1 for the header row.
            OverlayBody::Table { rows, .. } => rows.len() + 1,
            OverlayBody::Lines(lines) => lines.len(),
        }
    }
}

/// Which section of the sidebar is on screen.
///
/// The sidebar used to stack every section at once, which is what made it the
/// first thing to overflow at 80x24: `SESSION`, the task checklist, `CONTEXT`
/// and the vitals together want more rows than a 24-row terminal has left
/// after the prompt and the status bar. Tabs trade "see everything, truncated"
/// for "see one thing, whole" — the right trade for a pane 28 columns wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarTab {
    #[default]
    Session,
    Tasks,
    Vitals,
}

impl SidebarTab {
    /// Tab order, which is also the `Tabs` widget's index order.
    pub const ALL: [SidebarTab; 3] = [SidebarTab::Session, SidebarTab::Tasks, SidebarTab::Vitals];

    pub fn title(self) -> &'static str {
        match self {
            SidebarTab::Session => "Session",
            SidebarTab::Tasks => "Tasks",
            SidebarTab::Vitals => "Vitals",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    pub(super) fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }
}
