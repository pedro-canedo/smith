//! The subcommands that never build an agent: sessions, remember, export.

use std::process::ExitCode;

use smith_config::Config;
use smith_store::SessionStore;

use headless::{EXIT_OK, EXIT_USAGE};

use super::*;

/// `smith sessions …` — list, delete, fork or export saved conversations.
///
/// Read-only by default and destructive only on an explicit `delete`; every
/// subcommand names the session it acted on, since ids are opaque and a
/// silent success on the wrong one is unrecoverable.
pub(crate) fn run_sessions(action: &SessionAction) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut store = match SessionStore::open(&cwd) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("smith: could not open this project's session store: {e}");
            return ExitCode::from(2);
        }
    };

    let result = match action {
        SessionAction::List { limit } => list_sessions(&store, *limit),
        SessionAction::Delete { id } => match store.delete_session(id) {
            Ok(true) => {
                println!("deleted {id}");
                Ok(())
            }
            Ok(false) => Err(format!("no session {id} in this project")),
            Err(e) => Err(e.to_string()),
        },
        SessionAction::Fork { id, through } => match store.fork_session(id, *through) {
            Ok(new_id) => {
                println!("forked {id} -> {new_id}");
                println!("resume it with: smith --resume {new_id}");
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        SessionAction::Export { id, format } => export_session(&store, id, *format),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("smith: {e}");
            ExitCode::from(2)
        }
    }
}

pub(crate) fn list_sessions(store: &SessionStore, limit: u32) -> Result<(), String> {
    let sessions = store
        .list_sessions(Some(limit))
        .map_err(|e| e.to_string())?;
    if sessions.is_empty() {
        println!("no saved sessions in this project");
        return Ok(());
    }
    for summary in sessions {
        let turns = store.turn_totals(&summary.id).map(|t| t.turns).unwrap_or(0);
        let last = store.last_seq(&summary.id).ok().flatten();
        // The seq range is printed because `sessions fork --through` is
        // meaningless without knowing what the numbers can be.
        let range = match last {
            Some(seq) => format!("seq 0..={seq}"),
            None => "empty".to_string(),
        };
        println!(
            "{}  {}  ({turns} turns, {range})",
            summary.id, summary.title
        );
    }
    Ok(())
}

pub(crate) fn export_session(
    store: &SessionStore,
    id: &str,
    format: ExportFormat,
) -> Result<(), String> {
    if !store.session_exists(id).map_err(|e| e.to_string())? {
        return Err(format!("no session {id} in this project"));
    }
    let messages = store.load_messages(id).map_err(|e| e.to_string())?;

    match format {
        ExportFormat::Json => {
            let json = serde_json::to_string_pretty(&messages).map_err(|e| e.to_string())?;
            println!("{json}");
        }
        ExportFormat::Markdown => {
            println!("# smith session {id}\n");
            for message in &messages {
                let text = message.text();
                if text.trim().is_empty() {
                    // Tool-call rounds carry no prose; a heading with nothing
                    // under it is noise in a document meant to be read.
                    continue;
                }
                let who = match message.role {
                    smith_core::Role::User => "user",
                    smith_core::Role::Assistant => "assistant",
                };
                println!("## {who}\n\n{text}\n");
            }
        }
    }
    Ok(())
}

/// `smith remember <note>` — appends a standing instruction to a `SMITH.md`.
///
/// The project target is the *root* of the memory chain, not the working
/// directory: a note dropped into whatever subdirectory you happened to be in
/// would only apply while you stayed there, which is not what "remember this"
/// means. `--global` picks `~/.smith/SMITH.md` instead.
pub(crate) fn run_remember(note: &str, global: bool) -> ExitCode {
    let path = if global {
        smith_config::memory::global_memory_path()
    } else {
        let cwd = std::env::current_dir().unwrap_or_default();
        Ok(smith_config::memory::memory_path(
            &smith_config::MemoryScope::discover(cwd).root,
        ))
    };

    let result = path.and_then(|path| {
        smith_config::memory::remember(&path, note)?;
        Ok(path)
    });

    match result {
        Ok(path) => {
            println!("remembered in {}", path.display());
            ExitCode::from(EXIT_OK)
        }
        Err(e) => {
            eprintln!("smith: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Hands the browser `smith setup` provisioned to `smith-tools`.
///
/// `smith_tools::chromium` resolves a browser from `SMITH_CHROMIUM_PATH`,
/// then `CHROME_PATH`, then `PATH` — it knows nothing about `smith-config`,
/// and `smith-cli` cannot reach into it to teach it. Exporting the variable is
/// the seam that already exists, so no change to that crate is needed for a
/// provisioned browser to be found. (Having `chromium.rs` probe
/// `~/.smith/runtime` itself would be tidier; see the note filed with this
/// change.)
///
/// A variable the user set themselves is never overwritten: an explicit
/// override has to keep winning, which is the whole contract of those two
/// variables.
pub(crate) fn export_provisioned_browser(config: &Config) {
    let Some(path) = browser_path_to_export(config, |v| std::env::var(v).ok()) else {
        return;
    };
    // Safe here in the way `set_var` needs to be: this runs during startup
    // resolution, before any tool, provider or TUI task exists, so nothing
    // else is reading the environment concurrently.
    std::env::set_var(runtime::BROWSER_PATH_ENV, path);
}

/// The decision `export_provisioned_browser` makes, without the side effect —
/// so the precedence is testable without mutating the process environment out
/// from under every other test in the binary.
pub(crate) fn browser_path_to_export(
    config: &Config,
    env: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let path = config
        .runtime
        .chromium_path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())?;
    // A variable the user set themselves is never overwritten: an explicit
    // override has to keep winning, which is the whole contract of these two.
    for var in [
        runtime::BROWSER_PATH_ENV,
        runtime::BROWSER_PATH_ENV_FALLBACK,
    ] {
        if env(var).is_some_and(|v| !v.trim().is_empty()) {
            return None;
        }
    }
    Some(path.to_string())
}
