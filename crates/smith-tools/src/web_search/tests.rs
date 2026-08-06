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

// ---- pinned backend ------------------------------------------------------

/// The pin contract: pinned-but-unconfigured fails naming exactly the
/// missing key, and no other tier is attempted — the failure detail
/// carries one backend, not the whole chain.
#[tokio::test]
async fn a_pinned_backend_never_falls_back_and_names_the_missing_config() {
    let tool = WebSearchTool::with_settings(SearchSettings {
        backend: Some("tavily".into()),
        ..SearchSettings::default()
    });
    let ctx = smith_core::ToolContext::new(std::env::temp_dir(), "test");
    let result = tool
        .execute(
            serde_json::json!({"query": "anything"}),
            &ctx,
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_error);
    assert!(
        result.content.contains("[tavily] api_key"),
        "{}",
        result.content
    );
    assert!(result.content.contains("- Tavily:"), "{}", result.content);
    // Single-entry detail — Bing/DuckDuckGo were never consulted.
    assert!(!result.content.contains("- Bing:"), "{}", result.content);
    assert!(
        !result.content.contains("- DuckDuckGo:"),
        "{}",
        result.content
    );
}

#[tokio::test]
async fn an_unknown_pin_is_a_config_error_listing_the_valid_names() {
    let tool = WebSearchTool::with_settings(SearchSettings {
        backend: Some("frobnicate".into()),
        ..SearchSettings::default()
    });
    let ctx = smith_core::ToolContext::new(std::env::temp_dir(), "test");
    let result = tool
        .execute(
            serde_json::json!({"query": "anything"}),
            &ctx,
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_error);
    assert!(result.content.contains("frobnicate"), "{}", result.content);
    assert!(
        result.content.contains("valid values"),
        "{}",
        result.content
    );
}

/// A pinned SearXNG with no URL is the privacy user's likeliest mistake —
/// it must name the missing setting, not quietly search elsewhere.
#[tokio::test]
async fn a_searxng_pin_without_a_url_names_the_missing_setting() {
    let tool = WebSearchTool::with_settings(SearchSettings {
        backend: Some("SearXNG".into()), // case-insensitive on purpose
        ..SearchSettings::default()
    });
    let ctx = smith_core::ToolContext::new(std::env::temp_dir(), "test");
    let result = tool
        .execute(
            serde_json::json!({"query": "anything"}),
            &ctx,
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_error);
    assert!(result.content.contains("searxng_url"), "{}", result.content);
    assert!(!result.content.contains("- Bing:"), "{}", result.content);
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
    let (results, relevance) = classify_bing("resultado da mega sena", &xml, 3, "en-US").unwrap();
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
