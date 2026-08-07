//! The tools whose result comes from the UI rather than computation.

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, AgentPhase, Task, TaskStatus, UserQuestion};
use crate::tool::ToolResult;

use super::executor::QuestionAsk;
use super::Agent;

/// Parses a `write_tasks` call's `{"tasks": [...]}` input into `Task`s.
/// Exposed (not just used internally) so a resumed session can rebuild its
/// checklist from the last `write_tasks` call in persisted history.
pub fn parse_tasks(input: &serde_json::Value) -> Result<Vec<Task>, String> {
    let items = input
        .get("tasks")
        .and_then(|v| v.as_array())
        .ok_or("write_tasks requires a non-empty `tasks` array")?;
    if items.is_empty() {
        return Err("write_tasks requires a non-empty `tasks` array".into());
    }

    items
        .iter()
        .map(|item| {
            let content = item
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if content.is_empty() {
                return Err("each task requires a non-empty `content` string".into());
            }
            let status = match item.get("status").and_then(|v| v.as_str()) {
                Some("pending") => TaskStatus::Pending,
                Some("in_progress") => TaskStatus::InProgress,
                Some("blocked") => TaskStatus::Blocked,
                Some("review") => TaskStatus::Review,
                Some("completed") => TaskStatus::Completed,
                Some(other) => {
                    return Err(format!(
                        "unknown task status `{other}` — use pending, in_progress, blocked, \
                         review, or completed"
                    ))
                }
                None => return Err("each task requires a `status`".into()),
            };
            let text_field = |key: &str| {
                item.get(key)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            Ok(Task {
                content,
                status,
                id: text_field("id"),
                blocked_reason: text_field("blocked_reason"),
                // Deliberately not read from the input: the stamp is smith's,
                // never the model's — see `stamp_tasks`.
                updated_at: None,
            })
        })
        .collect()
}

/// Gives every card an identity and an honest timestamp.
///
/// Positional `"t{n}"` ids fill in only where the model sent none — the tool
/// schema asks it to echo ids back, and a model that does gets stable
/// identity across full-list replacements for free. `updated_at` is carried
/// over from the previous snapshot's card with the same id when nothing
/// visible changed, and set to `now_ms` otherwise, so a board can sort or
/// fade by recency without every card claiming to be new on every rewrite.
pub(super) fn stamp_tasks(previous: &[Task], mut incoming: Vec<Task>, now_ms: u64) -> Vec<Task> {
    for (index, task) in incoming.iter_mut().enumerate() {
        if task.id.is_none() {
            task.id = Some(format!("t{}", index + 1));
        }
        let prior = previous.iter().find(|p| p.id == task.id);
        task.updated_at = match prior {
            Some(p)
                if p.content == task.content
                    && p.status == task.status
                    && p.blocked_reason == task.blocked_reason =>
            {
                p.updated_at
            }
            _ => Some(now_ms),
        };
    }
    incoming
}

impl Agent {
    pub(super) async fn run_ask_user(
        &mut self,
        id: &str,
        input: serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
        question_tx: &mpsc::UnboundedSender<QuestionAsk>,
        cancel: CancellationToken,
    ) -> ToolResult {
        let prompt = input
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let opt = |k: &str| {
            input
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let options = [opt("option_a"), opt("option_b"), opt("option_c")];
        if prompt.is_empty() || options.iter().any(|o| o.is_empty()) {
            return ToolResult::error(
                "ask_user requires question, option_a, option_b, and option_c (all non-empty)",
            );
        }

        let question = UserQuestion {
            id: id.to_string(),
            prompt,
            options: options.clone(),
        };

        let _ = events.send(AgentEvent::PhaseChanged(AgentPhase::Asking));
        let _ = events.send(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: "ask_user".into(),
            input: input.clone(),
        });
        let _ = events.send(AgentEvent::UserQuestionNeeded(question.clone()));

        let (tx, rx) = oneshot::channel();
        if question_tx
            .send(QuestionAsk {
                question,
                respond_to: tx,
            })
            .is_err()
        {
            let result = ToolResult::error("question channel closed");
            let _ = events.send(AgentEvent::ToolCallResult {
                id: id.to_string(),
                output: result.content.clone(),
                is_error: true,
            });
            return result;
        }

        let answer = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let result = ToolResult::error("question cancelled");
                let _ = events.send(AgentEvent::ToolCallResult {
                    id: id.to_string(),
                    output: result.content.clone(),
                    is_error: true,
                });
                return result;
            }
            answer = rx => answer.unwrap_or_else(|_| Ok("User dismissed the question.".into())),
        };

        let result = match answer {
            Ok(answer) => ToolResult::ok(format!("User answered: {answer}")),
            Err(reason) => ToolResult::error(reason),
        };
        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: result.is_error,
        });
        result
    }

    pub(super) async fn run_write_tasks(
        &mut self,
        id: &str,
        input: serde_json::Value,
        events: &mpsc::UnboundedSender<AgentEvent>,
    ) -> ToolResult {
        let _ = events.send(AgentEvent::ToolCallStarted {
            id: id.to_string(),
            tool_name: "write_tasks".into(),
            input: input.clone(),
        });

        let result = match parse_tasks(&input) {
            Ok(tasks) => {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or_default();
                let tasks = stamp_tasks(&self.tasks, tasks, now_ms);
                self.tasks = tasks.clone();
                let _ = events.send(AgentEvent::TasksUpdated(tasks));
                ToolResult::ok("tasks updated")
            }
            Err(e) => ToolResult::error(e),
        };

        let _ = events.send(AgentEvent::ToolCallResult {
            id: id.to_string(),
            output: result.content.clone(),
            is_error: result.is_error,
        });
        result
    }
}
