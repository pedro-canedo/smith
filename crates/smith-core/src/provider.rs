use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::message::{CompletionRequest, StreamEvent};

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http request failed: {0}")]
    Http(String),
    /// A response arrived, but the provider rejected the request. `status` is
    /// kept as a number rather than formatted into `message` so callers can
    /// tell a rate limit (429) from a contract error (400) — see `retryable`.
    #[error("provider returned an error (HTTP {status}): {message}{}", retry_hint(.retry_after))]
    Api {
        status: u16,
        message: String,
        /// The response's `Retry-After`, when it sent one. This lives on `Api`
        /// and nowhere else because `Retry-After` is a *response* header: it
        /// only exists when a response actually arrived with a status. Hanging
        /// it off the enum as a whole (or off `Http`, which by definition never
        /// got a response) would make unrepresentable states representable.
        retry_after: Option<Duration>,
    },
    #[error("failed to parse provider response: {0}")]
    Parse(String),
    #[error("missing API key for provider {0}")]
    MissingApiKey(String),
    #[error("request cancelled")]
    Cancelled,
}

/// Rendered into `Api`'s `Display` so a rate-limited user sees when to come
/// back; `agent.rs` surfaces `e.to_string()` straight to the chat pane.
fn retry_hint(retry_after: &Option<Duration>) -> String {
    match retry_after {
        Some(d) => format!(" (retry after {}s)", d.as_secs()),
        None => String::new(),
    }
}

impl ProviderError {
    /// Whether re-sending the identical request could plausibly succeed.
    ///
    /// Deliberately opinionated, because the retry-with-backoff layer above
    /// this trusts the answer:
    ///
    /// - **`Http` (transport)**: retryable. A DNS blip, a refused connect, a
    ///   TLS handshake failure, or a socket dropped mid-stream says nothing
    ///   about the request itself — the same bytes may well go through on the
    ///   next attempt.
    /// - **429 / 5xx**: retryable. 429 is the provider explicitly saying "later";
    ///   5xx is a fault on their side, not in our payload.
    /// - **Any other 4xx**: not retryable. A 400 (malformed body), 401/403
    ///   (bad or revoked key), or 404 (unknown model) is a *contract* error:
    ///   nothing about the request changes between attempts, so replaying it
    ///   burns quota and rate-limit budget and can never succeed. Retrying here
    ///   turns one clear failure into N slow ones.
    /// - **`Parse`**: not retryable. We received bytes and could not decode
    ///   them; asking again yields the same undecodable shape. This is a bug in
    ///   the adapter or a provider protocol change — it needs a code fix, not a
    ///   second attempt.
    /// - **`MissingApiKey`**: not retryable. Config, not weather.
    /// - **`Cancelled`**: emphatically not retryable. The user pressed stop;
    ///   silently re-issuing the request would be the single most hostile thing
    ///   this layer could do.
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::Http(_) => true,
            ProviderError::Api { status, .. } => *status == 429 || (500..600).contains(status),
            ProviderError::Parse(_)
            | ProviderError::MissingApiKey(_)
            | ProviderError::Cancelled => false,
        }
    }

    /// The server-supplied delay, if any. A provider telling us when to come
    /// back beats any backoff formula we could invent, so callers should prefer
    /// this over their own schedule when it is present.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ProviderError::Api { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// What a given provider/model pair can do. Gates the context-window gauge,
/// auto-compaction, and prompt caching — all of which need per-model numbers,
/// not per-provider ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Total tokens the model accepts (prompt + completion).
    pub context_window: u32,
    /// Largest completion the model will emit in one response.
    pub max_output: u32,
    pub supports_tools: bool,
    /// Explicit, caller-controlled prompt caching (Anthropic `cache_control`),
    /// not opaque server-side caching the caller cannot address.
    pub supports_cache: bool,
    pub supports_thinking: bool,
    /// A dedicated token-counting endpoint. Without one, any context gauge is
    /// an estimate.
    pub supports_token_count: bool,
}

impl Default for ProviderCapabilities {
    /// The conservative fallback for a model we have no entry for.
    ///
    /// It errs *small* on purpose. Auto-compaction fires at a fraction of
    /// `context_window`, so the two failure directions are not symmetric:
    /// under-estimating compacts a conversation earlier than strictly
    /// necessary (mildly wasteful, fully recoverable), while over-estimating
    /// lets history grow past what the model accepts and the turn hard-fails
    /// mid-flight with the context already lost. 8192 is at or below the
    /// window of essentially every chat model still in service, so an unknown
    /// model degrades to "compacts a bit eagerly" rather than "blows up".
    ///
    /// Feature flags default off for the same reason: claiming a capability the
    /// model lacks produces requests it rejects outright.
    fn default() -> Self {
        Self {
            context_window: 8192,
            max_output: 4096,
            // Tools are the one exception: every provider smith targets speaks
            // some tool-call dialect, and defaulting this off would make an
            // unknown model useless rather than merely cautious.
            supports_tools: true,
            supports_cache: false,
            supports_thinking: false,
            supports_token_count: false,
        }
    }
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Stable id, e.g. "anthropic", "openai".
    fn id(&self) -> &'static str;

    /// Capabilities for one model. Takes the model name because
    /// `context_window` is a property of the model, not the provider — an
    /// Anthropic key can address both a 200K and a 1M window.
    ///
    /// Defaulted so test doubles and future adapters compile without a table;
    /// the default is the conservative one, which is the safe way to be wrong.
    fn capabilities(&self, _model: &str) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }

    /// Stream a completion. The returned stream yields normalized `StreamEvent`s;
    /// the caller accumulates them into a `Message`.
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16) -> ProviderError {
        ProviderError::Api {
            status,
            message: "boom".into(),
            retry_after: None,
        }
    }

    #[test]
    fn rate_limits_and_server_faults_are_retryable() {
        assert!(api(429).retryable());
        assert!(api(500).retryable());
        assert!(api(503).retryable());
    }

    #[test]
    fn contract_errors_are_not_retryable() {
        assert!(!api(400).retryable());
        assert!(!api(401).retryable());
        assert!(!api(404).retryable());
        assert!(!api(422).retryable());
    }

    #[test]
    fn transport_errors_are_retryable_but_parse_and_cancel_are_not() {
        assert!(ProviderError::Http("connection reset".into()).retryable());
        assert!(!ProviderError::Parse("expected `,`".into()).retryable());
        assert!(!ProviderError::MissingApiKey("anthropic".into()).retryable());
        assert!(!ProviderError::Cancelled.retryable());
    }

    #[test]
    fn retry_after_is_exposed_when_present_and_absent_otherwise() {
        let with = ProviderError::Api {
            status: 429,
            message: "slow down".into(),
            retry_after: Some(Duration::from_secs(30)),
        };
        assert_eq!(with.retry_after(), Some(Duration::from_secs(30)));
        assert_eq!(api(429).retry_after(), None);
        // Non-Api variants can never carry one: no response, no header.
        assert_eq!(ProviderError::Http("dns".into()).retry_after(), None);
    }

    #[test]
    fn display_stays_readable_for_every_variant() {
        assert_eq!(
            ProviderError::Http("connection reset".into()).to_string(),
            "http request failed: connection reset"
        );
        assert_eq!(
            api(400).to_string(),
            "provider returned an error (HTTP 400): boom"
        );
        assert_eq!(
            ProviderError::Api {
                status: 429,
                message: "rate limited".into(),
                retry_after: Some(Duration::from_secs(30)),
            }
            .to_string(),
            "provider returned an error (HTTP 429): rate limited (retry after 30s)"
        );
        assert_eq!(
            ProviderError::Parse("bad json".into()).to_string(),
            "failed to parse provider response: bad json"
        );
        assert_eq!(
            ProviderError::MissingApiKey("openai".into()).to_string(),
            "missing API key for provider openai"
        );
        assert_eq!(ProviderError::Cancelled.to_string(), "request cancelled");
    }

    #[test]
    fn default_capabilities_are_conservative() {
        let caps = ProviderCapabilities::default();
        assert!(
            caps.context_window <= 8192,
            "an unknown model must not be assumed roomy: {}",
            caps.context_window
        );
        assert!(caps.max_output <= caps.context_window);
        assert!(!caps.supports_cache);
        assert!(!caps.supports_thinking);
        assert!(!caps.supports_token_count);
    }
}
