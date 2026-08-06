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
    /// Pin every search to exactly one backend (`[search] backend`). See
    /// [`WebSearchTool::run_pinned`] for the names and the no-fallback
    /// contract.
    pub backend: Option<String>,
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
        if let Some(pin) = self
            .settings
            .backend
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            return self.run_pinned(pin, query, limit, ctx, cancel).await;
        }

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

    /// Runs exactly the pinned backend — no fallback, Hermes-style.
    ///
    /// The contract is that an explicit pin **wins even when unavailable**: a
    /// pin to `tavily` with no key fails saying "set `[tavily] api_key`"
    /// rather than silently searching somewhere else. Silent rerouting is
    /// worse than the error for exactly the user who pins — someone routing
    /// queries through their own SearXNG for privacy would have them leak to
    /// Bing on the first hiccup, and never know.
    async fn run_pinned(
        &self,
        pin: &str,
        query: &str,
        limit: usize,
        ctx: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<(String, Vec<SearchResult>), Vec<(&'static str, Unavailable)>> {
        // `google_news` and `google-news` are the same intent; so are case
        // variants. Normalising is cheap and the pin is typed by a human.
        let normalized = pin.to_ascii_lowercase().replace('_', "-");

        let (label, outcome): (&'static str, Result<Vec<SearchResult>, Unavailable>) =
            match normalized.as_str() {
                "searxng" => (
                    "SearXNG",
                    match self
                        .settings
                        .searxng_url
                        .as_deref()
                        .filter(|u| !u.trim().is_empty())
                    {
                        Some(url) => {
                            ctx.report_progress("trying SearXNG…".to_string());
                            crate::searxng::search(&self.client, url, query, limit).await
                        }
                        None => Err(Unavailable::Misconfigured(
                            "`[search] backend` is pinned to `searxng` but no `[search] \
                             searxng_url` is set — set one, or remove the pin"
                                .into(),
                        )),
                    },
                ),
                "exa" => (
                    "Exa",
                    match self
                        .settings
                        .exa_api_key
                        .as_deref()
                        .filter(|k| !k.trim().is_empty())
                    {
                        Some(key) => {
                            ctx.report_progress("trying Exa…".to_string());
                            self.search_exa(key, query, limit).await
                        }
                        None => Err(Unavailable::Misconfigured(
                            "`[search] backend` is pinned to `exa` but no `[exa] api_key` is \
                             set — set one, or remove the pin"
                                .into(),
                        )),
                    },
                ),
                "tavily" => (
                    "Tavily",
                    match self
                        .settings
                        .tavily_api_key
                        .as_deref()
                        .filter(|k| !k.trim().is_empty())
                    {
                        Some(key) => {
                            ctx.report_progress("trying Tavily…".to_string());
                            self.search_tavily(key, query, limit).await
                        }
                        None => Err(Unavailable::Misconfigured(
                            "`[search] backend` is pinned to `tavily` but no `[tavily] api_key` \
                             is set — set one, or remove the pin"
                                .into(),
                        )),
                    },
                ),
                "bing" => {
                    ctx.report_progress("trying Bing…".to_string());
                    ("Bing", self.search_bing(query, limit, cancel).await)
                }
                "bing-browser" => {
                    ctx.report_progress("trying Bing via headless browser…".to_string());
                    (
                        "Bing via headless browser",
                        self.search_bing_browser(query, limit, cancel).await,
                    )
                }
                "google-news" => {
                    ctx.report_progress("trying Google News…".to_string());
                    ("Google News", self.search_google_news(query, limit).await)
                }
                "duckduckgo" | "ddg" => {
                    ctx.report_progress("trying DuckDuckGo…".to_string());
                    (
                        "DuckDuckGo",
                        search_duckduckgo_lite(&self.client, query, limit).await,
                    )
                }
                _ => (
                    "web_search config",
                    Err(Unavailable::Misconfigured(format!(
                        "`[search] backend = \"{pin}\"` names no backend — valid values: \
                         searxng, exa, tavily, bing, bing-browser, google-news, duckduckgo \
                         (or remove the key to use the full chain)"
                    ))),
                ),
            };

        match outcome {
            Ok(results) => Ok((format!("{label} (pinned)"), results)),
            Err(e) => Err(vec![(label, e)]),
        }
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
mod tests;
