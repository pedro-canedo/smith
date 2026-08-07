//! One request each, to the six services `run_backends` tries in order.

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

use tokio_util::sync::CancellationToken;

use super::*;

impl WebSearchTool {
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
    pub(super) async fn search_bing(
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

    pub(super) async fn bing_once(
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
    pub(super) async fn search_bing_browser(
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
    pub(super) async fn search_google_news(
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
    pub(super) async fn search_tavily(
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
    pub(super) async fn search_exa(
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
}
