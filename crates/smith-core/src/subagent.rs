//! Subagents — the `task` tool.
//!
//! A subagent is a whole second [`Agent`](crate::agent::Agent) run from inside
//! one tool call of the first. The parent asks a question ("find every call
//! site of `run_one_tool` and summarise what they pass"), the child spends
//! twenty tool calls answering it, and the *only* thing that comes back into
//! the parent's history is the child's final message.
//!
//! That asymmetry is the entire point, and it is worth being explicit about
//! what it buys: twenty file reads is easily 40k tokens of tool results the
//! parent would otherwise carry for the rest of the session, in exchange for a
//! few hundred tokens of report. A subagent that streamed its transcript back
//! to the parent would cost strictly *more* than doing the work inline (two
//! system prompts, two sets of tool schemas) and would have failed at its job.
//!
//! Everything here is about keeping a child bounded — in tools, in budget, in
//! depth, and in what it is allowed to interrupt.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{PermissionAsk, QuestionAsk, ToolExecutor};
use crate::event::AgentEvent;
use crate::message::{StopReason, ToolDefinition};
use crate::permission_detail::format_permission_detail;
use crate::tool::{PermissionClass, ToolContext, ToolResult};

/// The tool a model calls to delegate. Intercepted by name in
/// `Agent::run_one_tool`, like `ask_user` and `write_tasks`.
pub const TASK_TOOL: &str = "task";

/// The child used when a call names no `subagent_type`, and the only one that
/// exists without a definition file.
pub const GENERAL_PURPOSE: &str = "general-purpose";

/// How deep delegation may go. **One**: the root agent may spawn a child, and
/// that child may not spawn anything.
///
/// Cost is the first reason — each level multiplies the worst-case token spend
/// by the branching factor, and a two-level tree of runaway children is a bill
/// nobody authorised and nobody can see accruing. The second is that there is
/// no honest way to *show* a deeper tree: a child's activity reaches the user
/// as progress lines on a single tool card, and a grandchild's lines would
/// have to be flattened onto that same card with no way to tell whose they
/// are. The third is that the delegation worth having is one level anyway —
/// "go read these twenty files and tell me what you found" — and every
/// additional level makes the report a summary of a summary.
///
/// Enforced twice, deliberately: a child's tool set never contains `task`
/// (so the model cannot even see it), and `run_task` refuses on depth
/// regardless (so a JSON-envelope fallback call, which resolves names against
/// the *registered* tools, cannot sneak past the first check).
pub const MAX_DEPTH: u32 = 1;

/// Tool-call rounds one child may run. Well under the parent's 50: a child
/// exists to answer one bounded question, and a child that needs sixteen
/// rounds of exploration is one whose task should have been split by the
/// parent — which is the caller that can actually see the whole problem.
pub const MAX_ROUNDS: u32 = 16;

/// Tool calls one child may make, and the cap on how much of the parent's
/// shared pool any single child can claim (see
/// `Agent::subagent_tool_budget`). Thirty read calls is a thorough sweep of a
/// subsystem; past that the child is browsing.
pub const MAX_TOOL_CALLS: u32 = 30;

/// Wall clock for one child, further clamped to whatever the parent's own turn
/// has left. A child cannot outlive the turn that spawned it, and this cap
/// makes it fail fast rather than eating the parent's entire remaining budget
/// on one delegation.
pub const MAX_WALL_CLOCK: Duration = Duration::from_secs(240);

/// Tools a subagent never gets even though they are `ReadOnly`.
///
/// `ask_user` and `write_tasks` are intercepted by the agent and reach into
/// UI state that belongs to the *parent* — a modal the user cannot attribute,
/// and a checklist the parent is keeping. `task` is the depth limit.
const NEVER: [&str; 3] = ["ask_user", "write_tasks", TASK_TOOL];

/// How long a child's interim prose or a forwarded progress line may be when
/// it lands on the parent's tool card.
const PROGRESS_CHARS: usize = 140;

/// A subagent's identity: what it is called, what it is for, what it may use,
/// and what it is told.
///
/// Parsed from `~/.smith/agents/<name>.md` by the frontend (which owns the
/// notion of a home directory) and handed to the agent via
/// `Agent::with_subagent_definitions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentDefinition {
    /// Matched against the `subagent_type` argument of a `task` call.
    pub name: String,
    /// What this child is for. The *parent model* reads this to choose, so it
    /// is required — an unnamed capability may as well not exist.
    pub description: String,
    /// Tool names this child may use, or `None` for the default read-only set.
    /// Always further intersected with what is actually registered and
    /// read-only; see [`resolve_tool_set`].
    pub tools: Option<Vec<String>>,
    /// Model override. `None` keeps the parent's — the one model we know the
    /// configured provider actually serves.
    pub model: Option<String>,
    /// Markdown body: extra standing instructions appended to the built-in
    /// subagent prompt.
    pub instructions: String,
}

impl SubagentDefinition {
    /// The built-in child, available with no configuration at all.
    pub fn general_purpose() -> Self {
        Self {
            name: GENERAL_PURPOSE.to_string(),
            description: "Researches a question across the codebase and reports back. \
                          Read-only: searches, reads and summarises."
                .to_string(),
            tools: None,
            model: None,
            instructions: String::new(),
        }
    }

    /// Parses a definition file: YAML-ish front matter between `---` fences,
    /// then a markdown body.
    ///
    /// Hand-rolled rather than pulled from `serde_yaml`, for the same reason
    /// `smith-mcp` hand-rolls JSON-RPC: the accepted grammar is four scalar
    /// keys and one list, the failure messages should name the file's own
    /// vocabulary, and a YAML dependency in `smith-core` would be the largest
    /// thing in the crate.
    ///
    /// Unknown keys are ignored rather than rejected — a definition written
    /// for a later version of smith should degrade to what this version
    /// understands, not fail to load.
    pub fn parse(source: &str) -> Result<Self, String> {
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let rest = source
            .trim_start()
            .strip_prefix("---")
            .ok_or("missing `---` front matter: a subagent file must open with a `---` line")?
            .trim_start_matches(['\r', ' '])
            .strip_prefix('\n')
            .ok_or("the opening `---` must be on a line of its own")?;

        let mut front = String::new();
        let mut body = String::new();
        let mut closed = false;
        for line in rest.split_inclusive('\n') {
            if !closed && line.trim_end() == "---" {
                closed = true;
                continue;
            }
            if closed {
                body.push_str(line);
            } else {
                front.push_str(line);
            }
        }
        if !closed {
            return Err("front matter is never closed: expected a second `---` line".into());
        }

        let mut name = None;
        let mut description = None;
        let mut model = None;
        let mut tools: Option<Vec<String>> = None;
        // Set while reading a `key:` whose value is a block list underneath it.
        let mut list_key: Option<String> = None;

        for line in front.lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            if let Some(item) = line.trim_start().strip_prefix("- ") {
                if list_key.as_deref() == Some("tools") {
                    tools.get_or_insert_with(Vec::new).push(unquote(item));
                }
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                return Err(format!("front matter line is not `key: value`: {line}"));
            };
            let key = key.trim().to_ascii_lowercase();
            let value = unquote(value);
            list_key = None;
            match key.as_str() {
                "name" => name = Some(value),
                "description" => description = Some(value),
                "model" => model = Some(value),
                "tools" => {
                    if value.is_empty() {
                        // A block list follows on the next lines.
                        list_key = Some("tools".to_string());
                        tools.get_or_insert_with(Vec::new);
                    } else {
                        tools = Some(
                            value
                                .trim_matches(['[', ']'])
                                .split(',')
                                .map(unquote)
                                .filter(|t| !t.is_empty())
                                .collect(),
                        );
                    }
                }
                _ => {}
            }
        }

        let name = name.filter(|n| !n.is_empty()).ok_or(
            "front matter has no `name`, and the name is how a `task` call selects this subagent",
        )?;
        if name.split_whitespace().count() != 1 {
            return Err(format!(
                "`name` must be a single word with no spaces (the model passes it verbatim as \
                 subagent_type), but got `{name}`"
            ));
        }
        let description = description.filter(|d| !d.is_empty()).ok_or(
            "front matter has no `description`, and that is what the calling model reads to \
             decide whether to use this subagent",
        )?;

        Ok(Self {
            name,
            description,
            tools,
            model: model.filter(|m| !m.is_empty()),
            instructions: body.trim().to_string(),
        })
    }
}

fn unquote(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .to_string()
}

/// Which tools a child actually gets, and which requested ones were dropped.
///
/// **Read-only, always.** A definition asking for `write_file` or `run_bash`
/// does not get it. This is the single most consequential default here, so:
///
/// - A child's work is invisible while it runs. Its tool calls reach the user
///   as one summarised progress line on a card, and its transcript is thrown
///   away — so a child that edits files produces changes nobody watched being
///   made and nobody can attribute afterwards.
/// - Every mutating call would need a permission decision, and the modal
///   belongs to the parent's UI. See [`refuse_permissions`] for why routing it
///   up was rejected.
/// - The delegation that pays for itself is *reading*. Reads are what fill a
///   context window; a write is one line of `tool_use` the parent can perfectly
///   well emit itself, having read the child's report.
/// - A wrong read is recoverable by reading again. A wrong write, made by an
///   agent nobody was watching, is not — and `/rewind` cannot cover a child's
///   `run_bash` any better than it covers the parent's.
///
/// The intersection is with *what is registered*: a definition naming a tool
/// this build doesn't have loses that tool and keeps the rest, rather than
/// failing to load.
pub fn resolve_tool_set(
    tools: &dyn ToolExecutor,
    requested: Option<&[String]>,
) -> (BTreeSet<String>, Vec<String>) {
    let usable = |name: &str| {
        !NEVER.contains(&name) && tools.permission_class(name) == Some(PermissionClass::ReadOnly)
    };
    match requested {
        None => (
            tools
                .tool_defs()
                .into_iter()
                .map(|d| d.name)
                .filter(|n| usable(n))
                .collect(),
            Vec::new(),
        ),
        Some(names) => {
            let (allowed, refused): (Vec<String>, Vec<String>) =
                names.iter().cloned().partition(|n| usable(n));
            (allowed.into_iter().collect(), refused)
        }
    }
}

/// A [`ToolExecutor`] view that exposes — and permits — only a named subset.
///
/// The filtering has to be here rather than in the child's prompt because a
/// prompt is a request, not a gate: the whole reason `resolve_tool_set` is
/// read-only is undone by a model that calls `run_bash` anyway.
pub struct RestrictedTools {
    inner: Arc<dyn ToolExecutor>,
    allowed: BTreeSet<String>,
}

impl RestrictedTools {
    pub fn new(inner: Arc<dyn ToolExecutor>, allowed: BTreeSet<String>) -> Self {
        Self { inner, allowed }
    }

    pub fn allowed(&self) -> &BTreeSet<String> {
        &self.allowed
    }

    fn refusal(&self, name: &str) -> String {
        let mut names: Vec<&str> = self.allowed.iter().map(String::as_str).collect();
        names.sort_unstable();
        format!(
            "`{name}` is not available to this subagent. You have: {}. \
             Subagents are read-only — if the task needs a write, a command, or a question \
             answered, say so in your final report and let the agent that called you do it.",
            if names.is_empty() {
                "no tools at all".to_string()
            } else {
                names.join(", ")
            }
        )
    }
}

#[async_trait]
impl ToolExecutor for RestrictedTools {
    fn tool_defs(&self) -> Vec<ToolDefinition> {
        self.inner
            .tool_defs()
            .into_iter()
            .filter(|d| self.allowed.contains(&d.name))
            .collect()
    }

    /// A forbidden name reports `ReadOnly`, which sounds like a relaxation and
    /// is the opposite of one: the call is refused by `execute` below before it
    /// can reach the inner executor, so the class only decides *which* refusal
    /// the model gets. Reporting `None` would make `run_one_tool` treat it as
    /// `Dangerous` and open a permission prompt — for a tool that will never
    /// run, on a UI with nobody to answer it, and the model would be told "the
    /// user denied this" when no user was ever asked.
    fn permission_class(&self, name: &str) -> Option<PermissionClass> {
        if self.allowed.contains(name) {
            self.inner.permission_class(name)
        } else {
            Some(PermissionClass::ReadOnly)
        }
    }

    fn snapshot_paths(
        &self,
        name: &str,
        input: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Vec<std::path::PathBuf> {
        if self.allowed.contains(name) {
            self.inner.snapshot_paths(name, input, ctx)
        } else {
            Vec::new()
        }
    }

    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        if !self.allowed.contains(name) {
            return ToolResult::error(self.refusal(name));
        }
        self.inner.execute(name, input, ctx, cancel).await
    }
}

/// The child's system prompt: the built-in half, plus whatever its definition
/// adds.
///
/// The parent's own system prompt is deliberately *not* inherited. It is
/// thousands of tokens describing a session the child is not in — slash
/// commands it cannot run, a user it cannot address, mutating tools it does
/// not have — and every one of those is an instruction the child may try to
/// follow. Environment context (date, cwd, project memory) *is* inherited,
/// through the parent's `context_provider`, because that is fact rather than
/// role.
pub fn child_system_prompt(def: &SubagentDefinition, allowed: &BTreeSet<String>) -> String {
    let tools = if allowed.is_empty() {
        "none".to_string()
    } else {
        allowed
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut prompt = format!(
        "You are a subagent. Another agent delegated one self-contained task to you and is \
         blocked waiting for your answer. There is no human in this conversation.\n\n\
         Your final message is your entire report — it is the only thing that reaches the agent \
         that called you. Every file you read, every search you run and everything you say along \
         the way is discarded when you finish. So write the report as the deliverable: concrete \
         file paths, line numbers, symbol names, and quoted snippets wherever the exact text \
         matters, so the caller never has to redo your work. State what you could not determine \
         instead of guessing, and do not describe your process.\n\n\
         You have these tools and no others: {tools}. You cannot write files, run commands, ask \
         questions, or delegate further — so never promise to; report what you found and let the \
         caller act.\n\n\
         Your budget is limited. If you run out you are stopped where you are and whatever you \
         have written is returned, so work directly toward the answer and write it down."
    );
    if !def.instructions.trim().is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(def.instructions.trim());
    }
    prompt
}

/// Everything the parent learned from watching one child run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChildReport {
    /// The child's final assistant text — the report itself.
    pub report: String,
    /// Tool calls the child actually started, charged back against the
    /// parent's shared pool.
    pub tool_calls: u32,
    /// The child's turn failed (or was cancelled), rendered.
    pub error: Option<String>,
    /// The child hit one of its own caps, rendered.
    pub limit: Option<String>,
}

/// Drains a child's private event channel, turning it into progress lines on
/// the parent's `task` card and collecting the report.
///
/// This — not a new `AgentEvent` variant — is how a subagent is visible.
/// `ToolProgress` already flows to `stream-json` and already renders as the
/// newest line on a running tool card, so a child's activity shows up in every
/// frontend that exists without any of them learning what a subagent is. The
/// alternative, forwarding the child's events onto the parent's stream
/// verbatim, would open tool cards the parent's transcript has no `tool_use`
/// for and print the child's prose as if the assistant had said it to the user.
///
/// The one exception is [`AgentEvent::TokenUsage`], forwarded unchanged: the
/// user is paying for those tokens, so they belong in their totals. Compaction
/// makes the same exception for the same reason.
pub async fn relay_child(
    label: &str,
    call_id: &str,
    mut child_rx: mpsc::UnboundedReceiver<AgentEvent>,
    events: &mpsc::UnboundedSender<AgentEvent>,
) -> ChildReport {
    let mut out = ChildReport::default();
    let say = |line: String| {
        let _ = events.send(AgentEvent::ToolProgress {
            id: call_id.to_string(),
            line: format!("{label}: {line}"),
        });
    };

    while let Some(event) = child_rx.recv().await {
        match event {
            AgentEvent::ToolCallStarted {
                tool_name, input, ..
            } => {
                out.tool_calls += 1;
                say(format!(
                    "[{}] {}",
                    out.tool_calls,
                    brief(&format_permission_detail(&tool_name, &input))
                ));
            }
            // The child's own progress, one level down: a grep reporting how
            // far it has walked. Passed through rather than re-labelled — it
            // already reads as a status line.
            AgentEvent::ToolProgress { line, .. } => say(brief(&line)),
            AgentEvent::AssistantTurnComplete {
                message,
                stop_reason,
            } => {
                let text = message.text();
                if text.trim().is_empty() {
                    continue;
                }
                if stop_reason == StopReason::EndTurn {
                    // Last one wins: an earlier end-turn can only happen when
                    // the loop recovered a text-shaped tool call afterwards.
                    out.report = text;
                } else {
                    // Interim prose — "I'll start by grepping for X". The most
                    // useful single line to show, because it is the child
                    // saying what it is doing in its own words.
                    say(brief(&text));
                }
            }
            AgentEvent::TokenUsage(usage) => {
                let _ = events.send(AgentEvent::TokenUsage(usage));
            }
            AgentEvent::TurnLimitReached { detail, .. } => out.limit = Some(detail),
            AgentEvent::Error(e) => out.error = Some(e),
            _ => {}
        }
    }
    out
}

/// First line, clipped — a progress line has one row on a tool card.
fn brief(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.chars().count() <= PROGRESS_CHARS {
        return line.to_string();
    }
    format!("{}…", line.chars().take(PROGRESS_CHARS).collect::<String>())
}

/// The child's permission channel: everything is refused, nothing is shown.
///
/// A child never needs this — its tools are read-only, which is exactly the
/// class that does not prompt — so this is a backstop, and the shape of the
/// backstop is the decision. Three options were on the table:
///
/// - *Inherit the parent's session grants.* Rejected: the grants were given
///   for calls the user watched being proposed. Reusing them to authorise an
///   unwatched agent's writes is precisely the escalation the permission model
///   exists to prevent.
/// - *Route the prompt up with the child's identity.* Tempting, and rejected
///   on the details: the modal is keyed by `tool_call_id`, and a child's ids
///   belong to a transcript the user cannot see, so they would be approving a
///   call with no card and no context. Worse, the parent can be sitting on its
///   own modal at that moment — two prompts, one channel, and a user who
///   cannot tell which agent is asking.
/// - *Refuse.* What this does. The model is told plainly, in the same breath,
///   what to do instead: put it in the report and let the caller act.
pub async fn refuse_permissions(mut rx: mpsc::UnboundedReceiver<PermissionAsk>) {
    while let Some(ask) = rx.recv().await {
        let _ = ask.respond_to.send(crate::event::PermissionDecision::Deny);
    }
}

/// The child's question channel: refused with a reason.
///
/// `QuestionAsk` can answer `Err(reason)` — headless mode added it for the
/// same situation — and that is the honest shape here. Answering on the user's
/// behalf would put words in their mouth; leaving the oneshot to drop would
/// reach the model as "User dismissed the question", which is a claim about a
/// user who was never asked.
pub async fn refuse_questions(mut rx: mpsc::UnboundedReceiver<QuestionAsk>) {
    while let Some(ask) = rx.recv().await {
        let _ = ask.respond_to.send(Err(
            "There is no user to ask: you are a subagent. Decide with what you have, and \
             report the open question in your final answer so the agent that called you can \
             raise it."
                .to_string(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::NoTools;
    use crate::event::{PermissionDecision, UserQuestion};
    use crate::message::{ContentBlock, Message};
    use tokio::sync::oneshot;

    struct FakeTools(Vec<(&'static str, PermissionClass)>);

    #[async_trait]
    impl ToolExecutor for FakeTools {
        fn tool_defs(&self) -> Vec<ToolDefinition> {
            self.0
                .iter()
                .map(|(name, _)| ToolDefinition {
                    name: (*name).to_string(),
                    description: String::new(),
                    input_schema: serde_json::json!({"type": "object"}),
                })
                .collect()
        }
        fn permission_class(&self, name: &str) -> Option<PermissionClass> {
            self.0
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, class)| *class)
        }
        async fn execute(
            &self,
            name: &str,
            _input: serde_json::Value,
            _ctx: &ToolContext,
            _cancel: CancellationToken,
        ) -> ToolResult {
            ToolResult::ok(format!("{name} ran"))
        }
    }

    fn registry() -> FakeTools {
        FakeTools(vec![
            ("read_file", PermissionClass::ReadOnly),
            ("grep", PermissionClass::ReadOnly),
            ("write_file", PermissionClass::Mutating),
            ("run_bash", PermissionClass::Dangerous),
            ("ask_user", PermissionClass::ReadOnly),
            ("write_tasks", PermissionClass::ReadOnly),
            (TASK_TOOL, PermissionClass::ReadOnly),
        ])
    }

    #[test]
    fn the_default_tool_set_is_every_read_only_tool_except_the_ones_that_belong_to_the_parent() {
        let (allowed, refused) = resolve_tool_set(&registry(), None);
        assert_eq!(
            allowed,
            ["grep", "read_file"]
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );
        assert!(refused.is_empty());
    }

    #[test]
    fn a_definition_asking_for_a_mutating_tool_does_not_get_it() {
        let requested: Vec<String> = ["read_file", "write_file", "run_bash", TASK_TOOL]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (allowed, refused) = resolve_tool_set(&registry(), Some(&requested));
        assert_eq!(
            allowed,
            ["read_file"]
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(refused, vec!["write_file", "run_bash", TASK_TOOL]);
    }

    #[test]
    fn an_unregistered_tool_name_is_dropped_rather_than_failing_the_whole_set() {
        let requested = vec!["read_file".to_string(), "teleport".to_string()];
        let (allowed, refused) = resolve_tool_set(&registry(), Some(&requested));
        assert!(allowed.contains("read_file"));
        assert_eq!(refused, vec!["teleport"]);
    }

    #[tokio::test]
    async fn restricted_tools_hide_and_refuse_everything_outside_the_set() {
        let allowed: BTreeSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let tools = RestrictedTools::new(Arc::new(registry()), allowed);

        let names: Vec<String> = tools.tool_defs().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["read_file"]);

        let ctx = ToolContext::new(".", "test");
        let ok = tools
            .execute(
                "read_file",
                serde_json::json!({}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!ok.is_error, "{}", ok.content);

        let refused = tools
            .execute(
                "run_bash",
                serde_json::json!({"command": "rm -rf /"}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(refused.is_error);
        assert!(
            refused.content.contains("not available to this subagent")
                && refused.content.contains("read_file"),
            "{}",
            refused.content
        );
    }

    /// The refusal must come from `execute`, not from the permission gate —
    /// see the comment on `permission_class`.
    #[test]
    fn a_forbidden_tool_never_reports_a_class_that_would_open_a_permission_prompt() {
        let allowed: BTreeSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let tools = RestrictedTools::new(Arc::new(registry()), allowed);
        assert_eq!(
            tools.permission_class("run_bash"),
            Some(PermissionClass::ReadOnly)
        );
        assert_eq!(
            tools.permission_class("read_file"),
            Some(PermissionClass::ReadOnly)
        );
    }

    #[test]
    fn a_forbidden_tool_declares_nothing_to_snapshot() {
        let allowed: BTreeSet<String> = ["read_file"].iter().map(|s| s.to_string()).collect();
        let tools = RestrictedTools::new(Arc::new(registry()), allowed);
        assert!(tools
            .snapshot_paths(
                "write_file",
                &serde_json::json!({"path": "a.txt"}),
                &ToolContext::new(".", "test")
            )
            .is_empty());
    }

    #[test]
    fn a_definition_parses_front_matter_and_body() {
        let def = SubagentDefinition::parse(
            "---\n\
             name: doc-finder\n\
             description: Finds the docs for a symbol\n\
             tools: read_file, grep\n\
             model: some-small-model\n\
             ---\n\
             Prefer doc comments over README prose.\n",
        )
        .unwrap();
        assert_eq!(def.name, "doc-finder");
        assert_eq!(def.description, "Finds the docs for a symbol");
        assert_eq!(
            def.tools,
            Some(vec!["read_file".to_string(), "grep".to_string()])
        );
        assert_eq!(def.model.as_deref(), Some("some-small-model"));
        assert_eq!(def.instructions, "Prefer doc comments over README prose.");
    }

    #[test]
    fn tools_may_be_written_as_a_block_list() {
        let def = SubagentDefinition::parse(
            "---\nname: a\ndescription: b\ntools:\n  - read_file\n  - \"grep\"\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(
            def.tools,
            Some(vec!["read_file".to_string(), "grep".to_string()])
        );
    }

    #[test]
    fn omitting_tools_means_the_default_set_rather_than_no_tools() {
        let def = SubagentDefinition::parse("---\nname: a\ndescription: b\n---\n").unwrap();
        assert_eq!(def.tools, None);
        assert!(def.instructions.is_empty());
    }

    #[test]
    fn an_unknown_front_matter_key_is_ignored_rather_than_rejected() {
        let def =
            SubagentDefinition::parse("---\nname: a\ndescription: b\ncolour: teal\n---\n").unwrap();
        assert_eq!(def.name, "a");
    }

    #[test]
    fn a_definition_without_a_name_or_description_is_rejected() {
        let err = SubagentDefinition::parse("---\ndescription: b\n---\n").unwrap_err();
        assert!(err.contains("name"), "{err}");
        let err = SubagentDefinition::parse("---\nname: a\n---\n").unwrap_err();
        assert!(err.contains("description"), "{err}");
    }

    #[test]
    fn a_multi_word_name_is_rejected_because_the_model_passes_it_verbatim() {
        let err =
            SubagentDefinition::parse("---\nname: doc finder\ndescription: b\n---\n").unwrap_err();
        assert!(err.contains("single word"), "{err}");
    }

    #[test]
    fn unterminated_or_absent_front_matter_is_rejected() {
        assert!(SubagentDefinition::parse("just a body\n").is_err());
        assert!(SubagentDefinition::parse("---\nname: a\ndescription: b\n").is_err());
    }

    #[test]
    fn the_child_prompt_names_exactly_the_tools_it_has_and_appends_its_instructions() {
        let mut def = SubagentDefinition::general_purpose();
        def.instructions = "Always check the tests too.".to_string();
        let allowed: BTreeSet<String> = ["grep", "read_file"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let prompt = child_system_prompt(&def, &allowed);
        assert!(prompt.contains("grep, read_file"), "{prompt}");
        assert!(prompt.ends_with("Always check the tests too."), "{prompt}");
        assert!(prompt.contains("no human in this conversation"));
    }

    #[tokio::test]
    async fn the_relay_summarises_child_activity_and_keeps_only_the_final_report() {
        let (child_tx, child_rx) = mpsc::unbounded_channel();
        let (parent_tx, mut parent_rx) = mpsc::unbounded_channel();

        child_tx
            .send(AgentEvent::AssistantTurnComplete {
                message: Message::assistant(vec![ContentBlock::Text {
                    text: "Let me grep for it.".into(),
                }]),
                stop_reason: StopReason::ToolUse,
            })
            .unwrap();
        child_tx
            .send(AgentEvent::ToolCallStarted {
                id: "c1".into(),
                tool_name: "read_file".into(),
                input: serde_json::json!({"path": "src/main.rs"}),
            })
            .unwrap();
        child_tx
            .send(AgentEvent::ToolCallResult {
                id: "c1".into(),
                output: "a hundred kilobytes of file".into(),
                is_error: false,
            })
            .unwrap();
        child_tx
            .send(AgentEvent::AssistantTurnComplete {
                message: Message::assistant(vec![ContentBlock::Text {
                    text: "It is defined at src/main.rs:12.".into(),
                }]),
                stop_reason: StopReason::EndTurn,
            })
            .unwrap();
        drop(child_tx);

        let report = relay_child("general-purpose", "call_1", child_rx, &parent_tx).await;
        assert_eq!(report.report, "It is defined at src/main.rs:12.");
        assert_eq!(report.tool_calls, 1);

        let lines: Vec<String> = std::iter::from_fn(|| parent_rx.try_recv().ok())
            .map(|e| match e {
                AgentEvent::ToolProgress { id, line } => {
                    assert_eq!(id, "call_1");
                    line
                }
                other => panic!("the relay must only emit progress here: {other:?}"),
            })
            .collect();
        assert_eq!(
            lines,
            vec![
                "general-purpose: Let me grep for it.".to_string(),
                "general-purpose: [1] Read file `src/main.rs`".to_string(),
            ]
        );
        // The 100 KB tool result is not among them — that is the whole point.
        assert!(!lines.iter().any(|l| l.contains("kilobytes")));
    }

    #[tokio::test]
    async fn the_relay_forwards_token_usage_because_the_user_pays_for_it() {
        let (child_tx, child_rx) = mpsc::unbounded_channel();
        let (parent_tx, mut parent_rx) = mpsc::unbounded_channel();
        child_tx
            .send(AgentEvent::TokenUsage(crate::message::Usage {
                input_tokens: 10,
                output_tokens: 3,
                ..Default::default()
            }))
            .unwrap();
        drop(child_tx);

        relay_child("x", "call_1", child_rx, &parent_tx).await;
        match parent_rx.try_recv().unwrap() {
            AgentEvent::TokenUsage(u) => assert_eq!(u.input_tokens, 10),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_relay_records_a_limit_and_an_error_without_losing_the_partial_report() {
        let (child_tx, child_rx) = mpsc::unbounded_channel();
        let (parent_tx, _parent_rx) = mpsc::unbounded_channel();
        child_tx
            .send(AgentEvent::AssistantTurnComplete {
                message: Message::assistant(vec![ContentBlock::Text {
                    text: "Half an answer.".into(),
                }]),
                stop_reason: StopReason::EndTurn,
            })
            .unwrap();
        child_tx
            .send(AgentEvent::TurnLimitReached {
                kind: crate::event::TurnLimitKind::ToolCalls,
                detail: "reached the limit of 30 tool calls in one turn".into(),
            })
            .unwrap();
        child_tx.send(AgentEvent::Error("boom".into())).unwrap();
        drop(child_tx);

        let report = relay_child("x", "call_1", child_rx, &parent_tx).await;
        assert_eq!(report.report, "Half an answer.");
        assert_eq!(report.error.as_deref(), Some("boom"));
        assert!(report.limit.unwrap().contains("30 tool calls"));
    }

    #[tokio::test]
    async fn a_child_asking_permission_is_refused_rather_than_prompting_anyone() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(refuse_permissions(rx));
        let (respond_to, answer) = oneshot::channel();
        tx.send(PermissionAsk {
            request: crate::event::PermissionRequest {
                tool_call_id: "c1".into(),
                tool_name: "run_bash".into(),
                detail: "Run `rm -rf /`".into(),
            },
            respond_to,
        })
        .unwrap();
        assert_eq!(answer.await.unwrap(), PermissionDecision::Deny);
    }

    #[tokio::test]
    async fn a_child_asking_the_user_a_question_gets_a_refusal_not_an_invented_answer() {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(refuse_questions(rx));
        let (respond_to, answer) = oneshot::channel();
        tx.send(QuestionAsk {
            question: UserQuestion {
                id: "c1".into(),
                prompt: "Which one?".into(),
                options: ["a".into(), "b".into(), "c".into()],
            },
            respond_to,
        })
        .unwrap();
        let err = answer.await.unwrap().unwrap_err();
        assert!(err.contains("no user to ask"), "{err}");
    }

    #[test]
    fn an_executor_with_no_read_only_tools_yields_an_empty_set_rather_than_a_panic() {
        let (allowed, refused) = resolve_tool_set(&NoTools, None);
        assert!(allowed.is_empty());
        assert!(refused.is_empty());
    }
}
