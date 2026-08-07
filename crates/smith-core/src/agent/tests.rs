use super::*;
// `use super::*` reaches only what `agent.rs` itself still names. Everything
// the split moved into a sibling module — and every type the parent no longer
// mentions — has to be imported here by its own path. These go away section by
// section as the tests move next to their subject.
use super::fallback::resolve_tool_name;
use super::reasoning::ReasoningFilter;
use super::subagents::finish_subagent;
use super::tools::MAX_CONCURRENT_TOOLS;
use crate::context::estimate_messages_tokens;
use crate::event::{AgentEvent, PermissionDecision, TaskStatus, TurnLimitKind};
use crate::message::{Role, StopReason, StreamEvent, ToolDefinition};
use crate::provider::{ProviderCapabilities, ProviderError};
use crate::testkit::{
    empty_reply, text_reply, text_reply_with_usage, tool_call_reply, tool_calls_reply,
    ScriptedProvider, ScriptedResponse,
};
use crate::tool::{PermissionClass, ToolResult};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod accounting;
mod checkpointing;
mod compaction;
mod concurrency;
mod fallback;
mod hooks;
mod limits;
mod reasoning;
mod subagents;
mod turn;

// Fixtures used by more than one section below. A fixture used by
// exactly one lives in that section's file instead.

/// Proposes calling `write_file` (a Mutating tool), then ends the turn
/// with plain text once its result comes back.
fn write_file_then_done() -> ScriptedProvider {
    ScriptedProvider::tool_call_then_text("call_1", "write_file", serde_json::json!({}), "done")
}

/// Classifies `write_file` as Mutating and records whether it was ever
/// actually invoked.
struct RecordingTools {
    executed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ToolExecutor for RecordingTools {
    fn tool_defs(&self) -> Vec<crate::message::ToolDefinition> {
        Vec::new()
    }

    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        Some(PermissionClass::Mutating)
    }

    async fn execute(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        self.executed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        ToolResult::ok("wrote")
    }
}

/// Tool definitions with no declared properties — enough for the
/// name-matching tests, which never look at a schema.
fn defs(names: &[&str]) -> Vec<ToolDefinition> {
    names
        .iter()
        .map(|name| ToolDefinition {
            name: (*name).to_string(),
            description: "test tool".into(),
            input_schema: serde_json::json!({"type": "object"}),
        })
        .collect()
}

/// One definition that actually declares its arguments — what
/// `align_arguments` keys off.
fn def_with_properties(name: &str, properties: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: "test tool".into(),
        input_schema: serde_json::json!({"type": "object", "properties": properties}),
    }
}

/// Like `RecordingTools`, but advertises real `tool_defs()` entries under
/// caller-chosen names — needed so `recover_text_tool_call`'s tool-name
/// resolution has something to resolve against. Records every dispatch, so
/// a test can assert not just *that* something ran but *which* tool did.
struct RecordingToolsNamed {
    defs: Vec<ToolDefinition>,
    executed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    calls: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

impl RecordingToolsNamed {
    fn new(
        defs: Vec<ToolDefinition>,
        executed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            defs,
            executed,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ToolExecutor for RecordingToolsNamed {
    fn tool_defs(&self) -> Vec<ToolDefinition> {
        self.defs.clone()
    }

    fn permission_class(&self, _name: &str) -> Option<PermissionClass> {
        Some(PermissionClass::Mutating)
    }

    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        _ctx: &ToolContext,
        _cancel: CancellationToken,
    ) -> ToolResult {
        self.executed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.calls
            .lock()
            .unwrap()
            .push((name.to_string(), input.clone()));
        ToolResult::ok("wrote")
    }
}

/// For the tests below, which only inspect agent state and never run a
/// turn — hence the empty script.
fn fake_agent() -> Agent {
    let provider = Arc::new(ScriptedProvider::streams([]));
    let tools = Arc::new(NoTools);
    let tool_ctx = ToolContext::new(".", "test-session");
    Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
}

/// Collects `tool_use` ids (`want_use`) or `tool_result` ids from history.
fn collect_ids(history: &[Message], want_use: bool) -> Vec<String> {
    history
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } if want_use => Some(id.clone()),
            ContentBlock::ToolResult { tool_use_id, .. } if !want_use => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

fn api_error(status: u16, retry_after: Option<Duration>) -> ProviderError {
    ProviderError::Api {
        status,
        message: "boom".into(),
        retry_after,
    }
}

fn agent_for(provider: Arc<ScriptedProvider>, tools: Arc<dyn ToolExecutor>) -> Agent {
    Agent::new(
        provider,
        tools,
        "fake-model".to_string(),
        ToolContext::new(".", "test-session"),
    )
    .with_permission_policy(PermissionPolicy::Skip)
}

/// Runs one turn against throwaway channels and hands back everything the
/// turn emitted, so a test can assert on the event stream as a whole.
async fn run_collect(
    agent: &mut Agent,
    text: &str,
    cancel: CancellationToken,
) -> (bool, Vec<AgentEvent>) {
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();
    let completed = agent
        .run_turn(text.to_string(), events_tx, perm_tx, question_tx, cancel)
        .await;
    let events = std::iter::from_fn(|| events_rx.try_recv().ok()).collect();
    (completed, events)
}

fn errors(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Error(e) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

fn json_empty() -> serde_json::Value {
    serde_json::json!({})
}

fn tool_result_for(history: &[Message], id: &str) -> String {
    history
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == id => Some(content.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{id} was never answered"))
}

fn window_of(context_window: u32) -> ProviderCapabilities {
    ProviderCapabilities {
        context_window,
        ..ProviderCapabilities::default()
    }
}

fn prompt_usage(input_tokens: u32) -> Usage {
    Usage {
        input_tokens,
        ..Usage::default()
    }
}

fn context_events(events: &[AgentEvent]) -> Vec<(u32, u32, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ContextUsage {
                used,
                window,
                estimated,
            } => Some((*used, *window, *estimated)),
            _ => None,
        })
        .collect()
}
