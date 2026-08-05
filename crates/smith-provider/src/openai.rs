use std::collections::HashMap;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::StreamExt;
use smith_core::{
    CompletionRequest, ContentBlock, LlmProvider, Message, ProviderError, Role, StopReason,
    StreamEvent, ToolDefinition, Usage,
};
use tokio_util::sync::CancellationToken;

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Implements LlmProvider against OpenAI's Chat Completions API. Also reused
/// for Ollama's OpenAI-compatible endpoint by pointing `base_url` elsewhere.
pub struct OpenAiProvider {
    api_key: String,
    client: reqwest::Client,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Ollama serves an OpenAI-compatible endpoint and ignores the bearer
    /// token, so a placeholder key is enough.
    pub fn ollama(base_url: impl Into<String>) -> Self {
        Self::new("ollama".to_string()).with_base_url(base_url)
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &'static str {
        "openai"
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let body = build_request_body(&request);

        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api(format!("{status}: {text}")));
        }

        let mut events = resp.bytes_stream().eventsource();

        let stream = async_stream::stream! {
            let mut state = SseState::default();

            while let Some(ev) = events.next().await {
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(e) => {
                        yield Err(ProviderError::Http(e.to_string()));
                        break;
                    }
                };
                if ev.data.is_empty() {
                    continue;
                }
                if ev.data.trim() == "[DONE]" {
                    yield Ok(StreamEvent::MessageComplete { stop_reason: state.stop_reason, usage: state.usage });
                    break;
                }
                let payload: serde_json::Value = match serde_json::from_str(&ev.data) {
                    Ok(v) => v,
                    Err(e) => {
                        yield Err(ProviderError::Parse(e.to_string()));
                        continue;
                    }
                };
                for out in parse_chunk(&payload, &mut state) {
                    yield Ok(out);
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

#[derive(Default)]
struct SseState {
    /// tool_calls[].index -> id, populated on the chunk that first names a call.
    index_to_id: HashMap<u64, String>,
    stop_reason: StopReason,
    usage: Usage,
}

/// Translates one OpenAI `chat.completion.chunk` payload into zero or more
/// normalized StreamEvents. The `[DONE]` sentinel is handled by the caller,
/// not here, since it isn't valid JSON.
fn parse_chunk(payload: &serde_json::Value, state: &mut SseState) -> Vec<StreamEvent> {
    let mut out = Vec::new();

    if let Some(usage) = payload.get("usage").filter(|v| !v.is_null()) {
        if let Some(v) = usage.get("prompt_tokens").and_then(|v| v.as_u64()) {
            state.usage.input_tokens = v as u32;
        }
        if let Some(v) = usage.get("completion_tokens").and_then(|v| v.as_u64()) {
            state.usage.output_tokens = v as u32;
        }
    }

    let Some(choice) = payload.get("choices").and_then(|c| c.get(0)) else {
        return out;
    };

    if let Some(delta) = choice.get("delta") {
        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(StreamEvent::TextDelta(text.to_string()));
            }
        }
        if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            for call in calls {
                let index = call.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                    let name = call
                        .pointer("/function/name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    state.index_to_id.insert(index, id.to_string());
                    out.push(StreamEvent::ToolUseStart {
                        id: id.to_string(),
                        name,
                    });
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(|v| v.as_str()) {
                    if let Some(id) = state.index_to_id.get(&index) {
                        out.push(StreamEvent::ToolUseInputDelta {
                            id: id.clone(),
                            partial_json: args.to_string(),
                        });
                    }
                }
            }
        }
    }

    if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        state.stop_reason = match reason {
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };
        // Drain rather than iterate: the map is per-stream state, and a
        // provider that sends more than one `finish_reason` (or a caller that
        // reuses the state) would otherwise re-announce every tool call it
        // has ever seen, completing ids that were already closed.
        for (_, id) in std::mem::take(&mut state.index_to_id) {
            out.push(StreamEvent::ToolUseComplete { id });
        }
    }

    out
}

fn build_request_body(request: &CompletionRequest) -> serde_json::Value {
    let messages = messages_to_wire(&request.messages, request.system.as_deref());

    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.max_tokens,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if !request.tools.is_empty() {
        body["tools"] =
            serde_json::Value::Array(request.tools.iter().map(tool_def_to_wire).collect());
    }

    body
}

fn tool_def_to_wire(def: &ToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": def.name,
            "description": def.description,
            "parameters": def.input_schema,
        }
    })
}

/// OpenAI has no `tool_result` content block: each ToolResult becomes its own
/// `role: "tool"` message, and ToolUse blocks fold into a single assistant
/// message's `tool_calls` array alongside any text.
fn messages_to_wire(messages: &[Message], system: Option<&str>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Some(system) = system {
        out.push(serde_json::json!({ "role": "system", "content": system }));
    }

    for message in messages {
        match message.role {
            Role::User => {
                let mut text = String::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            out.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
                if !text.is_empty() {
                    out.push(serde_json::json!({ "role": "user", "content": text }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": input.to_string() },
                            }));
                        }
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
                let mut wire = serde_json::json!({
                    "role": "assistant",
                    "content": if text.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(text) },
                });
                if !tool_calls.is_empty() {
                    wire["tool_calls"] = serde_json::Value::Array(tool_calls);
                }
                out.push(wire);
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_request_body_with_system_and_tools() {
        let request = CompletionRequest {
            model: "gpt-4.1".into(),
            system: Some("be terse".into()),
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "reads a file".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 1024,
            temperature: None,
        };

        let body = build_request_body(&request);
        assert_eq!(body["model"], "gpt-4.1");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be terse");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn tool_result_message_becomes_tool_role_messages() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "42".into(),
                is_error: false,
            }],
        }];
        let wire = messages_to_wire(&messages, None);
        assert_eq!(wire[0]["role"], "tool");
        assert_eq!(wire[0]["tool_call_id"], "call_1");
        assert_eq!(wire[0]["content"], "42");
    }

    #[test]
    fn parses_text_delta_chunk() {
        let mut state = SseState::default();
        let chunk = serde_json::json!({
            "choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": null}]
        });
        let events = parse_chunk(&chunk, &mut state);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "hi"));
    }

    #[test]
    fn parses_tool_call_chunks_and_finish() {
        let mut state = SseState::default();
        let start = serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "read_file", "arguments": ""}}]}, "finish_reason": null}]
        });
        let arg_delta = serde_json::json!({
            "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":\"a.txt\"}"}}]}, "finish_reason": null}]
        });
        let finish = serde_json::json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
        });

        let started = parse_chunk(&start, &mut state);
        assert!(
            matches!(&started[0], StreamEvent::ToolUseStart { id, name } if id == "call_1" && name == "read_file")
        );

        let delta = parse_chunk(&arg_delta, &mut state);
        assert!(
            matches!(&delta[0], StreamEvent::ToolUseInputDelta { id, partial_json } if id == "call_1" && partial_json.contains("a.txt"))
        );

        let finished = parse_chunk(&finish, &mut state);
        assert!(matches!(&finished[0], StreamEvent::ToolUseComplete { id } if id == "call_1"));
        assert_eq!(state.stop_reason, StopReason::ToolUse);
    }
}
