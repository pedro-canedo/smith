//! `web_search` — lets the agent look things up instead of guessing (or,
//! worse, telling the user to run search commands themselves). Three-tier
//! backend, tried in order: Exa (structured, includes extracted page text), a
//! headless Chromium rendering the results page like a real visitor, and
//! finally scraping DuckDuckGo's lite HTML endpoint over plain HTTP. Each tier
//! falls through to the next when it is unreachable, unconfigured, or errors;
//! a query that fails on all three is reported as a single friendly error
//! rather than three stack traces.
//!
//! The middle tier is what keeps this useful with no API key at all: a browser
//! the user already has installed costs nothing per query and sees the page a
//! human would.

use async_trait::async_trait;
use smith_core::{PermissionClass, Tool, ToolContext, ToolResult};

const EXA_SEARCH_URL: &str = "https://api.exa.ai/search";
const DUCKDUCKGO_LITE_URL: &str = "https://lite.duckduckgo.com/lite/";
/// The top three results — title, URL and summary each — is what a synthesised
/// answer actually needs; past that the extra rows mostly cost context. Callers
/// wanting more say so with `num_results`.
const DEFAULT_NUM_RESULTS: u64 = 3;
const MAX_NUM_RESULTS: u64 = 10;
/// Caps each backend attempt so a stalled request falls through to the next
/// tier (or the final error) instead of hanging the whole turn.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// `pub(crate)` so the Chromium backend in `crate::chromium` can produce the
/// same rows every other tier does — the formatting below is then shared, and
/// a result reads identically to the model whichever backend found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchResult {
    pub(crate) title: String,
    pub(crate) url: String,
    pub(crate) snippet: String,
    /// Publication date as `YYYY-MM-DD`, when the backend reports one. This
    /// is the only recency signal the model gets: without it, a five-year-old
    /// page and this morning's are indistinguishable in the result list.
    pub(crate) published: Option<String>,
}

pub struct WebSearchTool {
    exa_api_key: Option<String>,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new(exa_api_key: Option<String>) -> Self {
        Self {
            exa_api_key,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
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
        _ctx: &ToolContext,
        cancel: tokio_util::sync::CancellationToken,
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
            .clamp(1, MAX_NUM_RESULTS);

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        // Why the reasons are collected rather than discarded: "the search ran
        // and found nothing" and "no search backend works here" are completely
        // different facts, and they used to be indistinguishable to the model.
        // Both produced "No results", so the model would rephrase, try again,
        // fail again, and eventually answer from its training data — which is
        // the exact failure this tool exists to prevent. Only a backend that
        // genuinely ran gets to say "nothing found".
        let mut unavailable: Vec<String> = Vec::new();

        match self.search_exa(query, num_results).await {
            Ok(results) => return ToolResult::ok(format_results("Exa", query, &today, &results)),
            Err(reason) => unavailable.push(format!("Exa: {reason}")),
        }

        // Only worth a browser launch if there is a browser: `is_available`
        // is a cached lookup, so skipping the tier on a machine without one
        // costs nothing per query.
        if crate::chromium::is_available() {
            match crate::chromium::search(query, num_results as usize, &cancel).await {
                // An empty page from a browser is far more likely to be a
                // challenge or rate-limit than a genuine zero-result query.
                Ok(results) if !results.is_empty() => {
                    return ToolResult::ok(format_results(
                        "headless Chromium (DuckDuckGo)",
                        query,
                        &today,
                        &results,
                    ));
                }
                Ok(_) => unavailable.push(
                    "headless Chromium: the results page came back empty (challenge or rate limit)"
                        .into(),
                ),
                Err(e) => unavailable.push(format!("headless Chromium: {e}")),
            }
        } else {
            unavailable.push(
                "headless Chromium: no Chrome/Chromium binary found on PATH or in \
                 SMITH_CHROMIUM_PATH"
                    .into(),
            );
        }

        match search_duckduckgo_lite(&self.client, query, num_results).await {
            Ok(results) => ToolResult::ok(format_results("DuckDuckGo", query, &today, &results)),
            Err(reason) => {
                unavailable.push(format!("DuckDuckGo: {reason}"));
                ToolResult::error(unavailable_message(&unavailable))
            }
        }
    }
}

/// What the model is told when *no* backend could run.
///
/// It must not read like "nothing was found", or the model rephrases and
/// retries until it gives up and answers from memory — which is exactly what
/// happened in practice. The instruction is therefore explicit: stop, and tell
/// the user the tool needs configuring. The remedy belongs to the user, not to
/// a better query.
fn unavailable_message(reasons: &[String]) -> String {
    format!(
        "web_search is UNAVAILABLE — no backend could run. This is NOT \"no results were \
         found\": nothing was searched at all.\n\n{}\n\nDo not retry with a different query; \
         no query will work until this is fixed, and do not answer from your training data \
         instead. Tell the user plainly that web search is not configured, and that either \
         of these fixes it:\n\
         - install Chromium or Google Chrome (free; smith drives it headlessly), or point \
         SMITH_CHROMIUM_PATH at an existing binary;\n\
         - set an Exa API key: add `[exa]` with `api_key = \"...\"` to ~/.smith/config.toml \
         (https://dashboard.exa.ai).",
        reasons
            .iter()
            .map(|r| format!("  - {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

impl WebSearchTool {
    /// `Err` carries why this tier could not answer, so the caller can tell
    /// the user what to fix rather than reporting an empty result set.
    async fn search_exa(&self, query: &str, num_results: u64) -> Result<Vec<SearchResult>, String> {
        let mut req = self.client.post(EXA_SEARCH_URL).json(&serde_json::json!({
            "query": query,
            "numResults": num_results,
            "contents": { "text": { "maxCharacters": 500 } },
        }));
        // Send without a key too — some Exa endpoints allow a limited
        // keyless/hosted-free tier; a 401 just falls through to DuckDuckGo.
        if let Some(key) = &self.exa_api_key {
            req = req.header("x-api-key", key);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(match (status.as_u16(), self.exa_api_key.is_some()) {
                // The keyless attempt is a courtesy probe, not a configuration
                // error — saying "no API key configured" is the actionable half.
                (401 | 403, false) => "no API key configured".to_string(),
                (401 | 403, true) => "the configured API key was rejected".to_string(),
                (429, _) => "rate limited".to_string(),
                _ => format!("HTTP {status}"),
            });
        }
        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parse_exa_response(&body))
    }
}

fn parse_exa_response(body: &serde_json::Value) -> Vec<SearchResult> {
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
        .collect()
}

/// Exa reports publication dates as ISO-8601 timestamps
/// (`2024-03-01T00:00:00.000Z`); only the calendar day is useful to the model,
/// and anything that doesn't look like one is dropped rather than shown raw.
fn normalize_published(raw: &str) -> Option<String> {
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
    num_results: u64,
) -> Result<Vec<SearchResult>, String> {
    let resp = client
        .get(DUCKDUCKGO_LITE_URL)
        .query(&[("q", query)])
        .header("User-Agent", "Mozilla/5.0 (compatible; smith-agent/1.0)")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    let html = resp.text().await.map_err(|e| e.to_string())?;
    let results = parse_duckduckgo_lite(&html, num_results as usize);

    // A 200 that parses to nothing is the interesting case, and it is what
    // this endpoint now returns in practice: DuckDuckGo answers plain HTTP
    // scrapers with an anti-bot challenge page — same status, same content
    // type, no result anchors. Reporting that as "no results" told the model
    // its query was bad, so it rephrased, failed again, and finally answered
    // from memory. It is an unavailable backend, not an empty search.
    if results.is_empty() {
        return Err(if looks_like_challenge(&html) {
            "blocked by an anti-bot challenge page".to_string()
        } else {
            "the results page had no parseable results".to_string()
        });
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
/// only, so this doesn't try to be a general HTML parser. Looks for the
/// `result-link` anchors and their following `result-snippet` cell, and
/// unwraps DuckDuckGo's `/l/?uddg=<encoded target>` redirect links back to
/// the real URL.
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
/// rather than the target directly — pull the real URL back out. Shared with
/// the Chromium backend, whose page uses the same redirect shape.
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

pub(crate) fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
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

    #[test]
    fn parses_exa_response_into_results() {
        let body = serde_json::json!({
            "results": [
                {"title": "Rust", "url": "https://rust-lang.org", "text": "A systems language."},
                {"title": "No text field", "url": "https://example.com"},
            ]
        });
        let results = parse_exa_response(&body);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust");
        assert_eq!(results[0].snippet, "A systems language.");
        assert_eq!(results[1].snippet, "");
    }

    #[test]
    fn parses_exa_response_missing_results_key_as_empty() {
        let body = serde_json::json!({"error": "no key"});
        assert!(parse_exa_response(&body).is_empty());
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
        // No date reported, so no date line — the trailing guidance still
        // mentions the word, hence matching the indented line specifically.
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
        let results = vec![SearchResult {
            title: "Title".into(),
            url: "https://example.com".into(),
            snippet: String::new(),
            published: None,
        }];
        let text = format_results("Exa", "q", "2026-08-05", &results);
        // The whole point of the rewrite: partial coverage must not become a
        // blanket refusal, which is what the old wording produced.
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
        let results = parse_exa_response(&body);
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

    // ---- "unavailable" must never look like "found nothing" ----------------

    #[test]
    fn a_challenge_page_is_detected_rather_than_parsed_as_zero_results() {
        // Trimmed from what lite.duckduckgo.com actually serves a scraper: a
        // 200, correct content type, and not one result anchor.
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
    fn the_unavailable_message_tells_the_model_to_stop_and_report() {
        let msg = unavailable_message(&[
            "Exa: no API key configured".to_string(),
            "headless Chromium: no Chrome/Chromium binary found".to_string(),
            "DuckDuckGo: blocked by an anti-bot challenge page".to_string(),
        ]);

        // The distinction that was missing, stated outright.
        assert!(msg.contains("UNAVAILABLE"));
        assert!(msg.contains("NOT \"no results were found\""));
        // Retrying is what the model actually did, four times, before giving
        // up and answering from training data. Both halves are forbidden.
        assert!(msg.contains("Do not retry"));
        assert!(msg.contains("do not answer from your training data"));
        // Every reason survives, so the user learns which fix applies to them.
        assert!(msg.contains("no API key configured"));
        assert!(msg.contains("no Chrome/Chromium binary"));
        assert!(msg.contains("anti-bot challenge"));
        // And both remedies are named.
        assert!(msg.contains("SMITH_CHROMIUM_PATH"));
        assert!(msg.contains("dashboard.exa.ai"));
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
}
