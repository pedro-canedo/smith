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

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    api_key: String,
    client: reqwest::Client,
    base_url: String,
}

impl AnthropicProvider {
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
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let body = build_request_body(&request);

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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
                let payload: serde_json::Value = match serde_json::from_str(&ev.data) {
                    Ok(v) => v,
                    Err(e) => {
                        yield Err(ProviderError::Parse(e.to_string()));
                        continue;
                    }
                };
                for out in parse_sse_payload(&payload, &mut state) {
                    yield Ok(out);
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

#[derive(Default)]
struct SseState {
    tool_index_to_id: HashMap<usize, String>,
    usage: Usage,
    stop_reason: StopReason,
}

/// Translates one Anthropic SSE event payload into zero or more normalized
/// StreamEvents, threading index->tool_use_id and usage/stop_reason state
/// across calls (Anthropic streams those incrementally across several events).
fn parse_sse_payload(payload: &serde_json::Value, state: &mut SseState) -> Vec<StreamEvent> {
    let event_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut out = Vec::new();

    match event_type {
        "message_start" => {
            if let Some(tokens) = payload
                .pointer("/message/usage/input_tokens")
                .and_then(|v| v.as_u64())
            {
                state.usage.input_tokens = tokens as u32;
            }
        }
        "content_block_start" => {
            let index = payload.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(block) = payload.get("content_block") {
                if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    state.tool_index_to_id.insert(index, id.clone());
                    out.push(StreamEvent::ToolUseStart { id, name });
                }
            }
        }
        "content_block_delta" => {
            let index = payload.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(delta) = payload.get("delta") {
                match delta.get("type").and_then(|v| v.as_str()) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                            out.push(StreamEvent::TextDelta(text.to_string()));
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(id), Some(partial)) = (
                            state.tool_index_to_id.get(&index),
                            delta.get("partial_json").and_then(|v| v.as_str()),
                        ) {
                            out.push(StreamEvent::ToolUseInputDelta {
                                id: id.clone(),
                                partial_json: partial.to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        "content_block_stop" => {
            let index = payload.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(id) = state.tool_index_to_id.remove(&index) {
                out.push(StreamEvent::ToolUseComplete { id });
            }
        }
        "message_delta" => {
            if let Some(sr) = payload
                .pointer("/delta/stop_reason")
                .and_then(|v| v.as_str())
            {
                state.stop_reason = match sr {
                    "tool_use" => StopReason::ToolUse,
                    "max_tokens" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };
            }
            if let Some(tokens) = payload
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
            {
                state.usage.output_tokens = tokens as u32;
            }
        }
        "message_stop" => {
            out.push(StreamEvent::MessageComplete {
                stop_reason: state.stop_reason,
                usage: state.usage,
            });
        }
        "error" => {
            let msg = payload
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error")
                .to_string();
            out.push(StreamEvent::Error(msg));
        }
        _ => {}
    }

    out
}

fn build_request_body(request: &CompletionRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request.messages.iter().map(message_to_wire).collect();

    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "max_tokens": request.max_tokens,
        "stream": true,
    });

    if let Some(system) = &request.system {
        body["system"] = serde_json::Value::String(system.clone());
    }
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
        "name": def.name,
        "description": def.description,
        "input_schema": def.input_schema,
    })
}

fn message_to_wire(message: &Message) -> serde_json::Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<serde_json::Value> =
        message.content.iter().map(content_block_to_wire).collect();
    serde_json::json!({ "role": role, "content": content })
}

fn content_block_to_wire(block: &ContentBlock) -> serde_json::Value {
    match block {
        ContentBlock::Text { text } => serde_json::json!({ "type": "text", "text": text }),
        ContentBlock::ToolUse { id, name, input } => {
            serde_json::json!({ "type": "tool_use", "id": id, "name": name, "input": input })
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_request_body_with_system_and_tools() {
        let request = CompletionRequest {
            model: "claude-sonnet-5".into(),
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
        assert_eq!(body["model"], "claude-sonnet-5");
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["type"], "text");
        assert_eq!(body["tools"][0]["name"], "read_file");
    }

    #[test]
    fn parses_text_delta_stream() {
        let mut state = SseState::default();
        let start = serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}});
        let delta = serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hello"}});
        let stop = serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}});
        let msg_stop = serde_json::json!({"type": "message_stop"});

        assert!(parse_sse_payload(&start, &mut state).is_empty());
        let deltas = parse_sse_payload(&delta, &mut state);
        assert!(matches!(&deltas[0], StreamEvent::TextDelta(t) if t == "hello"));
        assert!(parse_sse_payload(&stop, &mut state).is_empty());
        let complete = parse_sse_payload(&msg_stop, &mut state);
        assert!(
            matches!(&complete[0], StreamEvent::MessageComplete { stop_reason: StopReason::EndTurn, usage } if usage.output_tokens == 3)
        );
    }

    #[test]
    fn parses_tool_use_stream() {
        let mut state = SseState::default();
        let start = serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file"}});
        let delta1 = serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"}});
        let delta2 = serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"a.txt\"}"}});
        let stop = serde_json::json!({"type": "content_block_stop", "index": 0});

        let started = parse_sse_payload(&start, &mut state);
        assert!(
            matches!(&started[0], StreamEvent::ToolUseStart { id, name } if id == "toolu_1" && name == "read_file")
        );

        let d1 = parse_sse_payload(&delta1, &mut state);
        assert!(
            matches!(&d1[0], StreamEvent::ToolUseInputDelta { id, partial_json } if id == "toolu_1" && partial_json == "{\"path\":")
        );

        let d2 = parse_sse_payload(&delta2, &mut state);
        assert!(matches!(&d2[0], StreamEvent::ToolUseInputDelta { .. }));

        let completed = parse_sse_payload(&stop, &mut state);
        assert!(matches!(&completed[0], StreamEvent::ToolUseComplete { id } if id == "toolu_1"));
    }
}
