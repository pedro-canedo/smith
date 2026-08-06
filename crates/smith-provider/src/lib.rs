//! LLM provider adapters (Anthropic, OpenAI, ...) implementing smith_core::LlmProvider.

use std::time::Duration;

use smith_core::ProviderError;

pub mod anthropic;
pub mod fallback;
pub mod ollama;
pub mod openai;

pub use anthropic::AnthropicProvider;
pub use fallback::{FallbackEntry, FallbackProvider};
pub use ollama::{classify_ollama_error, ollama_tags, OllamaModel};
pub use openai::OpenAiProvider;

/// TCP connect + TLS handshake. A healthy API endpoint completes this in well
/// under a second anywhere in the world; ten seconds means the host is down,
/// DNS is lying, or a captive portal is swallowing the SYN. Failing here is
/// cheap and unambiguously retryable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Time between successive bytes *on the wire*, not the total request.
///
/// These are streaming clients, so `Client::timeout()` — a deadline on the
/// whole request including the response body — is the wrong tool: it would kill
/// a legitimate long generation at an arbitrary point, and the longer the
/// answer the likelier it dies. `read_timeout` resets on every chunk, so it
/// only fires when the connection has actually gone quiet. Two minutes is
/// generous enough for a slow first token behind a long prompt (and both
/// providers emit keepalive/ping SSE events well inside it) while still
/// releasing a silently half-dead socket instead of hanging the turn forever.
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Evict keep-alive sockets that have been idle this long. NATs and proxies
/// drop idle connections without telling either end; without this we hand a
/// corpse to the next turn and pay a full timeout to discover it.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The shared client both adapters build on, so the timeout policy has exactly
/// one definition.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .build()
        .unwrap_or_else(|e| {
            // Only fails if the TLS backend won't initialise. Panicking here
            // would take down the TUI at startup over a client we can still
            // build untimed and surface errors through normally.
            tracing::warn!(error = %e, "falling back to an HTTP client with no timeouts");
            reqwest::Client::new()
        })
}

/// Reads `Retry-After` off an error response.
///
/// Only the delta-seconds form is understood. The HTTP-date form would need a
/// date parser and a trustworthy local clock, and neither Anthropic nor OpenAI
/// sends it — an unparsed value simply means the retry layer falls back to its
/// own backoff, the same behaviour as no header at all.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Turns a non-2xx response into `ProviderError::Api`, capturing the status and
/// `Retry-After` before the body is consumed.
pub(crate) async fn api_error(resp: reqwest::Response) -> ProviderError {
    let status = resp.status().as_u16();
    let retry_after = parse_retry_after(resp.headers());
    let message = resp.text().await.unwrap_or_default();
    ProviderError::Api {
        status,
        message,
        retry_after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn parses_delta_seconds_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));
    }

    #[test]
    fn absent_retry_after_yields_none() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn unparseable_retry_after_yields_none() {
        // The HTTP-date form is valid HTTP but not something we decode; it must
        // degrade to "no hint", never to a bogus duration.
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn builds_a_client_with_the_shared_timeout_policy() {
        // reqwest exposes no getters for the configured timeouts, so this only
        // pins that the builder configuration is valid and never panics.
        let _ = http_client();
    }
}
