//! Short, human-readable permission summaries.
//!
//! The modal should answer "what will happen?" in one glance — create/edit a
//! file, run a command — not dump the full payload.

use serde_json::Value;

const MAX_COMMAND_CHARS: usize = 100;

/// Build a one-line (or few-line) confirmation summary for a tool call.
pub fn format_permission_detail(tool_name: &str, input: &Value) -> String {
    match tool_name {
        "write_file" => format_write(input),
        "edit_file" => format_edit(input),
        "run_bash" => format_bash(input),
        "read_file" => format_path_action("Read file", input),
        "list_dir" => format_path_action("List directory", input),
        "glob" => {
            let pattern = field(input, "pattern");
            if pattern.is_empty() {
                "Search files".into()
            } else {
                format!("Search files matching `{pattern}`")
            }
        }
        other => format_generic(other, input),
    }
}

fn field<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

fn format_write(input: &Value) -> String {
    let path = field(input, "path");
    let content = field(input, "content");
    if path.is_empty() {
        return "Create or overwrite a file".into();
    }
    let bytes = content.len();
    let lines = if content.is_empty() {
        0
    } else {
        content.lines().count().max(1)
    };
    format!("Create/overwrite `{path}` ({lines} lines, {bytes} bytes)")
}

fn format_edit(input: &Value) -> String {
    let path = field(input, "path");
    if path.is_empty() {
        "Edit a file".into()
    } else {
        let old_len = field(input, "old_str").len();
        let new_len = field(input, "new_str").len();
        format!("Edit `{path}` (replace {old_len}→{new_len} chars)")
    }
}

fn format_bash(input: &Value) -> String {
    let command = field(input, "command");
    if command.is_empty() {
        "Run a shell command".into()
    } else {
        format!("Run `{}`", truncate_chars(command, MAX_COMMAND_CHARS))
    }
}

fn format_path_action(verb: &str, input: &Value) -> String {
    let path = field(input, "path");
    if path.is_empty() {
        verb.to_string()
    } else {
        format!("{verb} `{path}`")
    }
}

fn format_generic(tool_name: &str, input: &Value) -> String {
    let Some(obj) = input.as_object() else {
        return format!("Call `{tool_name}`");
    };
    // Prefer a path / command / url style key when present.
    for key in ["path", "command", "url", "query", "name"] {
        if let Some(Value::String(v)) = obj.get(key) {
            if !v.is_empty() {
                return format!(
                    "Call `{tool_name}` ({key}: {})",
                    truncate_chars(v, MAX_COMMAND_CHARS)
                );
            }
        }
    }
    if obj.is_empty() {
        format!("Call `{tool_name}`")
    } else {
        format!("Call `{tool_name}` ({} args)", obj.len())
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn write_file_is_a_short_summary() {
        let detail = format_permission_detail(
            "write_file",
            &json!({
                "path": "styles.css",
                "content": ":root {\n  --primary: #6366f1;\n}\n"
            }),
        );
        assert!(detail.contains("Create/overwrite `styles.css`"), "{detail}");
        assert!(detail.contains("lines"), "{detail}");
        assert!(
            !detail.contains("--primary"),
            "must not dump file body: {detail}"
        );
        assert!(!detail.contains('{'));
    }

    #[test]
    fn edit_file_is_a_short_summary() {
        let detail = format_permission_detail(
            "edit_file",
            &json!({
                "path": "a.rs",
                "old_str": "foo",
                "new_str": "barbar"
            }),
        );
        assert_eq!(detail, "Edit `a.rs` (replace 3→6 chars)");
    }

    #[test]
    fn run_bash_shows_command() {
        let detail = format_permission_detail(
            "run_bash",
            &json!({ "command": "cargo test -p smith-core" }),
        );
        assert_eq!(detail, "Run `cargo test -p smith-core`");
    }

    #[test]
    fn generic_tool_prefers_path() {
        let detail =
            format_permission_detail("mcp_thing", &json!({ "path": "/tmp/x", "extra": 1 }));
        assert!(detail.contains("mcp_thing"));
        assert!(detail.contains("/tmp/x"));
    }
}
