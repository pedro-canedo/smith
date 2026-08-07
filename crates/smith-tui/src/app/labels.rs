//! Naming a tool call for the card header and the activity line.

use super::chatline::group_class;
use super::MAX_LABEL_CHARS;

/// The friendly header labels of a tool card, one per lifecycle state.
///
/// The card's header is these labels, never the raw tool name — `web_search`
/// reads as "Searching the web…" while it spins and "Search completed" once
/// it lands; the raw name stays available in the verbose body (Ctrl+T or
/// Enter on the card). The call's *target* (path, query, command) is not part
/// of the label: the renderer appends it from `tool_input`, so the label can
/// stay a constant verb phrase.
pub(crate) struct ToolLabels {
    pub(crate) running: String,
    pub(crate) done: String,
    pub(crate) failed: String,
}

/// Labels for `tool_name`, including the `mcp__server__tool` bridge naming.
///
/// MCP and unknown tools land on the same fallback: the prettified name with
/// generic verbs around it.
pub(crate) fn tool_labels(tool_name: &str) -> ToolLabels {
    let (running, done, failed) = match tool_name {
        "web_search" => ("Searching the web…", "Search completed", "Search failed"),
        "web_fetch" => ("Fetching page…", "Page fetched", "Fetch failed"),
        "read_file" => ("Reading", "Read", "Could not read"),
        "write_file" => ("Writing", "Wrote", "Write failed"),
        "edit_file" | "multi_edit" => ("Editing", "Edited", "Edit failed"),
        "list_dir" => ("Listing", "Listed", "Could not list"),
        "glob" | "grep" => ("Searching", "Searched", "Search failed"),
        "run_bash" => ("Running command…", "Command completed", "Command failed"),
        "task" => ("Delegating…", "Delegated", "Delegation failed"),
        "write_tasks" => ("Updating tasks…", "Tasks updated", "Task update failed"),
        other => {
            let pretty = pretty_tool_name(other);
            return ToolLabels {
                running: format!("Calling {pretty}…"),
                done: format!("{pretty} completed"),
                failed: format!("{pretty} failed"),
            };
        }
    };
    ToolLabels {
        running: running.to_string(),
        done: done.to_string(),
        failed: failed.to_string(),
    }
}

/// The header of a card standing for a whole run of calls.
///
/// Not `tool_labels`, which speaks for exactly one call: "Search completed" is
/// wrong on a card holding six searches and four page fetches, and picking the
/// first call's wording makes the header change meaning depending on which
/// tool happened to start the run.
pub(crate) fn group_labels(tool_name: &str) -> ToolLabels {
    let (running, done, failed) = match group_class(tool_name) {
        Some("research") => ("Researching the web…", "Research", "Research failed"),
        // Unreachable while `group_class` has one class, and a plain fallback
        // rather than a panic so adding a class can never take the UI down.
        _ => return tool_labels(tool_name),
    };
    ToolLabels {
        running: running.to_string(),
        done: done.to_string(),
        failed: failed.to_string(),
    }
}

/// `mcp__server__tool` → `server · tool`; anything else passes through.
fn pretty_tool_name(name: &str) -> String {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .map(|(server, tool)| format!("{server} · {tool}"))
        .unwrap_or_else(|| name.to_string())
}

/// Short, human-readable summary of what a tool call is doing — the
/// running-state label plus its target, kept as the tool line's `text` for
/// anything that reads lines as plain strings (tests, future exports).
/// What one folded call was about — the query, the URL — with no activity
/// wording around it. Mirrors `ui::tool_target`, which does the same job for
/// a card's own header, but from the raw input rather than from a `ChatLine`.
pub(super) fn group_target(tool_name: &str, input: &serde_json::Value) -> String {
    let field = match tool_name {
        "web_search" => "query",
        "web_fetch" => "url",
        _ => return String::new(),
    };
    input
        .get(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

pub(super) fn activity_label(tool_name: &str, input: &serde_json::Value) -> String {
    let field = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or_default();
    let target = match tool_name {
        "read_file" | "write_file" | "edit_file" | "multi_edit" | "list_dir" => field("path"),
        "glob" | "grep" => field("pattern"),
        "task" => field("description"),
        "run_bash" => field("command"),
        "web_search" => field("query"),
        "web_fetch" => field("url"),
        _ => "",
    };
    let running = tool_labels(tool_name).running;
    let label = if target.is_empty() {
        running
    } else {
        format!("{running} {target}")
    };
    truncate(&label, MAX_LABEL_CHARS)
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

pub(super) fn looks_like_approval_request(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("aprovar")
        || lower.contains("approve the plan")
        || lower.contains("approval")
        || lower.contains("/plan approve")
}
