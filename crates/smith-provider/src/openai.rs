use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::BoxStream;
use futures::StreamExt;
use smith_core::provider::ProviderCapabilities;
use smith_core::{
    CompletionRequest, ContentBlock, LlmProvider, Message, ProviderError, Role, StopReason,
    StreamEvent, ToolDefinition, Usage,
};
use tokio_util::sync::CancellationToken;

use crate::{api_error, http_client};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Which catalogue `capabilities()` should consult. The wire format is shared,
/// but the models behind it are not: an Ollama server hosts arbitrary local
/// weights whose window has nothing to do with OpenAI's hosted models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    OpenAi,
    Ollama,
    OpenRouter,
    NineRouter,
}

/// Context window and max-output tokens per model, matched by prefix so dated
/// or suffixed ids resolve to their base model.
const OPENAI_MODEL_LIMITS: &[(&str, u32, u32)] = &[
    ("gpt-4.1-mini", 1_047_576, 32_768),
    ("gpt-4.1", 1_047_576, 32_768),
    ("gpt-4o", 128_000, 16_384),
    ("o3", 200_000, 100_000),
];

/// Ollama's own default `num_ctx`.
///
/// The real window is whatever the *local* server allocates, which depends on
/// the Modelfile, the `num_ctx` override, and how much VRAM the box has — none
/// of which is discoverable from this side of the OpenAI-compatible endpoint.
/// So we report what Ollama allocates by default rather than the theoretical
/// maximum of the underlying weights: llama3.3 and qwen2.5 advertise 128K, but
/// an unconfigured `ollama serve` silently truncates the prompt to 4096 and the
/// model answers as if the rest of the conversation never happened. Guessing
/// 128K here would let compaction sail past a limit that is really 4096 —
/// exactly the failure this default exists to prevent.
const OLLAMA_CONTEXT_WINDOW: u32 = 4096;
/// Floor for OpenRouter/9Router models whose real window has not been probed
/// yet. Higher than Ollama's because these are hosted models, never a local
/// `num_ctx 4096` truncation — but still far below what most free models
/// advertise, for the same reason Ollama's guess is low: overclaiming lets
/// compaction sail past a real limit.
const GATEWAY_CONTEXT_WINDOW: u32 = 32_768;
/// Pulls `num_ctx` out of Ollama's flat `parameters` blob (`"num_ctx 8192"`,
/// one setting per line).
fn parse_num_ctx(parameters: &str) -> Option<u32> {
    parameters.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == "num_ctx")
            .then(|| parts.next()?.parse().ok())
            .flatten()
    })
}

/// Extracts what smith needs from OpenRouter's `GET /models` payload:
/// `(model -> context_length, free models that support tools)`.
///
/// Pure, so the shapes OpenRouter actually returns are pinned by fixture
/// tests instead of live calls.
fn parse_openrouter_catalogue(value: &serde_json::Value) -> (HashMap<String, u32>, Vec<String>) {
    let mut windows = HashMap::new();
    let mut free_tools = Vec::new();
    let Some(data) = value.get("data").and_then(|d| d.as_array()) else {
        return (windows, free_tools);
    };
    for model in data {
        let Some(id) = model.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(window) = model
            .get("context_length")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
        {
            windows.insert(id.to_string(), window);
        }
        let supports_tools = model
            .get("supported_parameters")
            .and_then(|v| v.as_array())
            .is_some_and(|params| params.iter().any(|p| p.as_str() == Some("tools")));
        if id.ends_with(":free") && supports_tools {
            free_tools.push(id.to_string());
        }
    }
    (windows, free_tools)
}

/// Cap on the output half of the window.
///
/// Halving works at 4096; at 262144 it would claim a 131k reply, which no
/// model produces and some servers reject outright.
const OLLAMA_MAX_OUTPUT_CEILING: u32 = 32_768;

fn openai_capabilities(model: &str) -> ProviderCapabilities {
    let Some(&(id, context_window, max_output)) = OPENAI_MODEL_LIMITS
        .iter()
        .find(|(id, _, _)| model.starts_with(id))
    else {
        return ProviderCapabilities::default();
    };

    ProviderCapabilities {
        context_window,
        max_output,
        supports_tools: true,
        // OpenAI caches prefixes automatically, but there is no `cache_control`
        // equivalent for the caller to place — this flag means "caller can
        // address the cache", which here is false.
        supports_cache: false,
        // Only the o-series reasons; the GPT models have no thinking channel.
        supports_thinking: id == "o3",
        // No token-counting endpoint; any gauge would be a local estimate.
        supports_token_count: false,
    }
}

fn ollama_capabilities(window: u32) -> ProviderCapabilities {
    ProviderCapabilities {
        context_window: window,
        // Output is carved out of the same `num_ctx` budget, so leave half the
        // window for the prompt rather than claiming the whole thing — but
        // never more than a model will actually emit in one reply, or the
        // request asks for something the server refuses.
        max_output: (window / 2).min(OLLAMA_MAX_OUTPUT_CEILING),
        // Tool support is per-model on Ollama and not advertised over this
        // endpoint; smith already sends tools and lets the server ignore them.
        supports_tools: true,
        supports_cache: false,
        supports_thinking: false,
        supports_token_count: false,
    }
}

/// Implements LlmProvider against OpenAI's Chat Completions API. Also reused
/// for Ollama's OpenAI-compatible endpoint by pointing `base_url` elsewhere.
pub struct OpenAiProvider {
    api_key: String,
    client: reqwest::Client,
    base_url: String,
    flavor: Flavor,
    /// OpenRouter's server-side fallback chain, injected into the request
    /// body as `models` + `route: "fallback"`. Held on the provider rather
    /// than on `CompletionRequest`: it is session configuration, and the
    /// request structs are built in dozens of places that should not know
    /// an OpenRouter-specific concept exists.
    fallback_models: Vec<String>,
    /// `:free` models with tool support, from OpenRouter's catalogue.
    free_tool_models: Arc<RwLock<Vec<String>>>,
    /// Whether the OpenRouter catalogue has been fetched successfully.
    catalogue_warmed: Arc<std::sync::atomic::AtomicBool>,
    /// Context windows learned from Ollama's own `/api/show`, keyed by model.
    ///
    /// A fact beats the constant below every time it is available. Empty until
    /// `warm_capabilities` has run, and left empty when it fails — the guess is
    /// the fallback, not the answer.
    known_windows: Arc<RwLock<HashMap<String, u32>>>,
}

impl OpenAiProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: http_client(),
            base_url: DEFAULT_BASE_URL.to_string(),
            flavor: Flavor::OpenAi,
            fallback_models: Vec::new(),
            free_tool_models: Arc::new(RwLock::new(Vec::new())),
            catalogue_warmed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            known_windows: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The server-side chain for OpenRouter's `route: "fallback"`. Ignored by
    /// every other flavor.
    pub fn with_fallback_models(mut self, models: Vec<String>) -> Self {
        self.fallback_models = models;
        self
    }

    /// The wire body for one request: the shared OpenAI shape, plus — on the
    /// OpenRouter flavor with a configured chain — the server-side fallback.
    ///
    /// `models[0]` is always the request's own model, deduplicated against
    /// the chain, so what the agent asked for is what the server tries first
    /// and a `/model` switch reorders the chain instead of fighting it.
    fn request_body(&self, request: &CompletionRequest) -> serde_json::Value {
        let mut body = build_request_body(request);
        if self.flavor == Flavor::OpenRouter && !self.fallback_models.is_empty() {
            let mut chain: Vec<&str> = vec![request.model.as_str()];
            chain.extend(
                self.fallback_models
                    .iter()
                    .map(String::as_str)
                    .filter(|m| *m != request.model),
            );
            if chain.len() > 1 {
                body["models"] = serde_json::json!(chain);
                body["route"] = serde_json::json!("fallback");
            }
        }
        body
    }

    /// The window `/api/show` reported for this model, or the conservative
    /// guess if it was never asked or never answered.
    fn known_window(&self, model: &str) -> u32 {
        self.known_windows
            .read()
            .ok()
            .and_then(|known| known.get(model).copied())
            .unwrap_or(OLLAMA_CONTEXT_WINDOW)
    }

    /// Pulls OpenRouter's catalogue once and caches what smith needs from it:
    /// each model's context window, and which `:free` models can call tools.
    ///
    /// One request for the whole account, guarded by `catalogue_warmed` — the
    /// catalogue is not per-model, and `switch_model` calls the warm again.
    async fn warm_openrouter_catalogue(&self) {
        if self
            .catalogue_warmed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let Ok(response) = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
        else {
            // Best-effort by the trait contract: the conservative defaults
            // stand, and a session must start without the catalogue.
            self.catalogue_warmed
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };
        if !response.status().is_success() {
            self.catalogue_warmed
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        }
        let Ok(value) = response.json::<serde_json::Value>().await else {
            self.catalogue_warmed
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return;
        };

        let (windows, free_tools) = parse_openrouter_catalogue(&value);
        if let Ok(mut known) = self.known_windows.write() {
            known.extend(windows);
        }
        if let Ok(mut free) = self.free_tool_models.write() {
            *free = free_tools;
        }
    }

    /// The `:free` models that can call tools, from the last catalogue warm.
    /// Empty until warmed (or when the account genuinely has none) — the
    /// setup wizard treats empty as "fall back to the curated list".
    pub async fn list_free_tool_models(&self) -> Vec<String> {
        self.warm_openrouter_catalogue().await;
        self.free_tool_models
            .read()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Ollama's native API sits beside the OpenAI-compatible one: the base URL
    /// is `.../v1`, and `/api/show` hangs off its parent.
    fn ollama_api_root(&self) -> String {
        self.base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string()
    }

    /// Asks Ollama what this model's context length actually is.
    ///
    /// `parameters` wins over `model_info` when it names `num_ctx`: the
    /// architecture's context length is what the weights support, `num_ctx` is
    /// what this server allocated, and the smaller one is what the prompt is
    /// truncated to.
    async fn probe_ollama_window(&self, model: &str) -> Option<u32> {
        let url = format!("{}/api/show", self.ollama_api_root());
        let body = serde_json::json!({ "model": model });
        let response = self.client.post(&url).json(&body).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let value: serde_json::Value = response.json().await.ok()?;

        let architecture_window = value
            .get("model_info")
            .and_then(|info| info.as_object())
            .and_then(|info| {
                info.iter()
                    .find(|(key, _)| key.ends_with(".context_length"))
                    .and_then(|(_, v)| v.as_u64())
            })
            .and_then(|n| u32::try_from(n).ok());

        let allocated = value
            .get("parameters")
            .and_then(|p| p.as_str())
            .and_then(parse_num_ctx);

        match (architecture_window, allocated) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Ollama serves an OpenAI-compatible endpoint and ignores the bearer
    /// token, so a placeholder key is enough.
    pub fn ollama(base_url: impl Into<String>) -> Self {
        let mut provider = Self::new("ollama".to_string()).with_base_url(base_url);
        provider.flavor = Flavor::Ollama;
        provider
    }

    /// OpenRouter — the same wire format on a different host, plus the
    /// attribution headers and the `models`/`route` fallback body.
    pub fn openrouter(api_key: String, base_url: impl Into<String>) -> Self {
        let mut provider = Self::new(api_key).with_base_url(base_url);
        provider.flavor = Flavor::OpenRouter;
        provider
    }

    /// 9Router — a local gateway speaking the OpenAI wire format.
    pub fn nine_router(api_key: String, base_url: impl Into<String>) -> Self {
        let mut provider = Self::new(api_key).with_base_url(base_url);
        provider.flavor = Flavor::NineRouter;
        provider
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &'static str {
        // Flavor-driven, and that is a bug fix rather than plumbing: this id
        // is what `Agent::note_usage` prices against and what lands in the
        // `turns.provider` column, so the old hardcoded "openai" recorded
        // every Ollama session as an OpenAI one. The dollars were right (no
        // price row matched either way); the label was not.
        match self.flavor {
            Flavor::OpenAi => "openai",
            Flavor::Ollama => "ollama",
            Flavor::OpenRouter => "openrouter",
            Flavor::NineRouter => "9router",
        }
    }

    async fn warm_capabilities(&self, model: &str) {
        if self.flavor == Flavor::OpenRouter {
            self.warm_openrouter_catalogue().await;
            return;
        }
        if self.flavor != Flavor::Ollama {
            return;
        }
        let Some(window) = self.probe_ollama_window(model).await else {
            // Left unset on purpose: `known_window` then answers with the
            // conservative default, which is the right way to be wrong.
            return;
        };
        if let Ok(mut known) = self.known_windows.write() {
            known.insert(model.to_string(), window);
        }
    }

    fn capabilities(&self, model: &str) -> ProviderCapabilities {
        match self.flavor {
            Flavor::OpenAi => openai_capabilities(model),
            Flavor::Ollama => ollama_capabilities(self.known_window(model)),
            // Same shape as Ollama: a probed window when we have one (the
            // OpenRouter catalogue warm lands in a later phase), else a
            // conservative floor. Conservative is the safe way to be wrong —
            // it only costs an early compaction until the warm answers.
            Flavor::OpenRouter | Flavor::NineRouter => ollama_capabilities(
                self.known_windows
                    .read()
                    .ok()
                    .and_then(|known| known.get(model).copied())
                    .unwrap_or(GATEWAY_CONTEXT_WINDOW),
            ),
        }
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let body = self.request_body(&request);

        let mut req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key);
        if self.flavor == Flavor::OpenRouter {
            // Attribution, per OpenRouter's docs. Optional, but it is how an
            // app appears on their rankings page, and it costs two headers.
            req = req
                .header("HTTP-Referer", "https://github.com/pedro-canedo/smith")
                .header("X-Title", "smith");
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(api_error(resp).await);
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
        // OpenAI reports cached tokens as a *breakdown of* `prompt_tokens`,
        // the opposite of Anthropic, which reports them alongside it. So the
        // cached share is subtracted back out of `input_tokens` here: `Usage`'s
        // four fields are disjoint by contract, and `prompt_tokens()` adds
        // them — double-counting the cached prefix would over-report the
        // context and over-bill the turn.
        //
        // `cache_write` stays zero because OpenAI does not charge for, or
        // report, cache creation.
        if let Some(cached) = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(|v| v.as_u64())
        {
            let cached = cached as u32;
            state.usage.cache_read = cached;
            state.usage.input_tokens = state.usage.input_tokens.saturating_sub(cached);
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
    /// Fixture in OpenRouter's real `/models` shape (trimmed).
    fn catalogue_fixture() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "nvidia/nemotron-3-ultra-550b-a55b:free",
                    "context_length": 1_000_000,
                    "supported_parameters": ["tools", "temperature"]
                },
                {
                    // Free but no tools: a chatbot, not an agent — excluded.
                    "id": "somelab/chatty-7b:free",
                    "context_length": 32_768,
                    "supported_parameters": ["temperature"]
                },
                {
                    // Tools but paid: window still learned, not in the free list.
                    "id": "anthropic/claude-sonnet-5",
                    "context_length": 200_000,
                    "supported_parameters": ["tools"]
                },
                { "id": "broken/no-context" }
            ]
        })
    }

    #[test]
    fn the_catalogue_parser_learns_windows_and_filters_free_tool_models() {
        let (windows, free_tools) = parse_openrouter_catalogue(&catalogue_fixture());
        assert_eq!(
            windows.get("nvidia/nemotron-3-ultra-550b-a55b:free"),
            Some(&1_000_000)
        );
        assert_eq!(windows.get("anthropic/claude-sonnet-5"), Some(&200_000));
        assert!(!windows.contains_key("broken/no-context"));
        assert_eq!(free_tools, ["nvidia/nemotron-3-ultra-550b-a55b:free"]);
    }

    #[test]
    fn an_empty_or_alien_payload_parses_to_nothing_rather_than_panicking() {
        let (windows, free) = parse_openrouter_catalogue(&serde_json::json!({}));
        assert!(windows.is_empty() && free.is_empty());
        let (windows, free) = parse_openrouter_catalogue(&serde_json::json!({"data": "?"}));
        assert!(windows.is_empty() && free.is_empty());
    }

    /// Exercises the real catalogue endpoint. `/models` is public, so this
    /// needs no key — only the network.
    ///
    /// `cargo test -p smith-provider live_openrouter -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_openrouter_lists_free_tool_models() {
        let provider = OpenAiProvider::openrouter("unused".into(), "https://openrouter.ai/api/v1");
        let free = provider.list_free_tool_models().await;
        println!("free+tools: {free:?}");
        assert!(
            !free.is_empty(),
            "no free tool-capable models — the curated chain needs revisiting"
        );
        let caps = provider.capabilities(&free[0]);
        println!("{}: window {}", free[0], caps.context_window);
        assert!(caps.context_window > GATEWAY_CONTEXT_WINDOW);
    }

    /// Pins the id fix: this string prices turns and labels the
    /// `turns.provider` column, and it used to say "openai" for everyone.
    #[test]
    fn every_flavor_reports_its_own_id() {
        assert_eq!(OpenAiProvider::new("k".into()).id(), "openai");
        assert_eq!(OpenAiProvider::ollama("http://x/v1").id(), "ollama");
        assert_eq!(
            OpenAiProvider::openrouter("k".into(), "http://x/v1").id(),
            "openrouter"
        );
        assert_eq!(
            OpenAiProvider::nine_router("k".into(), "http://x/v1").id(),
            "9router"
        );
    }

    /// The chain rides the body only on OpenRouter, only when configured,
    /// and always with the request's own model first.
    #[test]
    fn the_fallback_chain_is_serialized_the_openrouter_way() {
        let request = CompletionRequest {
            model: "nvidia/nemotron-3-ultra-550b-a55b:free".into(),
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: Vec::new(),
            max_tokens: 128,
            temperature: None,
        };

        let chained =
            OpenAiProvider::openrouter("k".into(), "http://x/v1").with_fallback_models(vec![
                // Includes the primary on purpose: it must be deduplicated,
                // not sent twice.
                "nvidia/nemotron-3-ultra-550b-a55b:free".into(),
                "poolside/laguna-s-2.1:free".into(),
            ]);
        let body = chained.request_body(&request);
        assert_eq!(body["route"], "fallback");
        let models: Vec<&str> = body["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            models,
            [
                "nvidia/nemotron-3-ultra-550b-a55b:free",
                "poolside/laguna-s-2.1:free"
            ]
        );

        // No chain -> no OpenRouter-specific keys at all.
        let plain = OpenAiProvider::openrouter("k".into(), "http://x/v1");
        let body = plain.request_body(&request);
        assert!(body.get("models").is_none());
        assert!(body.get("route").is_none());

        // Another flavor with a chain configured (nonsensical, but cheap to
        // defend): the body stays plain OpenAI.
        let ollama = {
            let mut p = OpenAiProvider::ollama("http://x/v1");
            p.fallback_models = vec!["whatever".into()];
            p
        };
        let body = ollama.request_body(&request);
        assert!(body.get("models").is_none());
    }

    /// Exercises the real `/api/show` against a running Ollama. Ignored, like
    /// the other live tests: it needs a server and a pulled model.
    ///
    /// `PROBE_MODEL=nemotron-3-super:cloud cargo test -p smith-provider \
    ///   live_ollama -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_ollama_reports_a_real_window() {
        let model =
            std::env::var("PROBE_MODEL").unwrap_or_else(|_| "nemotron-3-super:cloud".to_string());
        let provider = OpenAiProvider::ollama("http://127.0.0.1:11434/v1");
        let before = provider.capabilities(&model).context_window;
        provider.warm_capabilities(&model).await;
        let after = provider.capabilities(&model);
        println!(
            "{model}: guessed {before}, probed {} (max_output {})",
            after.context_window, after.max_output
        );
        assert!(
            after.context_window > OLLAMA_CONTEXT_WINDOW,
            "the probe did not improve on the guess"
        );
    }

    /// `num_ctx` is what the server actually allocated; the architecture's
    /// context length is only what the weights could support.
    #[test]
    fn num_ctx_is_read_out_of_ollamas_flat_parameter_blob() {
        assert_eq!(
            parse_num_ctx("num_ctx                        8192"),
            Some(8192)
        );
        assert_eq!(
            parse_num_ctx("stop                           \"<|eot|>\"\nnum_ctx  16384\n"),
            Some(16384)
        );
        assert_eq!(parse_num_ctx("temperature 0.7"), None);
        assert_eq!(parse_num_ctx(""), None);
        // A malformed value is not a window; better the conservative default
        // than a nonsense one.
        assert_eq!(parse_num_ctx("num_ctx lots"), None);
    }

    /// The bug this fixes, stated as arithmetic: a cloud model with a 262144
    /// window was being driven as if it had 4096, so compaction fired every
    /// few rounds and the model forgot what it had just searched for.
    #[test]
    fn a_known_window_replaces_the_conservative_guess() {
        let guessed = ollama_capabilities(OLLAMA_CONTEXT_WINDOW);
        assert_eq!(guessed.context_window, 4096);
        assert_eq!(guessed.max_output, 2048);

        let real = ollama_capabilities(262_144);
        assert_eq!(real.context_window, 262_144);
        // Halving stops being sensible at this size: no model emits a 131k
        // reply, and some servers refuse the request outright.
        assert_eq!(real.max_output, OLLAMA_MAX_OUTPUT_CEILING);
    }

    /// `/api/show` sits beside the OpenAI-compatible endpoint, not under it.
    #[test]
    fn the_native_api_root_is_the_parent_of_the_openai_one() {
        let provider = OpenAiProvider::ollama("http://127.0.0.1:11434/v1");
        assert_eq!(provider.ollama_api_root(), "http://127.0.0.1:11434");
        let trailing = OpenAiProvider::ollama("http://127.0.0.1:11434/v1/");
        assert_eq!(trailing.ollama_api_root(), "http://127.0.0.1:11434");
    }

    /// Until the probe answers — and forever if it never does — the guess
    /// stands. A provider that cannot be asked must still start a session.
    #[test]
    fn an_unprobed_model_keeps_the_conservative_default() {
        let provider = OpenAiProvider::ollama("http://127.0.0.1:11434/v1");
        assert_eq!(
            provider.capabilities("anything:cloud").context_window,
            OLLAMA_CONTEXT_WINDOW
        );
    }

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

    /// OpenAI reports cached tokens as a breakdown *of* `prompt_tokens`, the
    /// opposite of Anthropic. `Usage`'s fields are disjoint, so the cached
    /// share has to come back out of `input_tokens` or the same tokens get
    /// counted — and billed — twice.
    #[test]
    fn cached_prompt_tokens_are_split_out_of_the_input_count() {
        let mut state = SseState::default();
        parse_chunk(
            &serde_json::json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 10_000,
                    "completion_tokens": 250,
                    "prompt_tokens_details": {"cached_tokens": 8_000},
                },
            }),
            &mut state,
        );

        assert_eq!(state.usage.input_tokens, 2_000);
        assert_eq!(state.usage.cache_read, 8_000);
        // OpenAI does not bill or report cache creation.
        assert_eq!(state.usage.cache_write, 0);
        // The total is unchanged — only its attribution moved.
        assert_eq!(state.usage.prompt_tokens(), 10_000);
    }

    #[test]
    fn usage_without_a_cache_breakdown_is_all_input() {
        let mut state = SseState::default();
        parse_chunk(
            &serde_json::json!({
                "choices": [],
                "usage": {"prompt_tokens": 500, "completion_tokens": 20},
            }),
            &mut state,
        );
        assert_eq!(state.usage.input_tokens, 500);
        assert_eq!(state.usage.cache_read, 0);
        assert_eq!(state.usage.prompt_tokens(), 500);
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

    #[test]
    fn known_openai_models_report_their_real_window() {
        let gpt41 = openai_capabilities("gpt-4.1");
        assert_eq!(gpt41.context_window, 1_047_576);
        assert_eq!(gpt41.max_output, 32_768);
        assert!(!gpt41.supports_thinking);

        let gpt4o = openai_capabilities("gpt-4o");
        assert_eq!(gpt4o.context_window, 128_000);
        assert_eq!(gpt4o.max_output, 16_384);

        let o3 = openai_capabilities("o3");
        assert_eq!(o3.context_window, 200_000);
        assert!(o3.supports_thinking, "the o-series reasons");
    }

    #[test]
    fn longest_matching_prefix_wins() {
        // "gpt-4.1-mini" must not be shadowed by the "gpt-4.1" entry into a
        // different row; both happen to share limits, so assert the row itself.
        assert_eq!(
            openai_capabilities("gpt-4.1-mini"),
            openai_capabilities("gpt-4.1")
        );
    }

    #[test]
    fn unknown_openai_model_falls_back_to_the_conservative_default() {
        let caps = openai_capabilities("gpt-6-ultra");
        assert_eq!(caps, ProviderCapabilities::default());
        assert!(
            caps.context_window <= 8192,
            "unknown models must not be assumed roomy: {}",
            caps.context_window
        );
    }

    /// Unprobed, every model gets the conservative local default — the window
    /// is now a value rather than a constant baked into this function, so what
    /// this pins is the *fallback*, not the answer.
    #[test]
    fn ollama_reports_the_conservative_local_default_when_nothing_is_known() {
        let provider = OpenAiProvider::ollama("http://127.0.0.1:11434/v1");
        for model in ["llama3.3", "qwen2.5", "something-nobody-has-heard-of"] {
            let caps = provider.capabilities(model);
            assert_eq!(caps.context_window, 4096);
            assert!(caps.max_output <= caps.context_window);
            assert!(!caps.supports_cache && !caps.supports_token_count);
        }
    }

    #[test]
    fn flavor_selects_the_catalogue() {
        let openai = OpenAiProvider::new("k".into());
        assert_eq!(openai.capabilities("gpt-4o").context_window, 128_000);

        // The same model name against an Ollama server is a *local* model, so
        // it must not inherit the hosted model's window.
        let ollama = OpenAiProvider::ollama("http://localhost:11434/v1");
        assert_eq!(ollama.capabilities("gpt-4o").context_window, 4096);
    }
}
