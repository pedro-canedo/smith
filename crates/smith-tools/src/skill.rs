//! `skill` — the tool a model calls to load a skill's instructions on demand.
//!
//! Skills are discovered by `smith_config::extend::skills`; this is only the
//! channel the model reaches them through. Nothing here touches the
//! filesystem: the frontend hands over already-validated entries at startup,
//! so a call is a lookup and a string.
//!
//! # Why a tool, and not a name the model mentions
//!
//! The alternative on the table was making the model *say* a skill's name and
//! having the agent notice. It was rejected four times over:
//!
//! - **A mention has no channel.** Detecting one means scanning assistant text
//!   for names, which fires on any prose that merely quotes one ("I could use
//!   the `release` skill here, but I won't") — precisely the ambiguity
//!   `Agent::recover_text_tool_call` refuses to accept when it declines to
//!   dispatch on a bare `action` field. The only way to disambiguate is a
//!   sentinel the model must emit verbatim, which is a tool call with worse
//!   ergonomics and no schema.
//! - **A tool result is a message.** The body lands after the cached system
//!   prefix and after the existing history, so loading a skill invalidates
//!   nothing. A mention-triggered injection would have to go into the system
//!   prompt — rewriting the prefix mid-session — or be forged as a user
//!   message, which puts words in the user's mouth.
//! - **It reuses everything.** Argument-schema validation, the tolerant
//!   name/argument alignment that lets small models call tools at all, the
//!   permission class, the `stream-json` event stream, and the tool card that
//!   shows the user a skill was loaded and which one. A mention-based scheme
//!   would be invisible in every one of those.
//! - **The JSON-envelope fallback already covers models with no tool channel**
//!   (`{"action": "skill", "name": "release"}`), so the weakest models are
//!   better served by a tool than by the scheme that was supposed to be for
//!   them.
//!
//! # Cost when nothing is used
//!
//! The catalogue is listed in this tool's own `description`, so an unused
//! skill costs one line and the whole feature costs one tool definition.
//! smith ships builtin skills (`smith_config::extend::skills`), so in
//! practice the tool is always registered and the standing price is the
//! catalogue — a dozen-odd lines, paid once per session via the prefix
//! cache. The frontend still skips registration for an empty catalogue,
//! which is reachable only if the builtins are ever compiled out.

use crate::args::field_str;
use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

/// The tool name. `PermissionClass::ReadOnly`, so it never prompts: the body
/// was already on disk inside the project or the user's own config directory,
/// and reading it has no effect a prompt could protect.
pub const SKILL_TOOL: &str = "skill";

/// One skill as this tool sees it: an index line, and the body to hand back.
///
/// A plain struct rather than `smith_config::extend::skills::Skill` so
/// `smith-tools` does not grow a dependency on `smith-config`. The frontend
/// already depends on both and does the two-field conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillEntry {
    pub name: String,
    /// The always-loaded half.
    pub description: String,
    /// The on-demand half, already framed by the loader.
    pub body: String,
}

pub struct SkillTool {
    entries: Vec<SkillEntry>,
    /// Built once in `new`, because `Tool::description` returns a borrow.
    description: String,
}

impl SkillTool {
    pub fn new(mut entries: Vec<SkillEntry>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let catalogue = entries
            .iter()
            .map(|e| format!("- {} — {}", e.name, e.description))
            .collect::<Vec<_>>()
            .join("\n");
        let description = format!(
            "Load the full instructions for a named skill. A skill is a set of standing \
             instructions the user wrote for a particular kind of task; only the one-line \
             summaries below are loaded, and calling this returns the rest.\n\n\
             Call it when a summary below matches what you are about to do — before starting the \
             work, not after. Calling it for an unrelated task wastes context; skipping one that \
             does match means doing the task the way the user asked you not to.\n\n\
             Available skills:\n{catalogue}"
        );
        Self {
            entries,
            description,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn names(&self) -> String {
        self.entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        SKILL_TOOL
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        // `enum` as well as the prose list: it is the one part of the schema a
        // provider's constrained decoding can actually enforce, which turns
        // "invented a skill name" from a tool error into an impossibility on
        // the providers that support it.
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "The skill to load, exactly as listed in this tool's description.",
                    "enum": self.entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
                }
            },
            "required": ["name"],
            "additionalProperties": false,
        })
    }

    fn permission_class(&self) -> PermissionClass {
        PermissionClass::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        let Some(requested) = field_str(&input, "name") else {
            return ToolResult::error(format!(
                "`skill` needs a `name`. Available: {}.",
                self.names()
            ));
        };
        let wanted = requested.trim().to_ascii_lowercase();
        match self.entries.iter().find(|e| e.name == wanted) {
            Some(entry) => ToolResult::ok(entry.body.clone()),
            // Named rather than guessed at. `Agent::resolve_tool_name` is
            // deliberately tolerant about *tool* names and deliberately never
            // speculative when two could match; the same judgement applies
            // here, and the full list is short enough to just print.
            None => ToolResult::error(format!(
                "There is no skill named `{requested}`. Available: {}.",
                self.names()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> SkillTool {
        SkillTool::new(vec![
            SkillEntry {
                name: "release".into(),
                description: "How this repo cuts a release".into(),
                body: "SECRET STEPS: bump, tag, push.".into(),
            },
            SkillEntry {
                name: "commit-style".into(),
                description: "How commits are written here".into(),
                body: "Imperative mood.".into(),
            },
        ])
    }

    async fn call(tool: &SkillTool, input: serde_json::Value) -> ToolResult {
        tool.execute(
            input,
            &ToolContext::new(".", "test"),
            CancellationToken::new(),
        )
        .await
    }

    /// The feature, stated as one assertion: descriptions in the definition,
    /// bodies nowhere near it.
    #[tokio::test]
    async fn the_definition_carries_descriptions_and_the_body_only_arrives_on_request() {
        let tool = tool();
        let definition = tool.definition();
        assert!(definition
            .description
            .contains("How this repo cuts a release"));
        assert!(
            !definition.description.contains("SECRET STEPS"),
            "a body leaked into the always-loaded definition:\n{}",
            definition.description
        );

        let result = call(&tool, serde_json::json!({"name": "release"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("SECRET STEPS"));
    }

    #[tokio::test]
    async fn each_skill_costs_one_line_of_the_definition() {
        let definition = tool().definition();
        let listed: Vec<&str> = definition
            .description
            .lines()
            .filter(|l| l.starts_with("- "))
            .collect();
        assert_eq!(listed.len(), 2, "{listed:?}");
    }

    #[tokio::test]
    async fn lookup_tolerates_case_and_surrounding_space() {
        let result = call(&tool(), serde_json::json!({"name": "  RELEASE  "})).await;
        assert!(!result.is_error, "{}", result.content);
    }

    #[tokio::test]
    async fn an_unknown_skill_is_refused_with_the_list_rather_than_guessed_at() {
        let result = call(&tool(), serde_json::json!({"name": "relase"})).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("commit-style"),
            "{}",
            result.content
        );
        assert!(result.content.contains("release"), "{}", result.content);
    }

    #[tokio::test]
    async fn a_missing_name_is_refused_with_the_list() {
        let result = call(&tool(), serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.content.contains("Available"), "{}", result.content);
    }

    #[test]
    fn the_schema_enumerates_the_skills_that_exist() {
        let schema = tool().input_schema();
        let names = schema["properties"]["name"]["enum"].as_array().unwrap();
        assert_eq!(names.len(), 2);
        assert!(names.iter().any(|n| n == "release"));
    }

    /// It never prompts, and it declares nothing to snapshot — both follow
    /// from it doing nothing but hand back a string it already held.
    #[test]
    fn it_is_read_only_and_writes_nothing() {
        let tool = tool();
        assert_eq!(tool.permission_class(), PermissionClass::ReadOnly);
        assert!(tool
            .snapshot_paths(
                &serde_json::json!({"name": "release"}),
                &ToolContext::new(".", "t")
            )
            .is_empty());
    }
}
