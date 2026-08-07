//! Driving one provider request: retry, then drain the stream.

use futures::stream::BoxStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::AgentEvent;
use crate::message::{CompletionRequest, ContentBlock, Message, StopReason, StreamEvent, Usage};
use crate::provider::ProviderError;

use super::reasoning::ReasoningFilter;
use super::Agent;

/// One provider response, drained.
pub(super) struct StreamOutcome {
    pub(super) message: Message,
    pub(super) stop_reason: StopReason,
    pub(super) usage: Usage,
    /// Reasoning tags removed from the text channel on the way through — see
    /// [`ReasoningFilter`].
    pub(super) reasoning_tags_stripped: u32,
}

/// Drains a provider's StreamEvent stream, forwarding text deltas as AgentEvents
/// as they arrive and accumulating everything into a final assistant Message.
///
/// The `Usage` is returned as well as forwarded on the event channel: the
/// caller needs it for context and cost accounting, and reading it back off a
/// channel it also owns would be a race.
pub(super) async fn consume_stream(
    mut stream: futures::stream::BoxStream<
        'static,
        Result<StreamEvent, crate::provider::ProviderError>,
    >,
    events: &mpsc::UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> Result<StreamOutcome, String> {
    use futures::StreamExt;

    let mut text = String::new();
    let mut tool_uses: Vec<(String, String, String)> = Vec::new(); // id, name, accumulated json
    let mut current_tool: Option<usize> = None;
    let mut stop_reason = StopReason::EndTurn;
    let mut total_usage = Usage::default();
    let mut reasoning = ReasoningFilter::new();

    loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                stop_reason = StopReason::Cancelled;
                break;
            }
            item = stream.next() => match item {
                Some(item) => item,
                None => break,
            },
        };
        match item.map_err(|e| e.to_string())? {
            StreamEvent::TextDelta(delta) => {
                // Filtered *before* it is forwarded, not just before it is
                // stored: the transcript is built from these deltas, so
                // stripping only the accumulated copy would still put the tags
                // on screen.
                let visible = reasoning.push(&delta);
                if !visible.is_empty() {
                    text.push_str(&visible);
                    let _ = events.send(AgentEvent::AssistantTextDelta(visible));
                }
            }
            StreamEvent::ToolUseStart { id, name } => {
                tool_uses.push((id, name, String::new()));
                current_tool = Some(tool_uses.len() - 1);
            }
            StreamEvent::ToolUseInputDelta { partial_json, .. } => {
                if let Some(idx) = current_tool {
                    tool_uses[idx].2.push_str(&partial_json);
                }
            }
            StreamEvent::ToolUseComplete { .. } => {
                current_tool = None;
            }
            StreamEvent::MessageComplete {
                stop_reason: sr,
                usage,
            } => {
                stop_reason = sr;
                total_usage.add(&usage);
                let _ = events.send(AgentEvent::TokenUsage(usage));
            }
            StreamEvent::Error(e) => return Err(e),
        }
    }

    let tail = reasoning.finish();
    if !tail.is_empty() {
        text.push_str(&tail);
        let _ = events.send(AgentEvent::AssistantTextDelta(tail));
    }
    // Removing a block leaves the blank lines that framed it. Only trimmed
    // when something was actually removed, so an untouched reply keeps
    // whatever whitespace the model chose.
    if reasoning.stripped > 0 {
        text = text.trim().to_string();
    }

    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    for (id, name, json) in tool_uses {
        let input = if json.is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&json).unwrap_or(serde_json::json!({}))
        };
        content.push(ContentBlock::ToolUse { id, name, input });
    }

    Ok(StreamOutcome {
        message: Message::assistant(content),
        stop_reason,
        usage: total_usage,
        reasoning_tags_stripped: reasoning.stripped,
    })
}

impl Agent {
    /// Opens the completion stream, re-sending on failures worth re-sending.
    ///
    /// Only the *request* is retried, never a stream that already started:
    /// by then text deltas have reached the transcript, and replaying the
    /// request would duplicate the model's output on screen and in history.
    /// A mid-stream failure surfaces as an error, same as before.
    pub(super) async fn stream_with_retry(
        &self,
        request: CompletionRequest,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let mut attempt: u32 = 1;
        loop {
            let error = match self
                .provider
                .stream_completion(request.clone(), cancel.clone())
                .await
            {
                Ok(stream) => return Ok(stream),
                Err(e) => e,
            };

            let Some(delay) = self.retry_policy.delay_for(&error, attempt) else {
                return Err(error);
            };

            let _ = events.send(AgentEvent::ProviderRetry {
                attempt,
                max_attempts: self.retry_policy.max_attempts,
                delay_ms: delay.as_millis() as u64,
                reason: error.to_string(),
            });

            // Esc during a backoff has to take effect now, not when the timer
            // happens to expire: the whole point of showing the wait is that
            // the user can decide not to sit through it.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                _ = (self.sleeper)(delay) => {}
            }
            attempt += 1;
        }
    }
}
