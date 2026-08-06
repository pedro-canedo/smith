//! Chains whole providers, for the day the active one's *account* runs dry.
//!
//! # The two layers, and which one this is
//!
//! OpenRouter already falls back **between its own models** server-side
//! (`models` + `route: "fallback"` in the request body) — that layer costs
//! nothing and lives in `openai.rs`. What it cannot help with is the account
//! itself running out: the free tier is 50 requests/day across *all* free
//! models, and when that 429 arrives, every entry in the server-side chain is
//! equally exhausted. This wrapper is the second layer: it moves the session
//! to a different provider entirely — the local 9Router gateway, an Ollama —
//! and it does so mid-turn, without losing a word of the conversation.
//!
//! # How an advancement actually happens
//!
//! Nothing here retries. `Agent::stream_with_retry` already owns the retry
//! loop, re-sending the original request while `RetryPolicy` grants delays —
//! so this wrapper only has to (a) route each attempt to the entry that is
//! currently active, and (b) make sure a quota death is *retryable*. On a
//! quota-class error it advances the index and returns
//! `ProviderError::Http("… falling back to …")` — retryable by construction —
//! and the very next attempt of the existing loop lands on the next entry.
//! The message rides `AgentEvent::ProviderRetry.reason`, which both frontends
//! already render, so the user sees the handover as it happens.
//!
//! The index is **sticky** for the session: a provider that just said "come
//! back tomorrow" is not probed again on the next turn, because that would
//! burn retry budget every single turn for the rest of the day. `/model`
//! rebuilds the whole stack, which is the deliberate reset lever.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use smith_core::provider::ProviderCapabilities;
use smith_core::{CompletionRequest, LlmProvider, ProviderError, StreamEvent};
use tokio_util::sync::CancellationToken;

/// One rung of the chain.
pub struct FallbackEntry {
    pub provider: Arc<dyn LlmProvider>,
    /// The model driven on this entry — each provider has its own.
    pub model: String,
    /// For the handover message: "falling back to 9router / auto".
    pub label: String,
}

/// Consecutive retryable failures on one entry before it is abandoned even
/// without a quota marker.
///
/// The backstop for the two quota shapes that carry no marker at all: a daily
/// 429 whose body says nothing useful, and a local gateway that is simply
/// dead (connection refused is an `Http` error, not an `Api` one). Two, not
/// one: a single transient failure is what the retry layer exists to absorb,
/// and advancing on it would abandon a healthy provider over a hiccup.
const STRIKE_LIMIT: usize = 2;

pub struct FallbackProvider {
    entries: Vec<FallbackEntry>,
    /// Index of the entry currently serving. Only ever moves forward.
    active: AtomicUsize,
    /// Consecutive retryable failures on the active entry.
    strikes: AtomicUsize,
    /// Mirror of `RetryPolicy::max_retry_after`: a 429 telling us to wait
    /// longer than the retry layer will ever sleep is, for this session's
    /// purposes, an exhausted account.
    max_retry_after: Duration,
}

impl FallbackProvider {
    /// `entries` must be non-empty; the first is the primary.
    pub fn new(entries: Vec<FallbackEntry>, max_retry_after: Duration) -> Self {
        assert!(
            !entries.is_empty(),
            "a fallback chain with no entries is not a provider"
        );
        Self {
            entries,
            active: AtomicUsize::new(0),
            strikes: AtomicUsize::new(0),
            max_retry_after,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn active_entry(&self) -> &FallbackEntry {
        // The index only moves forward and is bounds-checked on every
        // advancement, so this cannot slide past the end.
        &self.entries[self
            .active
            .load(Ordering::SeqCst)
            .min(self.entries.len() - 1)]
    }
}

/// Whether this error means "this account is done for now" as opposed to
/// "this request had bad luck".
///
/// - **402**: out of credits. Nothing about waiting fixes it.
/// - **429 with `Retry-After` beyond the retry cap**: the server itself says
///   the wait is longer than the retry layer will ever sleep, so for this
///   session it is an exhausted account.
/// - **429 naming the daily free quota**: OpenRouter's own marker. The docs
///   only commit to a generic "Rate limit exceeded", so this matches the
///   day-quota wording observed in the wild; a miss degrades to the strike
///   backstop (one extra retry cycle), never to "no fallback".
///
/// A plain per-minute 429 is deliberately **not** quota exhaustion — waiting
/// a few seconds fixes it, and that is exactly the retry layer's job.
fn is_quota_exhausted(error: &ProviderError, max_retry_after: Duration) -> bool {
    match error {
        ProviderError::Api { status: 402, .. } => true,
        ProviderError::Api {
            status: 429,
            message,
            retry_after,
        } => {
            if retry_after.is_some_and(|after| after > max_retry_after) {
                return true;
            }
            let lowered = message.to_ascii_lowercase();
            lowered.contains("free-models-per-day")
                || (lowered.contains("free") && lowered.contains("per day"))
                || lowered.contains("daily limit")
        }
        _ => false,
    }
}

#[async_trait]
impl LlmProvider for FallbackProvider {
    fn id(&self) -> &'static str {
        self.active_entry().provider.id()
    }

    /// The accounting hook: turns are priced and persisted under the model
    /// actually serving, not the one the session was configured with.
    fn effective_model(&self, _requested: &str) -> String {
        self.active_entry().model.clone()
    }

    /// The context-management guarantee across fallbacks: the agent asks for
    /// capabilities fresh every round, so the gauge and the compaction
    /// threshold follow the active entry's model the moment the index moves.
    fn capabilities(&self, _model: &str) -> ProviderCapabilities {
        let entry = self.active_entry();
        entry.provider.capabilities(&entry.model)
    }

    /// Warms **every** entry, not just the active one — an advancement lands
    /// mid-turn, which is exactly when nobody can afford a cold probe, and a
    /// wrong window on the new entry would mis-aim compaction.
    async fn warm_capabilities(&self, _model: &str) {
        futures::future::join_all(
            self.entries
                .iter()
                .map(|entry| entry.provider.warm_capabilities(&entry.model)),
        )
        .await;
    }

    async fn stream_completion(
        &self,
        mut request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let idx = self
            .active
            .load(Ordering::SeqCst)
            .min(self.entries.len() - 1);
        let entry = &self.entries[idx];
        request.model = entry.model.clone();

        match entry.provider.stream_completion(request, cancel).await {
            Ok(stream) => {
                self.strikes.store(0, Ordering::SeqCst);
                Ok(stream)
            }
            Err(error) => {
                // Cancellation is the user, not the provider — it must not
                // count against anyone.
                if matches!(error, ProviderError::Cancelled) {
                    return Err(error);
                }

                let strikes = if error.retryable() {
                    self.strikes.fetch_add(1, Ordering::SeqCst) + 1
                } else {
                    self.strikes.load(Ordering::SeqCst)
                };
                let exhausted = is_quota_exhausted(&error, self.max_retry_after);

                let has_next = idx + 1 < self.entries.len();
                if (exhausted || strikes >= STRIKE_LIMIT) && has_next {
                    // CAS, not a store: two concurrent requests failing on the
                    // same entry must advance the chain one step, not two.
                    if self
                        .active
                        .compare_exchange(idx, idx + 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        self.strikes.store(0, Ordering::SeqCst);
                    }
                    let next = &self.entries[self.active.load(Ordering::SeqCst)];
                    // `Http` because it is retryable by construction: the
                    // *existing* retry loop is what drives the next attempt
                    // into the next entry, and this message is what the user
                    // sees on the ProviderRetry line while it does.
                    return Err(ProviderError::Http(format!(
                        "{} unavailable ({error}); falling back to {} / {}",
                        entry.label, next.label, next.model
                    )));
                }

                // Chain exhausted, or an error that is not this wrapper's
                // business: the honest failure, unchanged.
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use smith_core::testkit::text_reply;
    use smith_core::{Agent, AgentEvent, NoTools, ToolContext};
    use tokio::sync::mpsc;

    /// The whole feature, end to end: the primary dies of quota mid-turn, the
    /// existing retry loop drives the next attempt into the fallback, the
    /// turn completes, the user saw the handover, and the books name the
    /// entry that actually served.
    #[tokio::test]
    async fn a_turn_survives_a_quota_death_and_is_accounted_to_the_survivor() {
        let chain = FallbackProvider::new(
            vec![
                FallbackEntry {
                    provider: Arc::new(
                        ScriptedProvider::errors([ProviderError::Api {
                            status: 402,
                            message: "insufficient credits".into(),
                            retry_after: None,
                        }])
                        .with_id("openrouter"),
                    ),
                    model: "nvidia/nemotron-3-ultra-550b-a55b:free".into(),
                    label: "openrouter".into(),
                },
                FallbackEntry {
                    provider: Arc::new(
                        ScriptedProvider::streams([text_reply("done, from the fallback")])
                            .with_id("ollama"),
                    ),
                    model: "llama3.2".into(),
                    label: "ollama".into(),
                },
            ],
            CAP,
        );

        let mut agent = Agent::new(
            Arc::new(chain),
            Arc::new(NoTools),
            // The session's configured model — the wrapper overrides it per
            // request, and the books must NOT show this name afterwards.
            "nvidia/nemotron-3-ultra-550b-a55b:free".to_string(),
            ToolContext::new(".", "test-session"),
        );

        let (events_tx, mut events_rx) = mpsc::unbounded_channel();
        let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
        let (question_tx, _question_rx) = mpsc::unbounded_channel();

        let completed = agent
            .run_turn(
                "hello".to_string(),
                events_tx,
                permission_tx,
                question_tx,
                CancellationToken::new(),
            )
            .await;
        assert!(completed, "the turn must complete on the fallback");

        let mut saw_handover = false;
        let mut saw_text = false;
        while let Ok(event) = events_rx.try_recv() {
            match event {
                AgentEvent::ProviderRetry { reason, .. } => {
                    saw_handover |= reason.contains("falling back to ollama / llama3.2");
                }
                AgentEvent::AssistantTextDelta(t) => {
                    saw_text |= t.contains("from the fallback");
                }
                _ => {}
            }
        }
        assert!(saw_handover, "the user never saw the handover");
        assert!(saw_text, "the fallback's reply never arrived");

        // The books name who actually served — provider AND model.
        let turn = agent.last_turn().expect("a turn was accounted");
        assert_eq!(turn.provider, "ollama");
        assert_eq!(turn.model, "llama3.2");
    }

    use super::*;
    use smith_core::testkit::ScriptedProvider;
    use smith_core::Message;

    const CAP: Duration = Duration::from_secs(30);

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: "primary-model".into(),
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: Vec::new(),
            max_tokens: 64,
            temperature: None,
        }
    }

    fn entry(provider: ScriptedProvider, model: &str, label: &str) -> FallbackEntry {
        FallbackEntry {
            provider: Arc::new(provider),
            model: model.into(),
            label: label.into(),
        }
    }

    fn quota_429(message: &str, retry_after: Option<Duration>) -> ProviderError {
        ProviderError::Api {
            status: 429,
            message: message.into(),
            retry_after,
        }
    }

    #[test]
    fn quota_classification_matches_the_shapes_that_mean_done_for_today() {
        // Out of credits: always.
        assert!(is_quota_exhausted(
            &ProviderError::Api {
                status: 402,
                message: "insufficient credits".into(),
                retry_after: None
            },
            CAP
        ));
        // The server itself says the wait outlives the retry layer.
        assert!(is_quota_exhausted(
            &quota_429("rate limited", Some(Duration::from_secs(86_400))),
            CAP
        ));
        // The daily free-quota wording.
        assert!(is_quota_exhausted(
            &quota_429("Rate limit exceeded: free-models-per-day", None),
            CAP
        ));
        // A plain per-minute 429 is the retry layer's job, not ours.
        assert!(!is_quota_exhausted(
            &quota_429("Rate limit exceeded", Some(Duration::from_secs(5))),
            CAP
        ));
        // 5xx and transport errors are bad luck, not exhaustion.
        assert!(!is_quota_exhausted(
            &ProviderError::Http("connection refused".into()),
            CAP
        ));
    }

    #[tokio::test]
    async fn a_402_advances_and_reports_a_retryable_handover() {
        let chain = FallbackProvider::new(
            vec![
                entry(
                    ScriptedProvider::errors([ProviderError::Api {
                        status: 402,
                        message: "insufficient credits".into(),
                        retry_after: None,
                    }])
                    .with_id("openrouter"),
                    "big:free",
                    "openrouter",
                ),
                entry(
                    ScriptedProvider::streams([vec![]]).with_id("9router"),
                    "auto",
                    "9router",
                ),
            ],
            CAP,
        );

        let error = chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .map(|_stream| ())
            .expect_err("the first call fails while advancing");
        assert!(error.retryable(), "the handover must be retryable: {error}");
        assert!(error.to_string().contains("falling back to 9router / auto"));

        // The retry loop's next attempt lands on the second entry.
        assert!(chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .is_ok());
        assert_eq!(chain.id(), "9router");
        assert_eq!(chain.effective_model("whatever"), "auto");
    }

    #[tokio::test]
    async fn a_plain_429_does_not_advance() {
        let chain = FallbackProvider::new(
            vec![
                entry(
                    ScriptedProvider::errors([quota_429(
                        "Rate limit exceeded",
                        Some(Duration::from_secs(3)),
                    )])
                    .with_id("openrouter"),
                    "big:free",
                    "openrouter",
                ),
                entry(ScriptedProvider::streams([vec![]]), "auto", "9router"),
            ],
            CAP,
        );

        let error = chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .map(|_stream| ())
            .expect_err("still failing");
        // Propagated raw: the retry layer sees the real 429 and its short
        // Retry-After, sleeps, and tries the SAME entry again.
        assert!(matches!(error, ProviderError::Api { status: 429, .. }));
        assert_eq!(chain.id(), "openrouter", "one plain 429 must not advance");
    }

    #[tokio::test]
    async fn a_retry_after_beyond_the_cap_advances_instead_of_stranding_the_turn() {
        let chain = FallbackProvider::new(
            vec![
                entry(
                    ScriptedProvider::errors([quota_429(
                        "rate limited",
                        Some(Duration::from_secs(3600)),
                    )])
                    .with_id("openrouter"),
                    "big:free",
                    "openrouter",
                ),
                entry(ScriptedProvider::streams([vec![]]), "auto", "9router"),
            ],
            CAP,
        );

        // Without the wrapper this error would abort the turn outright:
        // `RetryPolicy::delay_for` refuses a Retry-After beyond its cap.
        let error = chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .map(|_stream| ())
            .expect_err("advancing");
        assert!(error.retryable());
        assert!(chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn two_strikes_advance_even_without_a_quota_marker() {
        let chain = FallbackProvider::new(
            vec![
                entry(
                    // A dead local gateway: transport errors, no Api status.
                    ScriptedProvider::errors([
                        ProviderError::Http("connection refused".into()),
                        ProviderError::Http("connection refused".into()),
                    ])
                    .with_id("9router"),
                    "auto",
                    "9router",
                ),
                entry(ScriptedProvider::streams([vec![]]), "llama3.2", "ollama"),
            ],
            CAP,
        );

        // First failure: strike one, no advancement — a hiccup is the retry
        // layer's to absorb.
        let first = chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .map(|_stream| ())
            .expect_err("strike one");
        assert!(!first.to_string().contains("falling back"), "{first}");
        // Second consecutive failure: abandoned.
        let second = chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .map(|_stream| ())
            .expect_err("strike two advances");
        assert!(
            second.to_string().contains("falling back to ollama"),
            "{second}"
        );
        assert!(chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_success_resets_the_strike_count() {
        let chain = FallbackProvider::new(
            vec![
                entry(
                    ScriptedProvider::streams_then_errors(
                        [
                            vec![], // attempt 1: ok
                        ],
                        [
                            ProviderError::Http("blip".into()), // strike 1
                        ],
                    )
                    .with_id("a"),
                    "m",
                    "a",
                ),
                entry(ScriptedProvider::streams([vec![]]), "n", "b"),
            ],
            CAP,
        );

        assert!(chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .is_ok());
        // One failure after a success is strike one again, not strike two.
        let error = chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .map(|_stream| ())
            .expect_err("a single strike");
        assert!(!error.to_string().contains("falling back"), "{error}");
        assert_eq!(chain.id(), "a");
    }

    #[tokio::test]
    async fn the_last_entry_fails_with_the_raw_error() {
        let chain = FallbackProvider::new(
            vec![entry(
                ScriptedProvider::errors([ProviderError::Api {
                    status: 402,
                    message: "insufficient credits".into(),
                    retry_after: None,
                }])
                .with_id("openrouter"),
                "big:free",
                "openrouter",
            )],
            CAP,
        );

        let error = chain
            .stream_completion(request(), CancellationToken::new())
            .await
            .map(|_stream| ())
            .expect_err("nowhere to go");
        // The real 402, not a synthetic handover: the user gets the truth.
        assert!(matches!(error, ProviderError::Api { status: 402, .. }));
    }

    #[tokio::test]
    async fn the_request_is_rewritten_to_the_active_entrys_model() {
        let first = ScriptedProvider::errors([ProviderError::Api {
            status: 402,
            message: "no credits".into(),
            retry_after: None,
        }])
        .with_id("openrouter");
        let second = Arc::new(ScriptedProvider::streams([vec![]]).with_id("ollama"));
        let second_handle = second.clone();

        let chain = FallbackProvider::new(
            vec![
                entry(first, "big:free", "openrouter"),
                FallbackEntry {
                    provider: second,
                    model: "llama3.2".into(),
                    label: "ollama".into(),
                },
            ],
            CAP,
        );

        let _ = chain
            .stream_completion(request(), CancellationToken::new())
            .await;
        let _ = chain
            .stream_completion(request(), CancellationToken::new())
            .await;
        let sent = second_handle.last_request().expect("second entry served");
        assert_eq!(
            sent.model, "llama3.2",
            "the request must carry the serving entry's model, not the session's"
        );
    }

    #[tokio::test]
    async fn cancellation_neither_strikes_nor_advances() {
        let chain = FallbackProvider::new(
            vec![
                entry(
                    ScriptedProvider::errors([
                        ProviderError::Cancelled,
                        ProviderError::Cancelled,
                        ProviderError::Cancelled,
                    ])
                    .with_id("a"),
                    "m",
                    "a",
                ),
                entry(ScriptedProvider::streams([vec![]]), "n", "b"),
            ],
            CAP,
        );

        for _ in 0..3 {
            let error = chain
                .stream_completion(request(), CancellationToken::new())
                .await
                .map(|_stream| ())
                .expect_err("cancelled");
            assert!(matches!(error, ProviderError::Cancelled));
        }
        assert_eq!(chain.id(), "a", "Esc must never look like an outage");
    }
}
