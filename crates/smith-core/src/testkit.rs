//! Test doubles for driving the agent loop without a real provider.
//!
//! Compiled for this crate's own tests, and — behind the `testkit` feature —
//! for tests in crates downstream of `smith-core`, which cannot see an
//! inline `#[cfg(test)]` module. A dev-dependency that turns the feature on
//! does not leak it into normal builds under resolver 2, so nothing here
//! reaches a release binary.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio_util::sync::CancellationToken;

use crate::message::{CompletionRequest, StopReason, StreamEvent, Usage};
use crate::provider::{LlmProvider, ProviderError};

/// What a [`ScriptedProvider`] does for one request.
pub enum ScriptedResponse {
    /// Replay these events as a successful stream.
    Stream(Vec<StreamEvent>),
    /// Fail the request before any events arrive — a transport or API error.
    /// An error *part-way through* a stream is `StreamEvent::Error` inside a
    /// `Stream` instead; the agent handles the two differently.
    Fail(ProviderError),
}

/// An `LlmProvider` that replays a fixed script — one entry per request, in
/// order — and records every `CompletionRequest` it was handed.
///
/// It replaces a family of one-off fakes that differed only in which events
/// they emitted. Recording the requests is the other half of the point: it is
/// what lets a test assert on what was actually *sent* (system prompt,
/// message history, tool definitions), not just on what came back.
///
/// Running past the end of the script panics rather than improvising a reply:
/// a turn that makes more requests than the test expected is a finding, and a
/// silent default would hide it.
pub struct ScriptedProvider {
    responses: Mutex<VecDeque<ScriptedResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl ScriptedProvider {
    pub fn new(responses: impl IntoIterator<Item = ScriptedResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// One successful stream per request — the common case.
    pub fn streams(streams: impl IntoIterator<Item = Vec<StreamEvent>>) -> Self {
        Self::new(streams.into_iter().map(ScriptedResponse::Stream))
    }

    /// A single plain-text reply that ends the turn.
    pub fn text(text: &str) -> Self {
        Self::streams([text_reply(text)])
    }

    /// One tool call, then a text reply once its result comes back — the
    /// shape of nearly every tool-path test.
    pub fn tool_call_then_text(id: &str, name: &str, input: serde_json::Value, text: &str) -> Self {
        Self::streams([tool_call_reply(id, name, input), text_reply(text)])
    }

    /// A failed request followed by a successful one — for retry and backoff.
    pub fn error_then_text(error: ProviderError, text: &str) -> Self {
        Self::new([
            ScriptedResponse::Fail(error),
            ScriptedResponse::Stream(text_reply(text)),
        ])
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn last_request(&self) -> Option<CompletionRequest> {
        self.requests.lock().unwrap().last().cloned()
    }

    /// Script entries not yet consumed — a test that scripted more turns than
    /// ran can assert on this.
    pub fn remaining(&self) -> usize {
        self.responses.lock().unwrap().len()
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "scripted"
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let n = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len()
        };
        let response = self.responses.lock().unwrap().pop_front();

        match response {
            Some(ScriptedResponse::Stream(events)) => {
                Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
            }
            Some(ScriptedResponse::Fail(error)) => Err(error),
            None => panic!("ScriptedProvider ran out of responses on request #{n}"),
        }
    }
}

/// One request's worth of events: a text reply that ends the turn.
pub fn text_reply(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta(text.to_string()),
        StreamEvent::MessageComplete {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ]
}

/// A turn with no content at all — what a stalled provider looks like.
pub fn empty_reply() -> Vec<StreamEvent> {
    vec![StreamEvent::MessageComplete {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }]
}

/// One request's worth of events: a single tool call, no accompanying text.
pub fn tool_call_reply(id: &str, name: &str, input: serde_json::Value) -> Vec<StreamEvent> {
    tool_calls_reply(&[(id, name, input)])
}

/// Several tool calls in one assistant turn.
pub fn tool_calls_reply(calls: &[(&str, &str, serde_json::Value)]) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for (id, name, input) in calls {
        events.push(StreamEvent::ToolUseStart {
            id: (*id).to_string(),
            name: (*name).to_string(),
        });
        events.push(StreamEvent::ToolUseInputDelta {
            id: (*id).to_string(),
            partial_json: input.to_string(),
        });
        events.push(StreamEvent::ToolUseComplete {
            id: (*id).to_string(),
        });
    }
    events.push(StreamEvent::MessageComplete {
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    });
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    async fn drain(provider: &ScriptedProvider) -> Result<Vec<StreamEvent>, ProviderError> {
        let stream = provider
            .stream_completion(CompletionRequest::default(), CancellationToken::new())
            .await?;
        Ok(stream.map(|e| e.unwrap()).collect().await)
    }

    #[tokio::test]
    async fn replays_one_script_entry_per_request_in_order() {
        let provider = ScriptedProvider::streams([text_reply("first"), text_reply("second")]);

        let first = drain(&provider).await.unwrap();
        assert!(matches!(&first[0], StreamEvent::TextDelta(t) if t == "first"));
        let second = drain(&provider).await.unwrap();
        assert!(matches!(&second[0], StreamEvent::TextDelta(t) if t == "second"));
        assert_eq!(provider.remaining(), 0);
    }

    #[tokio::test]
    async fn records_every_request_it_was_handed() {
        let provider = ScriptedProvider::streams([text_reply("ok")]);
        assert_eq!(provider.request_count(), 0);

        let request = CompletionRequest {
            model: "some-model".into(),
            system: Some("be concise".into()),
            ..CompletionRequest::default()
        };
        // Recording happens on receipt, so the stream itself is beside the point.
        let _stream = provider
            .stream_completion(request, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(provider.request_count(), 1);
        let last = provider.last_request().unwrap();
        assert_eq!(last.model, "some-model");
        assert_eq!(last.system.as_deref(), Some("be concise"));
    }

    #[tokio::test]
    async fn a_failed_request_is_followed_by_the_next_scripted_entry() {
        let provider =
            ScriptedProvider::error_then_text(ProviderError::Http("boom".into()), "recovered");

        let err = drain(&provider).await.unwrap_err();
        assert!(matches!(err, ProviderError::Http(_)));

        let events = drain(&provider).await.unwrap();
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "recovered"));
        // A failed request still counts — retry tests assert on this.
        assert_eq!(provider.request_count(), 2);
    }

    #[tokio::test]
    async fn tool_call_reply_carries_the_input_as_parseable_json() {
        let events = tool_call_reply("call_1", "write_file", serde_json::json!({"path": "a.txt"}));
        let json: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolUseInputDelta { partial_json, .. } => Some(partial_json.clone()),
                _ => None,
            })
            .collect();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["path"], "a.txt");
        assert!(matches!(
            events.last(),
            Some(StreamEvent::MessageComplete {
                stop_reason: StopReason::ToolUse,
                ..
            })
        ));
    }

    #[tokio::test]
    #[should_panic(expected = "ran out of responses")]
    async fn running_past_the_end_of_the_script_is_a_test_failure_not_a_default_reply() {
        let provider = ScriptedProvider::streams([text_reply("only one")]);
        drain(&provider).await.unwrap();
        let _ = drain(&provider).await;
    }
}
