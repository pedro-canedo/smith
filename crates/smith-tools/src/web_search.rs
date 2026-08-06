//! `web_search` — lets the agent look things up instead of guessing (or,
//! worse, telling the user to run search commands themselves).
//!
//! ## Backends, in the order they are tried
//!
//! 1. **SearXNG** (`[search] searxng_url`) — the user's own instance. First
//!    whenever it is set, ahead of even a paid key: it is the one backend with
//!    no shared IP reputation, no anti-bot layer and no rate limit the user did
//!    not choose. See [`crate::searxng`] for what they must enable.
//! 2. **Exa** (`[exa] api_key`) — paid, structured, reports real publication
//!    dates. Skipped entirely without a key: Exa's keyless tier now answers
//!    HTTP 402, so probing it only spent a request per search to be refused.
//! 3. **Tavily** (`[tavily] api_key`) — structured, agent-oriented, and its
//!    free tier (1,000 credits/month, no card) is the cheapest key there is.
//!    Skipped without a key, like Exa.
//! 4. **Bing over RSS**, plain HTTP — the free workhorse. See [`crate::bing`].
//! 5. **Bing over RSS**, through a headless browser — the same query on a
//!    different network path, for hosts where plain HTTP is intercepted.
//! 6. **Google News RSS** — keyless, no anti-bot layer, and the only free
//!    tier with real publication dates; a news index, so it backstops the
//!    current-events queries Bing fumbles rather than replacing it. See
//!    [`crate::google_news`].
//! 7. **DuckDuckGo lite** — last, and measured as blocked far more often than
//!    not; kept only because it costs one request on a path where everything
//!    else has already failed.
//!
//! DuckDuckGo used to be tiers 2 and 3. It was demoted on evidence: its
//! `html` and `lite` endpoints answer HTTP 202 with a 14 KB challenge page to
//! a plain client *and* to a real headless browser, and its JavaScript
//! endpoint renders no results at all under `--dump-dom` at any virtual time
//! budget.
//!
//! ## Three outcomes, never two
//!
//! "Found nothing", "cannot search right now" and "search is not set up" call
//! for three different reactions, and collapsing them is what previously ended
//! with the model quietly answering from training data. [`Unavailable`] keeps
//! them apart all the way to the message the model reads.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};
use tokio_util::sync::CancellationToken;

const EXA_SEARCH_URL: &str = "https://api.exa.ai/search";
const TAVILY_SEARCH_URL: &str = "https://api.tavily.com/search";
const DUCKDUCKGO_LITE_URL: &str = "https://lite.duckduckgo.com/lite/";

/// Five results by default rather than three: with three, a model that does
/// not find its answer in the snippets reliably re-searches with a reworded
/// query — a whole extra round trip — where two more rows would have answered.
/// The per-row cost (title, URL, summary) is small next to a second search.
/// Callers wanting more or fewer say so with `num_results`.
const DEFAULT_NUM_RESULTS: u64 = 5;
const MAX_NUM_RESULTS: u64 = 10;

/// Caps each backend attempt so a stalled request falls through to the next
/// tier (or the final error) instead of hanging the whole turn.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Sent by every plain-HTTP backend here.
///
/// It used to be `Mozilla/5.0 (compatible; smith-agent/1.0)` — a header that
/// tells an anti-bot layer exactly what it is looking for. Swapping it does not
/// on its own unblock DuckDuckGo (measured: still HTTP 202 with a Chrome UA,
/// over both GET and POST), but Bing's RSS endpoint does answer it, so the
/// self-identifying string bought nothing and cost the tier that works.
pub(crate) const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/131.0.0.0 Safari/537.36";

/// Total Bing attempts across all markets for one search.
///
/// Three, not more: a temporary block does not clear inside one turn, and the
/// point of stopping early is to leave the *next* turn's request some budget
/// rather than spending it all proving the same thing.
const BING_MAX_ATTEMPTS: u32 = 3;

/// Backoff between Bing attempts, in the shape of
/// [`smith_core::RetryPolicy`] — same base, same doubling, same equal jitter.
/// Not that type itself, which keys off `ProviderError` and describes an LLM
/// request; duplicating three constants is cheaper than bending it to cover a
/// search backend too.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(4);

/// How many distinct queries one session remembers.
///
/// Small on purpose: the case being served is the same turn asking a question
/// twice in slightly different words, not a long-lived index. A real session
/// was observed issuing four searches and three fetches in a single turn, which
/// is enough to trip a rate limit unaided.
const CACHE_CAPACITY: usize = 64;

/// `pub(crate)` so every backend module can produce the same rows — the
/// formatting below is then shared, and a result reads identically to the model
/// whichever backend found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchResult {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) snippet: String,
    /// Publication date as `YYYY-MM-DD`, when the backend reports a real one.
    /// This is the only recency signal the model gets: without it, a five-year
    /// old page and this morning's are indistinguishable in the result list.
    pub(crate) published: Option<String>,
}

/// Why one backend could not answer.
///
/// The variants exist to be told apart in the final message, because the right
/// next move differs completely: wait, fix a setting, or stop asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// The tier is not set up on this machine — no key, no browser, no URL.
    /// Nothing will change until the user configures something.
    NotConfigured(String),
    /// The tier *is* set up, and is answering wrongly. A retry cannot help;
    /// the message names what to change.
    Misconfigured(String),
    /// A challenge page, a 429, a poisoned result set, a network blip. The
    /// same endpoint worked minutes earlier and will work again shortly, so
    /// this must never be reported as a permanent failure.
    Transient(String),
}

impl Unavailable {
    fn reason(&self) -> &str {
        match self {
            Self::NotConfigured(r) | Self::Misconfigured(r) | Self::Transient(r) => r,
        }
    }
}

/// Everything `web_search` needs from configuration.
///
/// A struct rather than a widening argument list so adding the next backend
/// does not churn every call site.
#[derive(Debug, Clone, Default)]
pub struct SearchSettings {
    pub exa_api_key: Option<String>,
    /// API key for Tavily (https://app.tavily.com) — free tier available.
    pub tavily_api_key: Option<String>,
    /// Base URL of a SearXNG instance, e.g. `https://searx.example.com`.
    pub searxng_url: Option<String>,
    /// Bing market tag, e.g. `en-US`. See [`crate::bing::DEFAULT_MARKET`].
    pub market: Option<String>,
}

pub struct WebSearchTool {
    settings: SearchSettings,
    client: reqwest::Client,
    /// Results already fetched this session, keyed on the normalised query.
    ///
    /// A `Mutex` and not a channel because the whole point is that a second
    /// identical query costs nothing at all — including no await.
    cache: Mutex<HashMap<String, CacheEntry>>,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    source: String,
    results: Vec<SearchResult>,
}

impl WebSearchTool {
    /// The keyless, unconfigured tool: Bing and DuckDuckGo only.
    pub fn new(exa_api_key: Option<String>) -> Self {
        Self::with_settings(SearchSettings {
            exa_api_key,
            ..SearchSettings::default()
        })
    }

    pub fn with_settings(settings: SearchSettings) -> Self {
        Self {
            settings,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Use this whenever you're not confident about something you're about \
         to rely on — current events, a library's API or version, a fact you might be wrong \
         about, anything you'd otherwise have to guess — instead of guessing or telling the \
         user to search it themselves. Returns a short list of results (title, URL, snippet, \
         and a publication date when the backend reports one)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "num_results": {
                    "type": "integer",
                    "description": "How many results to return (default 5, max 10)."
                }
            },
            "required": ["query"]
        })
    }

    fn permission_class(&self) -> PermissionClass {
        // No local side effects — same reasoning as the read-only file
        // tools, just over the network instead of the filesystem.
        PermissionClass::ReadOnly
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> ToolResult {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            return ToolResult::error("web_search requires a non-empty `query`");
        }
        let num_results = input
            .get("num_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_NUM_RESULTS)
            .clamp(1, MAX_NUM_RESULTS) as usize;

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        if let Some(hit) = self.cached(query, num_results) {
            return ToolResult::ok(format_results(
                &format!(
                    "{}, cached from an identical query earlier this session",
                    hit.source
                ),
                query,
                &today,
                &hit.results,
            ));
        }

        match self.run_backends(query, num_results, ctx, &cancel).await {
            Ok((source, results)) => {
                self.remember(query, &source, &results);
                ToolResult::ok(format_results(&source, query, &today, &results))
            }
            Err(failures) => ToolResult::error(failure_message(&failures)),
        }
    }
}

impl WebSearchTool {
    /// Walks the tiers, collecting why each one could not answer.
    ///
    /// The first backend that *runs* wins, even when it found nothing —
    /// "searched and found nothing" is a real answer and must not be retried
    /// against a weaker engine as if it were a failure.
    ///
    /// Each attempted tier reports one progress line through `ctx`, so the
    /// card in the TUI shows *which* backend a slow search is waiting on
    /// instead of a bare spinner. A no-op when nothing is attached.
    async fn run_backends(
        &self,
        query: &str,
        limit: usize,
        ctx: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<(String, Vec<SearchResult>), Vec<(&'static str, Unavailable)>> {
        let mut failures: Vec<(&'static str, Unavailable)> = Vec::new();

        macro_rules! tier {
            ($label:expr, $call:expr) => {
                ctx.report_progress(format!("trying {}…", $label));
                match $call {
                    Ok(results) => return Ok(($label.to_string(), results)),
                    Err(e) => failures.push(($label, e)),
                }
            };
        }

        match &self.settings.searxng_url {
            Some(url) if !url.trim().is_empty() => {
                tier!(
                    "SearXNG",
                    crate::searxng::search(&self.client, url, query, limit).await
                );
            }
            _ => failures.push((
                "SearXNG",
                Unavailable::NotConfigured("no `[search] searxng_url` configured".into()),
            )),
        }

        match &self.settings.exa_api_key {
            Some(key) if !key.trim().is_empty() => {
                tier!("Exa", self.search_exa(key, query, limit).await);
            }
            // Deliberately not probed without a key: measured HTTP 402.
            _ => failures.push((
                "Exa",
                Unavailable::NotConfigured("no `[exa] api_key` configured".into()),
            )),
        }

        match &self.settings.tavily_api_key {
            Some(key) if !key.trim().is_empty() => {
                tier!("Tavily", self.search_tavily(key, query, limit).await);
            }
            _ => failures.push((
                "Tavily",
                Unavailable::NotConfigured("no `[tavily] api_key` configured".into()),
            )),
        }

        tier!("Bing", self.search_bing(query, limit, cancel).await);
        tier!(
            "Bing via headless browser",
            self.search_bing_browser(query, limit, cancel).await
        );
        tier!("Google News", self.search_google_news(query, limit).await);
        tier!(
            "DuckDuckGo",
            search_duckduckgo_lite(&self.client, query, limit).await
        );

        Err(failures)
    }

    /// Bing over plain HTTP, retried across markets and then over time.
    ///
    /// The retries answer measured failures: a market that does not match the
    /// query's language yields a poisoned result set, and a transport error or
    /// a 429 clears on its own. Attempts stop at [`BING_MAX_ATTEMPTS`].
    ///
    /// A `Weak` set — one coincidentally-matching term, see
    /// [`crate::bing::Relevance`] — is kept as a fallback while the next
    /// market gets a try, with no backoff in between (weakness is a relevance
    /// judgement, not a throttle). It is only returned once every market has
    /// had its chance to do better.
    async fn search_bing(
        &self,
        query: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchResult>, Unavailable> {
        let markets = crate::bing::markets_to_try(
            self.settings.market.as_deref(),
            system_locale().as_deref(),
            query,
        );

        let mut weak_fallback: Option<Vec<SearchResult>> = None;
        let mut last = Unavailable::Transient("no attempt was made".into());
        for attempt in 1..=BING_MAX_ATTEMPTS {
            // Cycle the markets, so a second pass re-tries the primary one
            // after a pause rather than giving a third market a turn.
            let market = &markets[(attempt as usize - 1) % markets.len()];
            match self.bing_once(query, market, limit).await {
                Ok((results, crate::bing::Relevance::Good)) => return Ok(results),
                Ok((results, _weak)) => {
                    // First weak set wins the fallback slot: it came from the
                    // best-ranked market.
                    weak_fallback.get_or_insert(results);
                    continue;
                }
                Err(e) => last = e,
            }
            if attempt < BING_MAX_ATTEMPTS && !sleep_backoff(attempt, cancel).await {
                return Err(Unavailable::Transient("cancelled".into()));
            }
        }
        if let Some(results) = weak_fallback {
            return Ok(results);
        }
        Err(last)
    }

    async fn bing_once(
        &self,
        query: &str,
        market: &str,
        limit: usize,
    ) -> Result<(Vec<SearchResult>, crate::bing::Relevance), Unavailable> {
        let url = crate::bing::search_url(query, market).map_err(Unavailable::Misconfigured)?;
        let language = crate::bing::language_of(market);
        let resp = self
            .client
            .get(&url)
            .header("User-Agent", BROWSER_USER_AGENT)
            .header(
                "Accept",
                "application/rss+xml, application/xml;q=0.9, */*;q=0.8",
            )
            // Coherent with the market being tried — an `en-US` header on a
            // `pt-BR` request is one more mismatched signal.
            .header("Accept-Language", format!("{market},{language};q=0.9"))
            .send()
            .await
            .map_err(|e| Unavailable::Transient(format!("could not reach Bing: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Unavailable::Transient(format!(
                "Bing returned HTTP {status}"
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| Unavailable::Transient(format!("could not read Bing's response: {e}")))?;

        classify_bing(query, &body, limit, market)
    }

    /// The same RSS feed, fetched by a real browser.
    ///
    /// Worth a tier of its own because it is a genuinely different network
    /// path — a different TLS stack and HTTP/2 fingerprint — so a host that
    /// intercepts or fingerprints plain requests can still be searched from.
    /// Chromium's XML viewer leaves the feed's markup intact in the dumped DOM,
    /// so this shares [`crate::bing::parse_rss`] with the tier above.
    async fn search_bing_browser(
        &self,
        query: &str,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<SearchResult>, Unavailable> {
        if !crate::chromium::is_available() {
            return Err(Unavailable::NotConfigured(
                "no Chrome/Chromium binary found on PATH, in ~/.smith/runtime, or in \
                 SMITH_CHROMIUM_PATH"
                    .into(),
            ));
        }
        let market = crate::bing::markets_to_try(
            self.settings.market.as_deref(),
            system_locale().as_deref(),
            query,
        )
        .swap_remove(0);
        let url = crate::bing::search_url(query, &market).map_err(Unavailable::Misconfigured)?;
        let dom = crate::chromium::fetch(&url, cancel)
            .await
            .map_err(Unavailable::Transient)?;
        // One browser launch is expensive enough that a weak set is taken
        // as-is rather than paying for a second one.
        classify_bing(query, &dom, limit, &market).map(|(results, _relevance)| results)
    }

    /// Google News' RSS search — keyless, and the one free tier whose
    /// `published` dates are real. See [`crate::google_news`].
    async fn search_google_news(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, Unavailable> {
        let url = crate::google_news::search_url(query).map_err(Unavailable::Misconfigured)?;
        let resp = self
            .client
            .get(&url)
            .header("User-Agent", BROWSER_USER_AGENT)
            .header(
                "Accept",
                "application/rss+xml, application/xml;q=0.9, */*;q=0.8",
            )
            .send()
            .await
            .map_err(|e| Unavailable::Transient(format!("could not reach Google News: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Unavailable::Transient(format!(
                "Google News returned HTTP {status}"
            )));
        }
        let body = resp.text().await.map_err(|e| {
            Unavailable::Transient(format!("could not read Google News' response: {e}"))
        })?;
        // No poison check here: Google News answers an off-topic query with
        // an empty feed, not with unrelated results — and empty is a real
        // answer ("no news about this"), reported as such.
        Ok(crate::google_news::parse_rss(&body, limit))
    }

    /// Tavily's search API. `Err` carries why, same contract as Exa.
    async fn search_tavily(
        &self,
        key: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, Unavailable> {
        let resp = self
            .client
            .post(TAVILY_SEARCH_URL)
            .header("Authorization", format!("Bearer {key}"))
            .json(&serde_json::json!({
                "query": query,
                "max_results": limit,
            }))
            .send()
            .await
            .map_err(|e| Unavailable::Transient(format!("could not reach Tavily: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => {
                    Unavailable::Misconfigured("the configured API key was rejected".into())
                }
                432 | 433 => {
                    Unavailable::Misconfigured("the account's plan limit was exceeded".into())
                }
                429 => Unavailable::Transient("rate limited".into()),
                _ => Unavailable::Transient(format!("HTTP {status}")),
            });
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Unavailable::Transient(e.to_string()))?;
        Ok(parse_tavily_response(&body, limit))
    }

    /// `Err` carries why this tier could not answer, so the caller can tell
    /// the user what to fix rather than reporting an empty result set.
    async fn search_exa(
        &self,
        key: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, Unavailable> {
        let resp = self
            .client
            .post(EXA_SEARCH_URL)
            .header("x-api-key", key)
            .json(&serde_json::json!({
                "query": query,
                "numResults": limit,
                "contents": { "text": { "maxCharacters": 500 } },
            }))
            .send()
            .await
            .map_err(|e| Unavailable::Transient(format!("could not reach Exa: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => {
                    Unavailable::Misconfigured("the configured API key was rejected".into())
                }
                402 => Unavailable::Misconfigured(
                    "the account is out of credit (HTTP 402 Payment Required)".into(),
                ),
                429 => Unavailable::Transient("rate limited".into()),
                _ => Unavailable::Transient(format!("HTTP {status}")),
            });
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| Unavailable::Transient(e.to_string()))?;
        Ok(parse_exa_response(&body, limit))
    }

    /// A cached hit for `query`, when at least `limit` results were stored.
    ///
    /// A narrower request is served by slicing; a wider one re-searches, since
    /// serving three results to a caller that asked for ten would silently lose
    /// the rest.
    fn cached(&self, query: &str, limit: usize) -> Option<CacheEntry> {
        let cache = self.cache.lock().ok()?;
        let entry = cache.get(&cache_key(query))?;
        (entry.results.len() >= limit).then(|| CacheEntry {
            source: entry.source.clone(),
            results: entry.results[..limit].to_vec(),
        })
    }

    fn remember(&self, query: &str, source: &str, results: &[SearchResult]) {
        // An empty result set is not cached: it is the outcome most likely to
        // be a transient upstream hiccup, and pinning it for the session would
        // make a query permanently unanswerable.
        if results.is_empty() {
            return;
        }
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        // Crude eviction — dropping everything rather than tracking use order.
        // At this size the only thing an LRU would buy is complexity.
        if cache.len() >= CACHE_CAPACITY {
            cache.clear();
        }
        cache.insert(
            cache_key(query),
            CacheEntry {
                source: source.to_string(),
                results: results.to_vec(),
            },
        );
    }
}

/// Turns a Bing response body into results-with-a-verdict or a reason.
///
/// Split out so the plain-HTTP and browser tiers cannot drift on what counts
/// as a poisoned page. A poisoned set is an error (retry under another
/// market); a merely `Weak` one is handed back with its verdict so the caller
/// decides whether it can afford another attempt.
fn classify_bing(
    query: &str,
    body: &str,
    limit: usize,
    market: &str,
) -> Result<(Vec<SearchResult>, crate::bing::Relevance), Unavailable> {
    // Parse the whole feed, not just `limit` rows: poison detection is a
    // judgement about the response, and ten rows make it far surer than three.
    let mut all = crate::bing::parse_rss(body, MAX_NUM_RESULTS as usize);
    if all.is_empty() {
        return Err(Unavailable::Transient(
            "the response carried no results (challenge page or block)".into(),
        ));
    }
    let relevance = crate::bing::judge_relevance(query, &all);
    if relevance == crate::bing::Relevance::Poisoned {
        return Err(Unavailable::Transient(format!(
            "the `{market}` market returned results unrelated to the query, which is how Bing \
             answers a request it is throttling"
        )));
    }
    all.truncate(limit);
    Ok((all, relevance))
}

/// The machine's locale, used only as a *second* Bing market to try.
fn system_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|k| std::env::var(k).ok())
        .filter(|v| !v.trim().is_empty())
}

/// Waits out the backoff for a 1-based `attempt`. `false` if cancelled — the
/// caller must stop rather than fire another request into a cancelled turn.
async fn sleep_backoff(attempt: u32, cancel: &CancellationToken) -> bool {
    let delay = backoff_delay(attempt);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

/// Doubling backoff with equal jitter, matching [`smith_core::retry`].
///
/// Equal jitter (`[d/2, d]`) rather than full jitter: a near-zero draw would
/// re-send into the very window that just refused, wasting the attempt.
fn backoff_delay(attempt: u32) -> Duration {
    let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
    let base = RETRY_BASE_DELAY.saturating_mul(factor).min(RETRY_MAX_DELAY);
    let half = base / 2;
    half + Duration::from_nanos(pseudo_random(half.as_nanos() as u64 + 1))
}

/// A value in `[0, bound)`, seeded from the wall clock. Same reasoning as
/// `smith_core::retry`: jitter needs decorrelation, not unpredictability, so a
/// `rand` dependency would be a poor trade for one modulo.
fn pseudo_random(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x % bound
}

/// Normalises a query so trivially different spellings share a cache entry.
fn cache_key(query: &str) -> String {
    query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// What the model is told when *no* backend could run.
///
/// Two messages, not one, because the right reaction is opposite. A temporary
/// block clears on its own, so the model should say so and offer to try again;
/// telling it "do not retry, this needs configuring" there would be a lie that
/// ends with it answering from memory. Missing configuration genuinely cannot
/// be fixed by another query, so there the instruction is to stop.
///
/// What both share is the part that is load-bearing: this is *not* "no results
/// were found", and the training data is not an acceptable substitute.
fn failure_message(failures: &[(&str, Unavailable)]) -> String {
    let detail = failures
        .iter()
        .map(|(label, e)| format!("  - {label}: {}", e.reason()))
        .collect::<Vec<_>>()
        .join("\n");

    // A transient block anywhere means at least one backend is real and simply
    // busy, which outranks every "not configured" beside it.
    if failures
        .iter()
        .any(|(_, e)| matches!(e, Unavailable::Transient(_)))
    {
        return format!(
            "web_search is TEMPORARILY BLOCKED — the backends are configured and reachable, but \
             they are rate limiting or challenging this machine right now. This is NOT \"no \
             results were found\": nothing was searched.\n\n{detail}\n\nThis usually clears \
             within a minute or two. Say so, and either try the same search again shortly or ask \
             the user whether to wait. Do NOT answer from your training data, and do NOT present \
             a guess as a found fact."
        );
    }

    format!(
        "web_search is UNAVAILABLE — no backend is set up on this machine. This is NOT \"no \
         results were found\": nothing was searched at all.\n\n{detail}\n\nDo not retry with a \
         different query; no query will work until this is fixed, and do not answer from your \
         training data instead. Tell the user plainly that web search is not configured, and \
         that any of these fixes it:\n\
         - point smith at a SearXNG instance: add `[search]` with `searxng_url = \"https://…\"` \
         to ~/.smith/config.toml (JSON output must be enabled there — add `json` under \
         `search: formats:` in its settings.yml);\n\
         - install Chromium or Google Chrome (free; smith drives it headlessly), or point \
         SMITH_CHROMIUM_PATH at an existing binary;\n\
         - set a Tavily API key (free tier): add `[tavily]` with `api_key = \"...\"` to \
         ~/.smith/config.toml (https://app.tavily.com);\n\
         - set an Exa API key: add `[exa]` with `api_key = \"...\"` to ~/.smith/config.toml \
         (https://dashboard.exa.ai)."
    )
}

fn parse_exa_response(body: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    body.get("results")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let title = r.get("title").and_then(|v| v.as_str())?.to_string();
            let url = r.get("url").and_then(|v| v.as_str())?.to_string();
            let snippet = r.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
            let published = r
                .get("publishedDate")
                .and_then(|v| v.as_str())
                .and_then(normalize_published);
            Some(SearchResult {
                title,
                url,
                snippet: snippet.to_string(),
                published,
            })
        })
        .take(limit)
        .collect()
}

fn parse_tavily_response(body: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    body.get("results")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let title = r.get("title").and_then(|v| v.as_str())?.to_string();
            let url = r.get("url").and_then(|v| v.as_str())?.to_string();
            // Tavily's `content` is an extracted passage that can run long;
            // cap it near Exa's 500-character contract so one result cannot
            // crowd the rest out of context.
            let mut snippet = r
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if snippet.chars().count() > 500 {
                snippet = snippet.chars().take(500).collect::<String>() + "…";
            }
            // Only present on news-topic searches, absent otherwise.
            let published = r
                .get("published_date")
                .and_then(|v| v.as_str())
                .and_then(normalize_published);
            Some(SearchResult {
                title,
                url,
                snippet,
                published,
            })
        })
        .take(limit)
        .collect()
}

/// Backends report publication dates as ISO-8601 timestamps
/// (`2024-03-01T00:00:00.000Z`); only the calendar day is useful to the model,
/// and anything that doesn't look like one is dropped rather than shown raw.
pub(crate) fn normalize_published(raw: &str) -> Option<String> {
    let day = raw.split('T').next()?.trim();
    let mut parts = day.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let well_formed = y.len() == 4
        && m.len() == 2
        && d.len() == 2
        && [y, m, d]
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_digit()));
    well_formed.then(|| day.to_string())
}

async fn search_duckduckgo_lite(
    client: &reqwest::Client,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, Unavailable> {
    let resp = client
        .get(DUCKDUCKGO_LITE_URL)
        .query(&[("q", query)])
        .header("User-Agent", BROWSER_USER_AGENT)
        .send()
        .await
        .map_err(|e| Unavailable::Transient(e.to_string()))?;
    let status = resp.status();
    // DuckDuckGo serves its challenge with HTTP 202 rather than a 4xx, so a
    // plain `is_success()` check calls it a good response. Anything that is not
    // a 200 from this endpoint is a refusal.
    if status.as_u16() != 200 {
        return Err(Unavailable::Transient(format!(
            "HTTP {status} (its anti-bot challenge)"
        )));
    }
    let html = resp
        .text()
        .await
        .map_err(|e| Unavailable::Transient(e.to_string()))?;
    let results = parse_duckduckgo_lite(&html, limit);

    // A 200 that parses to nothing is the interesting case. Reporting it as
    // "no results" told the model its query was bad, so it rephrased, failed
    // again, and finally answered from memory. It is a blocked backend, not an
    // empty search — and it is temporary, since the same endpoint has been seen
    // answering properly minutes apart.
    if results.is_empty() {
        return Err(Unavailable::Transient(if looks_like_challenge(&html) {
            "blocked by an anti-bot challenge page".to_string()
        } else {
            "the results page had no parseable results".to_string()
        }));
    }
    Ok(results)
}

/// Whether an HTML body is DuckDuckGo refusing to serve a scraper.
fn looks_like_challenge(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    ["anomaly", "challenge", "captcha", "unusual traffic"]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Minimal, best-effort scrape of DuckDuckGo's lite HTML — a fallback path
/// only, so this doesn't try to be a general HTML parser.
fn parse_duckduckgo_lite(html: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = html;

    while results.len() < limit {
        let Some(anchor_start) = rest.find("<a rel=\"nofollow\"") else {
            break;
        };
        rest = &rest[anchor_start..];

        let Some(href_start) = rest.find("href=\"").map(|i| i + "href=\"".len()) else {
            break;
        };
        let Some(href_end) = rest[href_start..].find('"').map(|i| href_start + i) else {
            break;
        };
        let href = &rest[href_start..href_end];

        let Some(text_start) = rest[href_end..].find('>').map(|i| href_end + i + 1) else {
            break;
        };
        let Some(text_end) = rest[text_start..].find("</a>").map(|i| text_start + i) else {
            break;
        };
        let title = strip_tags(&rest[text_start..text_end]);

        let snippet = rest[text_end..]
            .find("class=\"result-snippet\"")
            .and_then(|snippet_class_offset| {
                let after_class = text_end + snippet_class_offset;
                let cell_start = rest[after_class..].find('>')? + after_class + 1;
                let cell_end = rest[cell_start..].find("</td>")? + cell_start;
                Some(strip_tags(&rest[cell_start..cell_end]))
            })
            .unwrap_or_default();

        rest = &rest[text_end..];

        if let Some(url) = resolve_duckduckgo_redirect(href) {
            if !title.is_empty() {
                results.push(SearchResult {
                    title,
                    url,
                    snippet,
                    // The lite endpoint's markup carries no dates at all, so
                    // this tier can never contribute a recency signal.
                    published: None,
                });
            }
        }
    }

    results
}

/// DuckDuckGo lite links point at `//duckduckgo.com/l/?uddg=<encoded>&rut=…`
/// rather than the target directly — pull the real URL back out.
pub(crate) fn resolve_duckduckgo_redirect(href: &str) -> Option<String> {
    let full = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    let parsed = url::Url::parse(&full).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == "uddg")
        .map(|(_, v)| v.into_owned())
        .or(Some(full))
}

pub(crate) fn strip_tags(fragment: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let mut in_tag = false;
    for c in fragment.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_html_entities(out.trim())
}

/// Decodes the named entities these backends actually emit, plus numeric ones.
///
/// Numeric escapes are not optional here: Bing's RSS writes accented text as
/// `&#231;`, and leaving those raw put literal `&#231;` sequences in front of
/// the model in every non-English result.
pub(crate) fn decode_html_entities(s: &str) -> String {
    let named = s
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ");

    let mut out = String::with_capacity(named.len());
    let mut rest = named.as_str();
    while let Some(start) = rest.find("&#") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find(';').filter(|&i| i > 0 && i <= 8) else {
            out.push_str("&#");
            rest = after;
            continue;
        };
        let digits = &after[..end];
        let code = if let Some(hex) = digits.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            digits.parse::<u32>().ok()
        };
        match code.and_then(char::from_u32) {
            Some(c) => out.push(c),
            // Not a real escape — re-emit just the candidate (`&#`, its
            // contents and the `;`), not the text already written before it.
            None => out.push_str(&rest[start..start + 2 + end + 1]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);

    // Last, so a doubly-escaped `&amp;#39;` does not decode twice into a quote.
    out.replace("&amp;", "&")
}

/// `today` is passed in rather than read from the clock so the formatting
/// stays pure and testable.
fn format_results(source: &str, query: &str, today: &str, results: &[SearchResult]) -> String {
    if results.is_empty() {
        return format!(
            "No results for \"{query}\" (via {source}, searched {today}). Try one refined query \
             — drop or correct the year, reword it, or go at the primary source — before telling \
             the user you couldn't find it. Don't answer from memory instead."
        );
    }
    let mut out = format!("Results for \"{query}\" (via {source}, searched {today}):\n\n");
    for (i, r) in results.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if let Some(published) = &r.published {
            out.push_str(&format!("   published {published}\n"));
        }
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
        out.push('\n');
    }
    out.push_str(
        "(Answer from the results above, not from training knowledge — they may be more current \
         than what you already know, and today is the search date shown above. Where a result \
         carries a `published` date, use it to judge how current it is and prefer the most recent \
         when they disagree. If the results only cover part of the question, report what you did \
         find and then name the gap — don't withhold the whole answer over one uncovered part.)",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(title: &str, url: &str) -> SearchResult {
        SearchResult {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            published: None,
        }
    }

    #[test]
    fn parses_exa_response_into_results() {
        let body = serde_json::json!({
            "results": [
                {"title": "Rust", "url": "https://rust-lang.org", "text": "A systems language."},
                {"title": "No text field", "url": "https://example.com"},
            ]
        });
        let results = parse_exa_response(&body, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].snippet, "A systems language.");
        assert_eq!(results[1].snippet, "");
    }

    #[test]
    fn parses_exa_response_missing_results_key_as_empty() {
        let body = serde_json::json!({"error": "no key"});
        assert!(parse_exa_response(&body, 10).is_empty());
    }

    #[test]
    fn resolves_duckduckgo_redirect_to_real_url() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc123";
        assert_eq!(
            resolve_duckduckgo_redirect(href).as_deref(),
            Some("https://example.com/page")
        );
    }

    #[test]
    fn parses_duckduckgo_lite_result_rows() {
        let html = r#"
            <tr>
            <td valign="top">1.</td>
            <td>
            <a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F&amp;rut=x">Example &amp; Co</a>
            </td>
            </tr>
            <tr>
            <td>&nbsp;</td>
            <td class="result-snippet">A short snippet about the site.</td>
            </tr>
        "#;
        let results = parse_duckduckgo_lite(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example & Co");
        assert_eq!(results[0].url, "https://example.com/");
        assert_eq!(results[0].snippet, "A short snippet about the site.");
    }

    #[test]
    fn parses_duckduckgo_lite_respects_result_limit() {
        let one_row = |n: u32| {
            format!(
                r#"<a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F{n}">Result {n}</a>"#
            )
        };
        let html = (0..5).map(one_row).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_duckduckgo_lite(&html, 2).len(), 2);
    }

    #[test]
    fn parses_duckduckgo_lite_empty_html_as_no_results() {
        assert!(parse_duckduckgo_lite("<html><body>no results</body></html>", 5).is_empty());
    }

    #[test]
    fn format_results_reports_source_and_numbers_entries() {
        let results = vec![SearchResult {
            title: "Title".into(),
            url: "https://example.com".into(),
            snippet: "Snippet.".into(),
            published: None,
        }];
        let text = format_results("Exa", "my query", "2026-08-05", &results);
        assert!(text.contains("via Exa"));
        assert!(text.contains("1. Title"));
        assert!(text.contains("https://example.com"));
        assert!(text.contains("Snippet."));
        assert!(!text.contains("\n   published "));
    }

    #[test]
    fn format_results_anchors_the_search_date_and_shows_publication_dates() {
        let results = vec![SearchResult {
            title: "Title".into(),
            url: "https://example.com".into(),
            snippet: "Snippet.".into(),
            published: Some("2024-03-01".into()),
        }];
        let text = format_results("Exa", "my query", "2026-08-05", &results);
        assert!(text.contains("searched 2026-08-05"), "got: {text}");
        assert!(text.contains("published 2024-03-01"), "got: {text}");
    }

    #[test]
    fn format_results_tells_the_model_to_report_partial_findings() {
        let results = vec![result("Title", "https://example.com")];
        let text = format_results("Exa", "q", "2026-08-05", &results);
        assert!(text.contains("report what you did find"));
        assert!(text.contains("name the gap"));
    }

    #[test]
    fn format_results_empty_is_a_clear_no_results_message() {
        let text = format_results("DuckDuckGo", "my query", "2026-08-05", &[]);
        assert!(text.contains("No results"));
        assert!(text.contains("my query"));
        assert!(text.contains("refined query"), "should suggest a retry");
    }

    #[test]
    fn parses_published_date_from_exa() {
        let body = serde_json::json!({
            "results": [
                {"title": "A", "url": "https://a.com", "publishedDate": "2024-03-01T00:00:00.000Z"},
                {"title": "B", "url": "https://b.com"},
                {"title": "C", "url": "https://c.com", "publishedDate": "not a date"},
            ]
        });
        let results = parse_exa_response(&body, 10);
        assert_eq!(results[0].published.as_deref(), Some("2024-03-01"));
        assert_eq!(results[1].published, None);
        assert_eq!(results[2].published, None, "garbage is dropped, not shown");
    }

    #[test]
    fn normalize_published_accepts_dates_and_rejects_the_rest() {
        assert_eq!(
            normalize_published("2024-03-01T12:30:00Z").as_deref(),
            Some("2024-03-01")
        );
        assert_eq!(
            normalize_published("2024-03-01").as_deref(),
            Some("2024-03-01")
        );
        assert_eq!(normalize_published("2024-3-1"), None);
        assert_eq!(normalize_published("March 2024"), None);
        assert_eq!(normalize_published(""), None);
    }

    #[test]
    fn duckduckgo_results_carry_no_publication_date() {
        let html = r#"<a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F">Example</a>"#;
        let results = parse_duckduckgo_lite(html, 5);
        assert_eq!(results[0].published, None);
    }

    // ---- entity decoding ---------------------------------------------------

    #[test]
    fn decodes_named_and_numeric_entities() {
        assert_eq!(decode_html_entities("Rust &amp; C++"), "Rust & C++");
        assert_eq!(decode_html_entities("Prote&#231;&#227;o"), "Proteção");
        assert_eq!(decode_html_entities("&#x41;&#x42;"), "AB");
        assert_eq!(decode_html_entities("&quot;q&quot;"), "\"q\"");
    }

    /// A bare `&#` in ordinary text must survive rather than swallow the rest
    /// of the string.
    #[test]
    fn leaves_text_that_only_looks_like_an_entity_alone() {
        assert_eq!(decode_html_entities("a &# b"), "a &# b");
        assert_eq!(decode_html_entities("100&#not;"), "100&#not;");
        assert_eq!(decode_html_entities("just text"), "just text");
    }

    // ---- caching -----------------------------------------------------------

    #[test]
    fn cache_key_normalises_spacing_and_case() {
        assert_eq!(cache_key("  Rust   Async  "), cache_key("rust async"));
        assert_ne!(cache_key("rust async"), cache_key("rust sync"));
    }

    /// The behaviour the cache exists for: a second identical query in the same
    /// turn costs no request at all.
    #[test]
    fn a_repeated_query_is_served_from_the_cache() {
        let tool = WebSearchTool::new(None);
        assert!(tool.cached("rust async", 2).is_none());

        tool.remember(
            "rust async",
            "Bing",
            &[result("A", "https://a.com"), result("B", "https://b.com")],
        );

        let hit = tool.cached("  RUST   ASYNC ", 2).expect("should hit");
        assert_eq!(hit.source, "Bing");
        assert_eq!(hit.results.len(), 2);
    }

    /// A narrower request slices the cached rows; a wider one must re-search
    /// rather than silently returning fewer results than were asked for.
    #[test]
    fn the_cache_serves_narrower_requests_but_not_wider_ones() {
        let tool = WebSearchTool::new(None);
        tool.remember(
            "q",
            "Bing",
            &[result("A", "https://a.com"), result("B", "https://b.com")],
        );
        assert_eq!(tool.cached("q", 1).unwrap().results.len(), 1);
        assert_eq!(tool.cached("q", 2).unwrap().results.len(), 2);
        assert!(tool.cached("q", 3).is_none());
    }

    /// An empty result set is the one most likely to be an upstream hiccup;
    /// caching it would make the query unanswerable for the whole session.
    #[test]
    fn an_empty_result_set_is_not_cached() {
        let tool = WebSearchTool::new(None);
        tool.remember("q", "Bing", &[]);
        assert!(tool.cached("q", 1).is_none());
    }

    #[test]
    fn the_cache_stays_bounded() {
        let tool = WebSearchTool::new(None);
        for n in 0..CACHE_CAPACITY + 10 {
            tool.remember(&format!("query {n}"), "Bing", &[result("A", "https://a")]);
        }
        assert!(tool.cache.lock().unwrap().len() <= CACHE_CAPACITY);
    }

    // ---- backoff -----------------------------------------------------------

    #[test]
    fn backoff_doubles_and_stays_inside_the_jitter_window() {
        for attempt in 1..=3u32 {
            let unjittered = RETRY_BASE_DELAY
                .saturating_mul(2u32.pow(attempt - 1))
                .min(RETRY_MAX_DELAY);
            for _ in 0..50 {
                let d = backoff_delay(attempt);
                assert!(
                    d >= unjittered / 2 && d <= unjittered,
                    "attempt {attempt}: {d:?} outside [{:?}, {unjittered:?}]",
                    unjittered / 2
                );
            }
        }
    }

    /// A whole search must not spend longer sleeping than a user will watch.
    #[test]
    fn the_retry_schedule_stays_inside_an_interactive_budget() {
        let total: Duration = (1..BING_MAX_ATTEMPTS).map(backoff_delay).sum();
        assert!(total <= Duration::from_secs(3), "spent {total:?} sleeping");
    }

    // ---- Bing response classification --------------------------------------

    fn feed(items: &[(&str, &str)]) -> String {
        let items: String = items
            .iter()
            .map(|(t, u)| format!("<item><title>{t}</title><link>{u}</link></item>"))
            .collect();
        format!("<rss><channel>{items}</channel></rss>")
    }

    #[test]
    fn a_good_bing_feed_yields_results_truncated_to_the_limit() {
        let xml = feed(&[
            ("Ratatui", "https://ratatui.rs/"),
            ("ratatui on GitHub", "https://github.com/ratatui/ratatui"),
            ("Ratatui docs", "https://docs.rs/ratatui"),
            ("Ratatui tutorial", "https://ratatui.rs/tutorials/"),
        ]);
        let (results, relevance) = classify_bing("ratatui crate docs", &xml, 2, "en-US").unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Ratatui");
        assert_eq!(relevance, crate::bing::Relevance::Good);
    }

    /// A set that matches one lone term of a rich query comes back usable but
    /// flagged, so `search_bing` can prefer another market over settling.
    #[test]
    fn a_weakly_matching_bing_feed_carries_its_verdict() {
        let xml = feed(&[
            ("MEGA - Cloud Storage", "https://mega.nz/a"),
            ("MEGA Pricing", "https://mega.nz/b"),
            ("MEGA Login", "https://mega.nz/c"),
        ]);
        let (results, relevance) =
            classify_bing("resultado da mega sena", &xml, 3, "en-US").unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(relevance, crate::bing::Relevance::Weak);
    }

    /// A challenge page and a poisoned result set are both transient — the
    /// distinction that keeps the model from declaring search permanently dead.
    #[test]
    fn an_empty_or_poisoned_bing_response_is_transient() {
        let empty = classify_bing("q", "<html>challenge</html>", 3, "en-US").unwrap_err();
        assert!(matches!(empty, Unavailable::Transient(_)), "{empty:?}");

        let poisoned = feed(&[
            ("Kopitiam - Lowyat.NET", "https://forum.lowyat.net/a"),
            ("Whatsapp web down?", "https://forum.lowyat.net/b"),
            ("Microsoft Community", "https://answers.microsoft.com/c"),
            ("Petfinder", "https://petfinder.com/d"),
        ]);
        let err = classify_bing("ratatui crate docs", &poisoned, 3, "en-US").unwrap_err();
        match err {
            Unavailable::Transient(reason) => {
                // The market has to be named: it is the setting the user would
                // change if this turned out to be permanent for them.
                assert!(reason.contains("en-US"), "got: {reason}");
                assert!(reason.contains("throttling"), "got: {reason}");
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    // ---- "unavailable" vs "blocked" vs "found nothing" ---------------------

    #[test]
    fn a_challenge_page_is_detected_rather_than_parsed_as_zero_results() {
        let challenge = r#"<!DOCTYPE html><html><head><title>DuckDuckGo</title></head>
            <body><p>Our systems have detected unusual traffic (anomaly detected).
            Please complete the challenge to continue.</p></body></html>"#;
        assert!(looks_like_challenge(challenge));
        assert!(
            parse_duckduckgo_lite(challenge, 5).is_empty(),
            "the premise: a challenge page parses to nothing"
        );
    }

    #[test]
    fn a_genuine_results_page_is_not_mistaken_for_a_challenge() {
        let html = r#"<a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F">Example</a>"#;
        assert!(!looks_like_challenge(html));
        assert_eq!(parse_duckduckgo_lite(html, 5).len(), 1);
    }

    #[test]
    fn nothing_configured_reads_as_unavailable_and_tells_the_model_to_stop() {
        let msg = failure_message(&[
            (
                "SearXNG",
                Unavailable::NotConfigured("no `[search] searxng_url` configured".into()),
            ),
            (
                "Exa",
                Unavailable::NotConfigured("no `[exa] api_key` configured".into()),
            ),
            (
                "Bing via headless browser",
                Unavailable::NotConfigured("no Chrome/Chromium binary found".into()),
            ),
        ]);

        assert!(msg.contains("UNAVAILABLE"));
        assert!(msg.contains("NOT \"no results were found\""));
        // Retrying is what the model actually did, four times, before giving
        // up and answering from training data. Both halves are forbidden.
        assert!(msg.contains("Do not retry"));
        assert!(msg.contains("do not answer from your training data"));
        // Every reason survives, so the user learns which fix applies to them.
        assert!(msg.contains("searxng_url"));
        assert!(msg.contains("no Chrome/Chromium binary"));
        // And every remedy is named.
        assert!(msg.contains("SMITH_CHROMIUM_PATH"));
        assert!(msg.contains("dashboard.exa.ai"));
        assert!(msg.contains("settings.yml"));
    }

    /// The third case, and the reason this function is not one string: a
    /// backend that is merely busy must not be reported as one that needs
    /// configuring, or the user is sent to fix something that is not broken.
    #[test]
    fn a_transient_block_reads_as_temporary_and_invites_a_retry() {
        let msg = failure_message(&[
            (
                "SearXNG",
                Unavailable::NotConfigured("no `[search] searxng_url` configured".into()),
            ),
            (
                "Bing",
                Unavailable::Transient("the `en-US` market returned unrelated results".into()),
            ),
            (
                "DuckDuckGo",
                Unavailable::Transient("blocked by an anti-bot challenge page".into()),
            ),
        ]);

        assert!(msg.contains("TEMPORARILY BLOCKED"), "got: {msg}");
        assert!(!msg.contains("UNAVAILABLE"), "got: {msg}");
        assert!(msg.contains("NOT \"no results were found\""));
        // The opposite instruction from the unavailable case, and the point of
        // separating them.
        assert!(msg.contains("try the same search again shortly"));
        assert!(!msg.contains("Do not retry"));
        // Still never a licence to answer from memory.
        assert!(msg.contains("Do NOT answer from your training data"));
    }

    /// A misconfigured backend the user owns is actionable, not transient —
    /// it belongs in the "fix this" message, naming what to fix.
    #[test]
    fn a_misconfigured_searxng_is_reported_as_something_to_fix() {
        let msg = failure_message(&[(
            "SearXNG",
            Unavailable::Misconfigured(crate::searxng::FORMAT_DISABLED_HINT.to_string()),
        )]);
        assert!(msg.contains("UNAVAILABLE"));
        assert!(msg.contains("format=json"));
        assert!(msg.contains("settings.yml"));
    }

    #[test]
    fn an_empty_result_set_from_a_working_backend_still_reads_as_no_results() {
        // The other side of the distinction: a backend that genuinely ran and
        // found nothing must still invite a refined query.
        let text = format_results("Exa", "obscure query", "2026-08-05", &[]);
        assert!(text.contains("No results"));
        assert!(!text.contains("UNAVAILABLE"));
        assert!(text.contains("refined query"));
    }

    /// The one test that hits the real network, so it is opt-in:
    ///
    /// ```sh
    /// cargo test -p smith-tools --lib web_search -- --ignored --nocapture
    /// ```
    ///
    /// Everything above pins parsing and messaging against fixed input. This is
    /// what catches the free tier being blocked, or starting to answer with the
    /// poisoned result sets that a fixture can only imitate — the failure this
    /// whole module was rewritten around.
    #[tokio::test]
    #[ignore = "requires network access"]
    async fn live_search_returns_results_relevant_to_the_query() {
        let tool = WebSearchTool::new(None);
        let ctx = ToolContext::new(std::env::temp_dir(), "test");
        let result = tool
            .execute(
                serde_json::json!({"query": "ratatui rust crate", "num_results": 3}),
                &ctx,
                CancellationToken::new(),
            )
            .await;

        println!("{}", result.content);
        assert!(!result.is_error, "search failed: {}", result.content);
        assert!(
            result.content.contains("ratatui"),
            "got: {}",
            result.content
        );

        // And the second identical query must not reach the network at all.
        let cached = tool
            .execute(
                serde_json::json!({"query": "  RATATUI   rust crate ", "num_results": 3}),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(
            cached.content.contains("cached from an identical query"),
            "got: {}",
            cached.content
        );
    }

    /// A cached answer says so, so the model does not read a second identical
    /// search as independent confirmation of the first.
    #[test]
    fn a_cached_answer_is_labelled_as_cached() {
        let text = format_results(
            "Bing, cached from an identical query earlier this session",
            "q",
            "2026-08-05",
            &[result("A", "https://a.com")],
        );
        assert!(text.contains("cached from an identical query"));
    }
}
