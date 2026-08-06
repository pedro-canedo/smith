//! Bing as a `web_search` backend, over its RSS output.
//!
//! Why Bing at all: every no-key alternative was measured and found blocked.
//! DuckDuckGo's `html`/`lite` endpoints answer HTTP 202 with a 14 KB challenge
//! page — to a plain HTTP client *and* to a real headless browser. Mojeek
//! serves a captcha, Ecosia a firewall page, Brave a 429, Startpage an empty
//! shell, and Exa's keyless tier now answers 402. DuckDuckGo's JavaScript
//! endpoint loads but renders no results in a headless browser at any virtual
//! time budget. Bing was the one engine that answered 200 with a complete,
//! parseable result set on every attempt.
//!
//! Why RSS rather than scraping the results page: `&format=rss` returns the
//! same ten results as ~5 KB of stable XML instead of ~122 KB of HTML whose
//! class names are an implementation detail, and its `<link>` is the real
//! target URL rather than Bing's `bing.com/ck/a?…&u=a1<base64>` redirect
//! wrapper. It also survives being fetched *through* a browser: Chromium's XML
//! viewer leaves the original markup intact in the DOM, so the plain-HTTP tier
//! and the headless-browser tier share one parser.
//!
//! ## The market parameter is load-bearing
//!
//! Without `setmkt`/`setlang`, Bing answers 200 with ten well-formed results
//! that have *nothing to do with the query* — "ratatui crate docs" returned
//! pages about WhatsApp, MLB trades and pet adoption on five consecutive
//! attempts. That is the dangerous failure mode, because it is structurally
//! indistinguishable from success; only the content gives it away. With
//! `setmkt=en-US&setlang=en` the same query returned the correct results five
//! times out of five. [`looks_poisoned`] is the guard, and it exists because a
//! silent wrong answer is worse than a loud missing one.

use crate::web_search::{decode_html_entities, SearchResult};

/// Bing's RSS rendering of the ordinary results page.
const SEARCH_ENDPOINT: &str = "https://www.bing.com/search";

/// The market used when none is configured.
///
/// `en-US` rather than the machine's locale: an agent's searches are
/// overwhelmingly English technical queries, and a mismatched market poisons
/// the response (measured: an English query under `pt-BR` or `de-DE` scored
/// zero relevant results). Users who mostly search in another language set
/// `[search] market`, and [`markets_to_try`] adds their locale as a second
/// attempt regardless.
pub(crate) const DEFAULT_MARKET: &str = "en-US";

/// The search URL for `query` in `market`.
///
/// Built with the `url` crate rather than by hand so a query containing `&`,
/// `#` or `+` cannot silently turn into a different search.
pub(crate) fn search_url(query: &str, market: &str) -> Result<String, String> {
    url::Url::parse_with_params(
        SEARCH_ENDPOINT,
        &[
            ("q", query),
            ("format", "rss"),
            ("setmkt", market),
            // `setlang` is sent alongside `setmkt` because sending the market
            // alone was not enough to get an un-poisoned response in testing.
            ("setlang", language_of(market)),
        ],
    )
    .map(String::from)
    .map_err(|e| format!("could not build a search URL: {e}"))
}

/// The language half of a `xx-YY` market tag (`pt-BR` → `pt`).
///
/// Falls back to the whole string so a caller that configured a bare `en`
/// still sends something coherent rather than an empty parameter.
fn language_of(market: &str) -> &str {
    market
        .split('-')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(market)
}

/// The markets to attempt, in order, for one search.
///
/// The second entry exists because the market has to match the *query's*
/// language, not the user's: an English query under `pt-BR` and a Portuguese
/// query under `en-US` were both poisoned in testing, while each under its own
/// market was correct. Since the query language is not known up front, a
/// poisoned first attempt is retried under the machine's locale before the
/// backend is written off. Duplicates are dropped so the common case costs one
/// request.
pub(crate) fn markets_to_try(configured: Option<&str>, locale: Option<&str>) -> Vec<String> {
    let primary = configured
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or(DEFAULT_MARKET)
        .to_string();

    let mut markets = vec![primary.clone()];
    if let Some(fallback) = locale.and_then(market_from_locale) {
        if fallback != primary {
            markets.push(fallback);
        }
    }
    markets
}

/// A POSIX locale (`pt_BR.UTF-8`, `de_DE@euro`, `en_US`) as a Bing market tag.
///
/// `None` for anything without a region — `C`, `POSIX` and a bare `en` carry no
/// market, and guessing one from the language alone would be inventing the very
/// mismatch this is meant to avoid.
pub(crate) fn market_from_locale(locale: &str) -> Option<String> {
    let base = locale.split(['.', '@']).next()?.trim();
    let (lang, region) = base.split_once('_').or_else(|| base.split_once('-'))?;
    if lang.len() < 2 || region.len() < 2 {
        return None;
    }
    Some(format!(
        "{}-{}",
        lang.to_ascii_lowercase(),
        region.to_ascii_uppercase()
    ))
}

/// Pulls result rows out of Bing's RSS.
///
/// A scanner rather than an XML parser for the same reason the rest of this
/// crate scans: the only structure that matters is `<item>` with a `<title>`,
/// `<link>` and `<description>`, and missing one of those should cost a result
/// rather than fail the search.
///
/// `<pubDate>` is deliberately ignored. It looks like the recency signal this
/// tool is short of, but every item in a response carries *today's* date at
/// different times of day — it is Bing's crawl timestamp, not the page's
/// publication date. Surfacing it would tell the model that a decade-old page
/// was published this morning, which is worse than admitting the tier has no
/// recency signal at all.
pub(crate) fn parse_rss(xml: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = xml;

    while results.len() < limit {
        let Some(start) = rest.find("<item>") else {
            break;
        };
        let body = &rest[start + "<item>".len()..];
        // A truncated final item is still worth reading: take everything left
        // when the closing tag never arrives.
        let end = body.find("</item>").unwrap_or(body.len());
        let item = &body[..end];
        rest = &body[end..];

        let (Some(title), Some(url)) = (element(item, "title"), element(item, "link")) else {
            continue;
        };
        if title.is_empty() || !url.starts_with("http") {
            continue;
        }
        results.push(SearchResult {
            title,
            url,
            snippet: element(item, "description").unwrap_or_default(),
            published: None,
        });
    }

    results
}

/// The decoded text of the first `<name>…</name>` in `item`.
fn element(item: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = item.find(&open)? + open.len();
    let end = item[start..].find(&close)? + start;
    let raw = &item[start..end];
    // Bing does not use CDATA here, but a feed that did would otherwise hand
    // the model a literal `<![CDATA[` prefix.
    let raw = raw
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(raw);
    Some(decode_html_entities(raw.trim()))
}

/// Whether a result set is Bing's poisoned response rather than a real answer.
///
/// The measured signature is absolute: a poisoned set has **no** result whose
/// title, URL or snippet contains **any** term from the query, across all ten
/// rows, while a genuine set matches on most of them. Requiring total absence
/// rather than a ratio is what keeps this from discarding real results for an
/// obscure query, where a legitimate answer may echo very little of the
/// wording back.
///
/// A short result set is never judged: one or two rows can honestly miss every
/// term, and there is not enough evidence there to overrule the engine.
pub(crate) fn looks_poisoned(query: &str, results: &[SearchResult]) -> bool {
    const MIN_RESULTS_TO_JUDGE: usize = 3;

    if results.len() < MIN_RESULTS_TO_JUDGE {
        return false;
    }
    let terms = query_terms(query);
    if terms.is_empty() {
        return false;
    }
    !results.iter().any(|r| {
        let haystack = format!("{} {} {}", r.title, r.url, r.snippet).to_lowercase();
        terms.iter().any(|t| haystack.contains(t))
    })
}

/// The query's content words, lowercased.
///
/// Splits on every non-alphanumeric character so `serde_json` contributes both
/// `serde` and `json` — matching only the whole token made a correct answer
/// (`Overview · Serde`) look poisoned. Terms shorter than three characters are
/// dropped: they match almost any text and would mask a genuinely poisoned set.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
        .map(|t| t.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, link: &str, description: &str) -> String {
        format!(
            "<item><title>{title}</title><link>{link}</link>\
             <description>{description}</description>\
             <pubDate>Wed, 05 Aug 2026 22:23:00 GMT</pubDate></item>"
        )
    }

    fn feed(items: &[String]) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\" ?><rss version=\"2.0\"><channel>\
             <title>Bing: q</title>{}</channel></rss>",
            items.join("")
        )
    }

    #[test]
    fn search_url_carries_the_query_format_and_market() {
        let url = search_url("rust & c++ #1", "en-US").unwrap();
        assert!(url.starts_with(SEARCH_ENDPOINT), "got: {url}");
        assert!(url.contains("q=rust+%26+c%2B%2B+%231"), "got: {url}");
        assert!(url.contains("format=rss"), "got: {url}");
        assert!(url.contains("setmkt=en-US"), "got: {url}");
        assert!(url.contains("setlang=en"), "got: {url}");
    }

    #[test]
    fn setlang_is_the_language_half_of_the_market() {
        let url = search_url("q", "pt-BR").unwrap();
        assert!(url.contains("setmkt=pt-BR"), "got: {url}");
        assert!(url.contains("setlang=pt"), "got: {url}");
        // A market with no region still produces a coherent pair.
        assert_eq!(language_of("en"), "en");
        assert_eq!(language_of("pt-BR"), "pt");
    }

    #[test]
    fn parses_title_link_and_description_from_a_feed() {
        let xml = feed(&[item(
            "Ratatui",
            "https://ratatui.rs/",
            "Cook up terminal user interfaces in Rust",
        )]);
        let results = parse_rss(&xml, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Ratatui");
        assert_eq!(results[0].url, "https://ratatui.rs/");
        assert_eq!(
            results[0].snippet,
            "Cook up terminal user interfaces in Rust"
        );
    }

    /// Bing's crawl timestamp must never be presented as a publication date.
    #[test]
    fn pubdate_is_not_reported_as_a_publication_date() {
        let xml = feed(&[item("T", "https://e.com/", "d")]);
        assert_eq!(parse_rss(&xml, 5)[0].published, None);
    }

    #[test]
    fn respects_the_result_limit_and_keeps_page_order() {
        let items: Vec<String> = (1..=8)
            .map(|n| item(&format!("Title {n}"), &format!("https://e.com/{n}"), "s"))
            .collect();
        let results = parse_rss(&feed(&items), 3);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].title, "Title 1");
        assert_eq!(results[2].title, "Title 3");
    }

    #[test]
    fn decodes_entities_in_titles_and_snippets() {
        let xml = feed(&[item(
            "Rust &amp; C++",
            "https://e.com/",
            "Compared &#231;ompletely &quot;side by side&quot;",
        )]);
        let results = parse_rss(&xml, 5);
        assert_eq!(results[0].title, "Rust & C++");
        assert_eq!(results[0].snippet, "Compared çompletely \"side by side\"");
    }

    #[test]
    fn skips_items_missing_a_title_or_a_usable_link() {
        let xml = feed(&[
            "<item><link>https://e.com/</link></item>".to_string(),
            "<item><title>No link</title></item>".to_string(),
            "<item><title>Relative</title><link>/nope</link></item>".to_string(),
            item("Good", "https://good.example/", "s"),
        ]);
        let results = parse_rss(&xml, 5);
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].title, "Good");
    }

    #[test]
    fn unwraps_cdata_sections() {
        let xml = "<item><title><![CDATA[Raw & Wild]]></title>\
                   <link>https://e.com/</link></item>";
        assert_eq!(parse_rss(xml, 5)[0].title, "Raw & Wild");
    }

    /// A challenge page, an empty feed, or markup cut off mid-write all have to
    /// come back as a clean empty list rather than panicking on a slice.
    #[test]
    fn malformed_or_truncated_markup_stops_cleanly() {
        assert!(parse_rss("", 3).is_empty());
        assert!(parse_rss("<html><body>nope</body></html>", 3).is_empty());
        let full = feed(&[item("T", "https://e.com/", "Some snippet text")]);
        for cut in 0..full.len() {
            let _ = parse_rss(&full[..cut], 3);
        }
    }

    // ---- poison detection --------------------------------------------------

    /// The exact failure observed against the live endpoint: ten well-formed
    /// results, none of them about the query.
    #[test]
    fn detects_a_poisoned_result_set() {
        let results: Vec<SearchResult> = [
            ("Kopitiam - Lowyat.NET", "https://forum.lowyat.net/Kopitiam"),
            (
                "Is Whatsapp web down?",
                "https://forum.lowyat.net/topic/5538738",
            ),
            (
                "Crash Reports - Microsoft Q&A",
                "https://answers.microsoft.com/x",
            ),
            ("Microsoft Community", "https://answers.microsoft.com/y"),
        ]
        .iter()
        .map(|(t, u)| SearchResult {
            title: (*t).into(),
            url: (*u).into(),
            snippet: String::new(),
            published: None,
        })
        .collect();
        assert!(looks_poisoned("ratatui crate docs", &results));
    }

    #[test]
    fn a_genuine_result_set_is_not_called_poisoned() {
        let results: Vec<SearchResult> = [
            ("Ratatui", "https://ratatui.rs/"),
            ("Some unrelated page", "https://example.com/"),
            ("Another unrelated page", "https://example.org/"),
        ]
        .iter()
        .map(|(t, u)| SearchResult {
            title: (*t).into(),
            url: (*u).into(),
            snippet: String::new(),
            published: None,
        })
        .collect();
        // One matching row out of three is enough — the signature being
        // detected is *total* absence.
        assert!(!looks_poisoned("ratatui crate docs", &results));
    }

    /// The false positive that a whole-token match produced: `serde_json`
    /// against a correct result titled "Overview · Serde".
    #[test]
    fn an_underscored_query_term_matches_either_half() {
        let results: Vec<SearchResult> = (0..4)
            .map(|n| SearchResult {
                title: "Overview · Serde".into(),
                url: format!("https://serde.rs/{n}"),
                snippet: String::new(),
                published: None,
            })
            .collect();
        assert!(!looks_poisoned(
            "serde_json arbitrary precision feature",
            &results
        ));
    }

    /// Too little evidence to overrule the engine: an obscure two-hit query
    /// may legitimately echo nothing back.
    #[test]
    fn a_short_result_set_is_never_judged_poisoned() {
        let results = vec![
            SearchResult {
                title: "Nothing alike".into(),
                url: "https://a.example/".into(),
                snippet: String::new(),
                published: None,
            },
            SearchResult {
                title: "Also nothing alike".into(),
                url: "https://b.example/".into(),
                snippet: String::new(),
                published: None,
            },
        ];
        assert!(!looks_poisoned("ratatui crate docs", &results));
        assert!(!looks_poisoned("q", &[]));
    }

    /// A query made only of short words yields no usable terms, so the check
    /// abstains instead of condemning every result.
    #[test]
    fn a_query_with_no_long_terms_abstains() {
        let results: Vec<SearchResult> = (0..4)
            .map(|n| SearchResult {
                title: "Unrelated".into(),
                url: format!("https://e.com/{n}"),
                snippet: String::new(),
                published: None,
            })
            .collect();
        assert!(!looks_poisoned("is it up", &results));
    }

    #[test]
    fn poison_detection_matches_case_insensitively_and_inside_urls() {
        let results: Vec<SearchResult> = (0..4)
            .map(|n| SearchResult {
                title: "Untitled".into(),
                url: format!("https://docs.rs/RATATUI/{n}"),
                snippet: String::new(),
                published: None,
            })
            .collect();
        assert!(!looks_poisoned("ratatui crate docs", &results));
    }

    // ---- market selection --------------------------------------------------

    #[test]
    fn the_default_market_is_used_when_nothing_is_configured() {
        assert_eq!(markets_to_try(None, None), vec![DEFAULT_MARKET.to_string()]);
    }

    #[test]
    fn a_configured_market_wins_and_the_locale_becomes_the_retry() {
        assert_eq!(
            markets_to_try(Some("en-GB"), Some("pt_BR.UTF-8")),
            vec!["en-GB".to_string(), "pt-BR".to_string()]
        );
    }

    /// One request, not two, when the locale adds nothing.
    #[test]
    fn a_locale_matching_the_primary_market_is_not_retried() {
        assert_eq!(
            markets_to_try(None, Some("en_US.UTF-8")),
            vec!["en-US".to_string()]
        );
        assert_eq!(
            markets_to_try(Some("  "), Some("en_US.UTF-8")),
            vec!["en-US".to_string()]
        );
    }

    #[test]
    fn parses_market_tags_out_of_posix_locales() {
        assert_eq!(market_from_locale("pt_BR.UTF-8").as_deref(), Some("pt-BR"));
        assert_eq!(market_from_locale("de_DE@euro").as_deref(), Some("de-DE"));
        assert_eq!(market_from_locale("en-GB").as_deref(), Some("en-GB"));
        // No region means no market — guessing one would recreate the exact
        // mismatch that poisons the response.
        assert_eq!(market_from_locale("C"), None);
        assert_eq!(market_from_locale("POSIX"), None);
        assert_eq!(market_from_locale("en"), None);
        assert_eq!(market_from_locale(""), None);
    }
}
