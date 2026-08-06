//! The one `App` the crate's tests build from.
//!
//! There were three literal `TuiConfig`s before this module — in `app`'s
//! tests, in `transcript`'s, and in `ui`'s — which meant a new required field
//! on `TuiConfig` had to be added in three places, and two of the three
//! disagreed about the provider and model labels for no reason anyone had
//! written down.
//!
//! Plain `#[cfg(test)]`, deliberately not a feature, unlike
//! `smith_core::testkit`. That one is gated because it crosses a crate
//! boundary: `smith-provider` and `smith-cli` dev-depend on it, and a
//! downstream crate cannot see an upstream crate's `#[cfg(test)]` module.
//! Every consumer here is inside `smith-tui`, so the gate would buy nothing
//! and cost a build dimension nobody runs.
//!
//! The labels below are load-bearing: `app::tests` asserts `/model` reports
//! `current: anthropic/claude-sonnet-5`. A test that needs different ones sets
//! them on the returned `App` rather than growing a parameter here — see
//! `ui::tests::app_with_context`, whose whole point is that a *specific* cwd
//! is dropped from a narrow status bar, and which would pass vacuously if it
//! inherited a cwd it never names.

use smith_core::{AgentEvent, PermissionPolicy};
use tempfile::TempDir;

use crate::app::{App, ChatLine, ChatRole, IdleHint, TuiConfig};
use crate::logbuf::LogBuffer;
use crate::slash::SlashRegistry;
use crate::theme::Theme;

pub(crate) fn test_app() -> App {
    app_with_commands(SlashRegistry::builtin())
}

/// Private: every caller wanted either the builtin registry (`test_app`) or
/// one discovered from files (`app_with_command_files`), never a hand-built
/// one, so the parameter stays an implementation detail of this module.
fn app_with_commands(commands: SlashRegistry) -> App {
    App::new(TuiConfig {
        banner: String::new(),
        provider_label: "anthropic".to_string(),
        model_label: "claude-sonnet-5".to_string(),
        cwd_display: "~/proj".to_string(),
        git_branch: None,
        idle_hint: IdleHint::Tip("test".to_string()),
        initial_lines: Vec::new(),
        permission_policy: PermissionPolicy::default(),
        theme: Theme::ansi(),
        goal: None,
        tasks: Vec::new(),
        commands,
        keys: Default::default(),
        history: Vec::new(),
        logs: LogBuffer::default(),
    })
}

/// An app whose custom commands were loaded from real files under a temp
/// project — the `TempDir` has to outlive the `App`, hence the pair.
pub(crate) fn app_with_command_files(files: &[(&str, &str)]) -> (TempDir, App) {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let global = tmp.path().join("global");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&global).unwrap();
    for (rel, body) in files {
        let path = project.join(".smith/commands").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let set = smith_config::CommandSet::discover_in(
        Some(&global),
        &project,
        &crate::slash::builtin_names(),
    );
    (tmp, app_with_commands(SlashRegistry::new(set)))
}

/// `count` finished tool cards, each followed by an assistant reply — the
/// transcript shape card focus and scrolling are exercised against.
pub(crate) fn app_with_cards(count: usize) -> App {
    let mut app = test_app();
    for i in 0..count {
        app.on_agent_event(AgentEvent::ToolCallStarted {
            id: format!("call_{i}"),
            tool_name: "read_file".into(),
            input: serde_json::json!({ "path": format!("src/f{i}.rs") }),
        });
        app.on_agent_event(AgentEvent::ToolCallResult {
            id: format!("call_{i}"),
            output: "ok".into(),
            is_error: false,
        });
        app.lines
            .push(ChatLine::new(ChatRole::Assistant, format!("resposta {i}")));
    }
    app
}
