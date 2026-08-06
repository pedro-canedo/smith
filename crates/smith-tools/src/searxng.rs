//! SearXNG as the first-choice `web_search` backend, when the user runs one.
//!
//! Every other free tier in this crate is scraping something that would rather
//! not be scraped, and it shows: measured against the public internet, five of
//! seven candidate engines answered a challenge page, a captcha or a 429. A
//! SearXNG instance the user hosts themselves has none of those problems —
//! no shared IP reputation, no rate limit they did not set, and no anti-bot
//! layer aimed at them. That is why it is tried *first* whenever it is
//! configured, ahead of even a paid Exa key.
//!
//! ## What the user has to configure
//!
//! SearXNG's JSON output is **disabled by default** in current versions; the
//! `/search?format=json` this module uses answers HTTP 403 until an admin opts
//! in. In `settings.yml`:
//!
//! ```yaml
//! search:
//!   formats:
//!     - html
//!     - json
//! ```
//!
//! then restart the instance. [`FORMAT_DISABLED_HINT`] is what the model and
//! the user are told when that step is missing, because "403 Forbidden"
//! against your own server is otherwise a genuinely baffling thing to debug.

use crate::web_search::{SearchResult, Unavailable};

/// Builds the JSON search URL under `base`.
///
/// A trailing slash on the configured base (or its absence) must not change
/// the endpoint, so the path is joined rather than concatenated.
pub(crate) fn search_url(base: &str, query: &str, limit: usize) -> Result<String, String> {
    let base = base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("the configured URL is empty".to_string());
    }
    let mut url = url::Url::parse(&format!("{base}/search"))
        .map_err(|e| format!("`{base}` is not a valid URL: {e}"))?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "json")
        // SearXNG paginates at ~10 results; asking for one page is enough for
        // every `limit` this tool allows and keeps the response small.
        .append_pair("pageno", "1");
    let _ = limit;
    Ok(url.into())
}

/// Told to the user when the instance is up but refusing JSON — by far the
/// most likely way a correctly-typed URL still fails.
pub(crate) const FORMAT_DISABLED_HINT: &str =
    "the instance refused `format=json` (HTTP 403). SearXNG disables JSON output by \
     default: add `json` under `search: formats:` in its settings.yml and restart it";

/// Runs one query against the configured instance.
pub(crate) async fn search(
    client: &reqwest::Client,
    base: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, Unavailable> {
    let url = search_url(base, query, limit).map_err(Unavailable::Misconfigured)?;

    let resp = client
        .get(&url)
        // A self-hosted instance has no reason to fingerprint its own user, but
        // some deployments sit behind a proxy that rejects clients with no UA.
        .header("User-Agent", crate::web_search::BROWSER_USER_AGENT)
        .header("Accept", "application/json")
        .send()
        .await
        // Unreachable is a *misconfiguration*, not a transient block: the user
        // pointed smith at this host on purpose, and a typo or a stopped
        // container will not fix itself on a retry.
        .map_err(|e| Unavailable::Misconfigured(format!("could not reach `{base}`: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            403 => Unavailable::Misconfigured(FORMAT_DISABLED_HINT.to_string()),
            // A self-hosted instance rate-limits when its own limiter is on, or
            // when the upstream engines it proxies are throttling it. Both pass.
            429 => Unavailable::Transient("the instance returned HTTP 429".to_string()),
            _ => Unavailable::Misconfigured(format!("the instance returned HTTP {status}")),
        });
    }

    let body: serde_json::Value = resp
        .json()
        .await
        // HTML where JSON was expected is what an instance serving only the
        // `html` format looks like on a 200.
        .map_err(|_| Unavailable::Misconfigured(FORMAT_DISABLED_HINT.to_string()))?;

    Ok(parse_response(&body, limit))
}

/// Reads SearXNG's `results` array.
///
/// `content` is the snippet and `publishedDate` is a genuine publication date
/// when an upstream engine supplied one — unlike Bing's RSS `pubDate`, which is
/// a crawl timestamp. That makes this the only free tier that can contribute a
/// recency signal.
pub(crate) fn parse_response(body: &serde_json::Value, limit: usize) -> Vec<SearchResult> {
    body.get("results")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let url = r.get("url").and_then(|v| v.as_str())?.to_string();
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
            if title.is_empty() {
                return None;
            }
            Some(SearchResult {
                title: title.to_string(),
                url,
                snippet: r
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                published: r
                    .get("publishedDate")
                    .and_then(|v| v.as_str())
                    .and_then(crate::web_search::normalize_published),
            })
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_json_search_url() {
        let url = search_url("https://searx.example.com", "rust & c++", 5).unwrap();
        assert!(
            url.starts_with("https://searx.example.com/search?"),
            "{url}"
        );
        assert!(url.contains("q=rust+%26+c%2B%2B"), "{url}");
        assert!(url.contains("format=json"), "{url}");
    }

    /// A trailing slash is the most likely way a user writes the URL, and it
    /// must not produce `//search`.
    #[test]
    fn a_trailing_slash_does_not_change_the_endpoint() {
        let with = search_url("https://searx.example.com/", "q", 5).unwrap();
        let without = search_url("https://searx.example.com", "q", 5).unwrap();
        assert_eq!(with, without);
    }

    #[test]
    fn honours_a_subpath_deployment() {
        let url = search_url("https://example.com/searx", "q", 5).unwrap();
        assert!(
            url.starts_with("https://example.com/searx/search?"),
            "{url}"
        );
    }

    #[test]
    fn rejects_an_unusable_base_url() {
        assert!(search_url("   ", "q", 5).is_err());
        assert!(search_url("not a url", "q", 5).is_err());
    }

    #[test]
    fn parses_results_with_snippets_and_publication_dates() {
        let body = serde_json::json!({
            "results": [
                {
                    "url": "https://ratatui.rs/",
                    "title": "Ratatui",
                    "content": "Terminal UIs in Rust",
                    "publishedDate": "2024-03-01T00:00:00"
                },
                {"url": "https://example.com/", "title": "No snippet"},
            ]
        });
        let results = parse_response(&body, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Ratatui");
        assert_eq!(results[0].snippet, "Terminal UIs in Rust");
        assert_eq!(results[0].published.as_deref(), Some("2024-03-01"));
        assert_eq!(results[1].snippet, "");
        assert_eq!(results[1].published, None);
    }

    #[test]
    fn drops_rows_with_no_url_or_no_title() {
        let body = serde_json::json!({
            "results": [
                {"title": "No url"},
                {"url": "https://example.com/", "title": "  "},
                {"url": "https://good.example/", "title": "Good"},
            ]
        });
        let results = parse_response(&body, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Good");
    }

    #[test]
    fn honours_the_result_limit() {
        let results: Vec<serde_json::Value> = (0..10)
            .map(|n| serde_json::json!({"url": format!("https://e.com/{n}"), "title": "T"}))
            .collect();
        let body = serde_json::json!({ "results": results });
        assert_eq!(parse_response(&body, 3).len(), 3);
    }

    #[test]
    fn a_response_with_no_results_key_is_empty_rather_than_an_error() {
        assert!(parse_response(&serde_json::json!({}), 5).is_empty());
    }

    /// The hint has to name the exact file and key, because a 403 from your own
    /// server is otherwise indistinguishable from a broken URL.
    #[test]
    fn the_format_hint_names_the_setting_to_change() {
        assert!(FORMAT_DISABLED_HINT.contains("settings.yml"));
        assert!(FORMAT_DISABLED_HINT.contains("formats"));
        assert!(FORMAT_DISABLED_HINT.contains("json"));
    }
}
