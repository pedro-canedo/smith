//! Google News as a `web_search` backend, over its RSS search endpoint.
//!
//! Why it earns a tier: it is the only *keyless* backend measured to carry a
//! real publication date per item — Bing's RSS `<pubDate>` is a crawl
//! timestamp and is deliberately dropped — and it answered 100 relevant
//! Portuguese items on the exact query the Bing tiers fumbled, with no
//! anti-bot layer in the way. It is a **news** index, though: for a library
//! or an error message it has nothing, so it sits behind the Bing tiers as a
//! recency-flavoured fallback rather than in front of them.
//!
//! The one wart to know about: item `<link>`s are `news.google.com/rss/
//! articles/...` redirect URLs, not the publisher's page. The title carries
//! the publisher name ("Headline - ge") and the date is real, which answers
//! most news questions from the result list alone; a `web_fetch` of the link
//! follows the redirect.

use crate::web_search::{decode_html_entities, SearchResult};

const SEARCH_ENDPOINT: &str = "https://news.google.com/rss/search";

/// `hl`/`gl`/`ceid` when the query's language is not detected: Google requires
/// them, and `en-US` matches the default Bing market.
const DEFAULT_PARAMS: (&str, &str, &str) = ("en-US", "US", "US:en");

/// The RSS search URL for `query`, in the language the query itself appears
/// to be written in.
pub(crate) fn search_url(query: &str) -> Result<String, String> {
    let (hl, gl, ceid) = crate::language::detect(query)
        .map(|l| l.google_news_params())
        .unwrap_or(DEFAULT_PARAMS);
    url::Url::parse_with_params(
        SEARCH_ENDPOINT,
        &[("q", query), ("hl", hl), ("gl", gl), ("ceid", ceid)],
    )
    .map(String::from)
    .map_err(|e| format!("could not build a Google News URL: {e}"))
}

/// Pulls result rows out of the feed. Same scanning approach as
/// [`crate::bing::parse_rss`], and deliberately not shared with it: this feed
/// has a `<pubDate>` worth keeping and a `<source>` tag Bing's lacks, so the
/// two would only share the parts that are trivial anyway.
pub(crate) fn parse_rss(xml: &str, limit: usize) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut rest = xml;

    while results.len() < limit {
        let Some(start) = rest.find("<item>") else {
            break;
        };
        let body = &rest[start + "<item>".len()..];
        let end = body.find("</item>").unwrap_or(body.len());
        let item = &body[..end];
        rest = &body[end..];

        let (Some(title), Some(url)) = (element(item, "title"), element(item, "link")) else {
            continue;
        };
        if title.is_empty() || !url.starts_with("http") {
            continue;
        }
        // The description is an HTML anchor list restating the title, so the
        // publisher name is the more informative snippet.
        let snippet = element(item, "source")
            .map(|s| format!("via {s}"))
            .unwrap_or_default();
        results.push(SearchResult {
            title,
            url,
            snippet,
            published: element(item, "pubDate").and_then(|d| iso_date(&d)),
        });
    }
    results
}

/// An RFC 2822 date ("Tue, 04 Aug 2026 16:21:16 GMT") as `YYYY-MM-DD`, the
/// shape `format_results` prints. `None` for anything unparseable — a missing
/// date is better than a wrong one.
fn iso_date(rfc2822: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc2822(rfc2822.trim())
        .ok()
        .map(|d| d.format("%Y-%m-%d").to_string())
}

/// The decoded text of the first `<name ...>…</name>` in `item`. Unlike the
/// Bing feed, `<source>` here carries an attribute, so the opening tag is
/// matched up to its `>` rather than as a literal.
fn element(item: &str, name: &str) -> Option<String> {
    let open_at = item.find(&format!("<{name}"))?;
    let after_tag = &item[open_at..];
    let content_start = open_at + after_tag.find('>')? + 1;
    let close = format!("</{name}>");
    let content_end = item[content_start..].find(&close)? + content_start;
    let raw = item[content_start..content_end].trim();
    let raw = raw
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(raw);
    Some(decode_html_entities(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, link: &str, pub_date: &str, source: &str) -> String {
        format!(
            "<item><title>{title}</title><link>{link}</link>\
             <guid isPermaLink=\"false\">x</guid><pubDate>{pub_date}</pubDate>\
             <description>&lt;a href=\"{link}\"&gt;{title}&lt;/a&gt;</description>\
             <source url=\"https://example.com\">{source}</source></item>"
        )
    }

    #[test]
    fn the_url_follows_the_querys_language() {
        let pt = search_url("resultado da mega sena").unwrap();
        assert!(pt.contains("hl=pt-BR"), "got: {pt}");
        assert!(pt.contains("gl=BR"), "got: {pt}");
        assert!(pt.contains("ceid=BR%3Apt-419"), "got: {pt}");

        let en = search_url("kubernetes release notes").unwrap();
        assert!(en.contains("hl=en-US"), "got: {en}");
        assert!(en.contains("ceid=US%3Aen"), "got: {en}");
    }

    #[test]
    fn parses_title_link_real_date_and_source() {
        let xml = item(
            "Flamengo vence o Palmeiras - ge",
            "https://news.google.com/rss/articles/CBMi",
            "Tue, 04 Aug 2026 16:21:16 GMT",
            "ge",
        );
        let results = parse_rss(&xml, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Flamengo vence o Palmeiras - ge");
        assert_eq!(results[0].published.as_deref(), Some("2026-08-04"));
        assert_eq!(results[0].snippet, "via ge");
    }

    #[test]
    fn an_unparseable_date_becomes_none_not_garbage() {
        let xml = item("T", "https://e.com/", "not a date", "s");
        assert_eq!(parse_rss(&xml, 5)[0].published, None);
    }

    #[test]
    fn respects_the_limit_and_skips_broken_items() {
        let items: String = (1..=4)
            .map(|n| {
                item(
                    &format!("Title {n}"),
                    &format!("https://e.com/{n}"),
                    "Tue, 04 Aug 2026 16:21:16 GMT",
                    "src",
                )
            })
            .chain(["<item><title>No link</title></item>".to_string()])
            .collect();
        let results = parse_rss(&items, 3);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].title, "Title 1");
    }

    /// Truncation anywhere must not panic — same contract as the Bing parser.
    #[test]
    fn malformed_or_truncated_markup_stops_cleanly() {
        let full = item("T", "https://e.com/", "Tue, 04 Aug 2026 16:21:16 GMT", "s");
        for cut in 0..full.len() {
            let _ = parse_rss(&full[..cut], 3);
        }
        assert!(parse_rss("", 3).is_empty());
    }
}
