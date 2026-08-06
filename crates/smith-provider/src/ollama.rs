//! What the local Ollama daemon says it can run.
//!
//! Ollama serves an OpenAI-compatible endpoint, so inference goes through
//! `openai.rs` like everything else. This module exists for the half that is
//! not OpenAI-shaped: `GET /api/tags`, the daemon's own catalogue, which is
//! the only place three facts live.
//!
//! - **Which models exist.** The wizard used to show a hardcoded list of nine
//!   popular names, so a machine's actual models were invisible and a name it
//!   had never pulled was one keypress away.
//! - **Whether a model is remote.** Ollama's cloud models carry a
//!   `remote_host` and are proxied by the local daemon, which holds the
//!   credential. From smith's side they are keyless; from the user's side they
//!   need `ollama signin`. Nothing else in smith can tell the two apart.
//! - **The real context window.** `openai.rs` assumes a conservative 4096
//!   until `/api/show` answers, for a reason that is sound for a local model
//!   and wrong for a cloud one — see `OLLAMA_CONTEXT_WINDOW` there.
//!
//! `capabilities` is read too, because an agent that cannot call tools is a
//! chatbot: a model without `tools` is worth naming as unsuitable rather than
//! offering and letting the first turn fail.

use serde::Deserialize;

use crate::ProviderError;

/// One entry of the daemon's catalogue, reduced to what smith acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaModel {
    pub name: String,
    /// Proxied to `ollama.com` rather than run locally.
    pub is_cloud: bool,
    /// Advertised by the daemon; `None` when it does not say.
    pub context_window: Option<u32>,
    /// Weights on disk. Cloud entries report a placeholder of a few hundred
    /// bytes, so this is only meaningful for local models.
    pub size_bytes: Option<u64>,
    /// Without this the model cannot drive the agent loop at all.
    pub supports_tools: bool,
}

impl OllamaModel {
    /// A one-line description for a picker: `nemotron-3-super:cloud  cloud · 262k ctx`.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.is_cloud {
            parts.push("cloud".to_string());
        } else if let Some(bytes) = self.size_bytes {
            parts.push(format!("{:.1} GB", bytes as f64 / 1_000_000_000.0));
        }
        if let Some(window) = self.context_window {
            parts.push(format!("{} ctx", compact_tokens(window)));
        }
        if !self.supports_tools {
            // Loudest thing on the row: picking it produces an agent that
            // cannot read a file, and the failure arrives a turn later.
            parts.push("NO TOOLS".to_string());
        }
        if parts.is_empty() {
            self.name.clone()
        } else {
            format!("{}  {}", self.name, parts.join(" · "))
        }
    }
}

/// `262144` -> `262k`, `1048576` -> `1.0M`. Mirrors the context gauge's
/// `compact`, which the user already reads in the sidebar.
fn compact_tokens(n: u32) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    remote_host: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    details: TagDetails,
}

#[derive(Deserialize, Default)]
struct TagDetails {
    #[serde(default)]
    context_length: Option<u32>,
}

/// Reads a `/api/tags` body. Pure, so the shape is pinned by a captured
/// fixture rather than by whatever the developer's machine has pulled.
///
/// Anything unparseable yields an empty list rather than an error: a
/// catalogue smith cannot read is a catalogue it should fall back from, not a
/// reason to fail a wizard that has a static list to offer.
pub fn parse_ollama_tags(body: &serde_json::Value) -> Vec<OllamaModel> {
    let Ok(parsed) = serde_json::from_value::<TagsResponse>(body.clone()) else {
        return Vec::new();
    };
    parsed
        .models
        .into_iter()
        .filter(|e| !e.name.trim().is_empty())
        .map(|e| OllamaModel {
            // Both signals, deliberately. `remote_host` is the fact and the
            // `:cloud` suffix is the convention; a daemon that stops sending
            // one must not silently reclassify every cloud model as local,
            // because that decides whether smith trusts the advertised
            // context window.
            is_cloud: e.remote_host.is_some_and(|h| !h.trim().is_empty())
                || e.name.ends_with(":cloud"),
            // A cloud entry's `size` is a placeholder of a few hundred bytes,
            // which is why `summary` only shows it for local models.
            size_bytes: e.size,
            context_window: e.details.context_length.filter(|w| *w > 0),
            supports_tools: e.capabilities.iter().any(|c| c == "tools"),
            name: e.name,
        })
        .collect()
}

/// Asks the daemon what it can run. `base_url` is the OpenAI-compatible one
/// (`…/v1`); the native API sits beside it, so the `/v1` is stripped.
pub async fn ollama_tags(base_url: &str) -> Result<Vec<OllamaModel>, ProviderError> {
    let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let body: serde_json::Value = crate::http_client()
        .get(format!("{root}/api/tags"))
        .send()
        .await
        .map_err(|e| ProviderError::Http(e.to_string()))?
        .json()
        .await
        .map_err(|e| ProviderError::Parse(e.to_string()))?;
    Ok(parse_ollama_tags(&body))
}

/// Turns an Ollama error *message* into an error worth showing.
///
/// Two failures are worth translating, and they arrive differently depending
/// on whether the request streamed — measured, not assumed:
///
/// - **`stream: true`** (what a turn uses): HTTP **403** with the JSON body.
///   That already reaches the `!status.is_success()` branch, so what was
///   missing was not detection but *language* — the user got
///   `provider returned an error (HTTP 403): {"error":{"message":…}}` and had
///   to read JSON to learn they needed to run one command.
/// - **`stream` absent** (what a probe uses): HTTP **200** with the same body.
///   Nothing downstream looks for an error in a success, so it surfaced as an
///   empty stream — see `error_in_success_body`.
///
/// The status each one is given is not cosmetic — it is how the rest of smith
/// already reasons about failures, and both answers happen to be right:
///
/// - **402** for a model the account is not entitled to. `retryable()` is
///   false, so the turn does not sit in a backoff loop over a billing fact,
///   and `FallbackProvider` treats 402 as a quota-class death, so a chain
///   moves past the model instead of stalling on it. Nothing about waiting
///   fixes it.
/// - **401** for a signed-out daemon. Also not retryable, for the same reason,
///   and it is literally what the upstream said before the daemon dressed it
///   as a 200.
///
/// The literal phrases matched here are the contract, and it is a contract
/// nobody promised us — it will change, and the tests are where that shows up.
pub fn classify_ollama_error(message: &str) -> ProviderError {
    let lower = message.to_ascii_lowercase();

    if lower.contains("subscription") || lower.contains("upgrade for access") {
        return ProviderError::Api {
            status: 402,
            message: format!(
                "this Ollama model needs a paid plan. Pick a free cloud model \
                 (`nemotron-3-super:cloud`), pick a local one, or upgrade at \
                 https://ollama.com/upgrade. The daemon said: {message}"
            ),
            retry_after: None,
        };
    }
    if lower.contains("unauthorized")
        || lower.contains("not signed in")
        || lower.contains("sign in")
    {
        return ProviderError::Api {
            status: 401,
            message: format!(
                "the Ollama daemon is not signed in, so its cloud models are \
                 refused — run `ollama signin` (free account, no card), then \
                 retry. The daemon said: {message}"
            ),
            retry_after: None,
        };
    }
    // Unrecognised: passed through with the status the transport reported it
    // under. A guess about someone else's error message is worse than the
    // message, and 200 is the honest answer to "what did the wire say".
    ProviderError::Api {
        status: 200,
        message: message.to_string(),
        retry_after: None,
    }
}

/// Pulls an error message out of a body that arrived with a 2xx status, if it
/// is carrying one. `{"error": "..."}` and `{"error": {"message": "..."}}` are
/// both in the wild — the first from the daemon, the second proxied from
/// upstream.
pub fn error_in_success_body(body: &serde_json::Value) -> Option<&str> {
    match body.get("error")? {
        serde_json::Value::String(s) => Some(s.as_str()),
        object => object.get("message")?.as_str(),
    }
}

#[cfg(test)]
mod tests;
