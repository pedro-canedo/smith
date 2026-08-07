use super::*;

/// Captured from a real daemon, trimmed to the fields this module reads. The
/// point of a fixture over a live call: the shape is pinned to something a
/// reviewer can see, not to whatever the developer happened to have pulled.
const TAGS: &str = include_str!("tags.json");

fn parsed() -> Vec<OllamaModel> {
    parse_ollama_tags(&serde_json::from_str(TAGS).unwrap())
}

fn by_name(name: &str) -> OllamaModel {
    parsed()
        .into_iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("{name} missing from the fixture"))
}

#[test]
fn the_catalogue_is_read_off_the_daemon_rather_than_guessed() {
    let models = parsed();
    assert_eq!(models.len(), 5, "{models:#?}");
    assert!(models.iter().any(|m| m.name == "qwen3.5:9b"));
}

/// The distinction the rest of the feature hangs off: a cloud model is proxied
/// by the daemon, so its advertised window is real and there are no weights to
/// pull.
#[test]
fn a_remote_host_marks_a_model_as_cloud() {
    assert!(by_name("nemotron-3-super:cloud").is_cloud);
    assert!(by_name("deepseek-v4-flash:cloud").is_cloud);
    assert!(!by_name("qwen3.5:9b").is_cloud);
    assert!(!by_name("qwen2.5-coder:14b").is_cloud);
}

/// Two signals on purpose. `remote_host` is the fact; the suffix is the
/// convention. Either alone reclassifies a cloud model as local the day the
/// other changes, and that decides whether smith trusts the window it was
/// told — the failure would be a silent truncation, not an error.
#[test]
fn either_signal_alone_still_identifies_a_cloud_model() {
    let suffix_only = serde_json::json!({
        "models": [{ "name": "mystery:cloud", "details": {} }]
    });
    assert!(parse_ollama_tags(&suffix_only)[0].is_cloud);

    let host_only = serde_json::json!({
        "models": [{ "name": "mystery", "remote_host": "https://ollama.com", "details": {} }]
    });
    assert!(parse_ollama_tags(&host_only)[0].is_cloud);

    let neither = serde_json::json!({
        "models": [{ "name": "mystery", "remote_host": "", "details": {} }]
    });
    assert!(!parse_ollama_tags(&neither)[0].is_cloud);
}

#[test]
fn the_advertised_context_window_is_carried_through() {
    assert_eq!(
        by_name("nemotron-3-super:cloud").context_window,
        Some(262_144)
    );
    assert_eq!(
        by_name("deepseek-v4-flash:cloud").context_window,
        Some(1_048_576)
    );
    assert_eq!(by_name("qwen2.5-coder:14b").context_window, Some(32_768));
}

/// A zero is the daemon declining to say, not a model with no context.
#[test]
fn a_zero_window_reads_as_unknown_rather_than_as_zero() {
    let body = serde_json::json!({
        "models": [{ "name": "x", "details": { "context_length": 0 } }]
    });
    assert_eq!(parse_ollama_tags(&body)[0].context_window, None);
}

/// An agent without tools is a chatbot. Better to say so on the row than to
/// let the user pick it and meet the failure a turn later.
#[test]
fn tool_support_is_read_from_the_capability_list() {
    assert!(by_name("nemotron-3-super:cloud").supports_tools);
    assert!(by_name("qwen3.5:9b").supports_tools);

    let toolless = serde_json::json!({
        "models": [{ "name": "embed-only", "capabilities": ["completion"], "details": {} }]
    });
    let model = &parse_ollama_tags(&toolless)[0];
    assert!(!model.supports_tools);
    assert!(model.summary().contains("NO TOOLS"), "{}", model.summary());
}

/// Two spellings, both real, and reading only one classifies half the
/// catalogue as local — which is what decides whether smith tries to download
/// weights that do not exist.
#[test]
fn both_spellings_of_a_cloud_name_are_recognised() {
    assert!(is_cloud_name("nemotron-3-super:cloud"));
    assert!(is_cloud_name("gpt-oss:120b-cloud"));
    assert!(is_cloud_name("minimax-m3:cloud"));

    assert!(!is_cloud_name("qwen3.5:9b"));
    assert!(!is_cloud_name("llama3.3"));
    // A name that merely mentions cloud is not one.
    assert!(!is_cloud_name("cloudy-llama"));
    assert!(!is_cloud_name("cloud"));
}

#[test]
fn a_summary_says_what_the_row_needs_to_decide() {
    let cloud = by_name("nemotron-3-super:cloud").summary();
    assert!(cloud.contains("cloud"), "{cloud}");
    assert!(cloud.contains("262k ctx"), "{cloud}");

    let local = by_name("qwen2.5-coder:14b").summary();
    assert!(local.contains("GB"), "{local}");
    assert!(!local.contains("cloud"), "{local}");
}

#[test]
fn compact_tokens_reads_the_way_the_gauge_does() {
    assert_eq!(compact_tokens(512), "512");
    assert_eq!(compact_tokens(32_768), "32k");
    assert_eq!(compact_tokens(262_144), "262k");
    assert_eq!(compact_tokens(1_048_576), "1.0M");
}

/// A catalogue smith cannot read is one to fall back from, not a reason to
/// fail a wizard that has a static list to offer.
#[test]
fn an_unreadable_body_yields_nothing_rather_than_an_error() {
    assert!(parse_ollama_tags(&serde_json::json!({})).is_empty());
    assert!(parse_ollama_tags(&serde_json::json!([1, 2, 3])).is_empty());
    assert!(parse_ollama_tags(&serde_json::json!({"models": "nope"})).is_empty());
    let nameless = serde_json::json!({"models": [{"size": 1}, {"name": "  "}]});
    assert!(parse_ollama_tags(&nameless).is_empty());
}

// ---- the errors that arrive with a success status --------------------------

/// Verbatim from a real daemon on 2026-08-06. These strings are the contract,
/// and it is a contract nobody promised us — when it changes, it changes here.
const SUBSCRIPTION_BODY: &str = r#"{"error":{"message":"this model requires a subscription, upgrade for access: https://ollama.com/upgrade (ref: 01K9)","type":"api_error"}}"#;
const UNAUTHORIZED_BODY: &str = r#"{"error":"Unauthorized"}"#;
/// A third shape, found while measuring which cloud models are free: some ask
/// for a plan *and* for extra usage on top, and the sentence never says
/// "subscription". It is caught by the `upgrade for access` half of the match,
/// which is the reason that half exists.
const PLAN_AND_USAGE_BODY: &str = r#"{"error":{"message":"this model requires both a Pro, Max, or Team plan and extra usage (it does not use included plan usage), upgrade for access: https://ollama.com/upgrade then add extra usage: https://ollama.com/settings (ref: 53e9)","type":"api_error"}}"#;

#[test]
fn a_plan_plus_usage_refusal_is_still_an_entitlement_failure() {
    let body: serde_json::Value = serde_json::from_str(PLAN_AND_USAGE_BODY).unwrap();
    let message = error_in_success_body(&body).expect("an error hides in here");
    assert!(
        !message.to_ascii_lowercase().contains("subscription"),
        "if this ever says `subscription` the test stops proving anything"
    );
    let err = classify_ollama_error(message);
    let ProviderError::Api { status, .. } = err else {
        panic!("an entitlement failure is an API error");
    };
    assert_eq!(status, 402, "the chain has to move past it, not retry it");
}

#[test]
fn a_subscription_refusal_names_a_free_model_and_the_upgrade_page() {
    let body: serde_json::Value = serde_json::from_str(SUBSCRIPTION_BODY).unwrap();
    let message = error_in_success_body(&body).expect("an error hides in this 200");
    let err = classify_ollama_error(message);
    let ProviderError::Api {
        status,
        ref message,
        ..
    } = err
    else {
        panic!("a subscription refusal is an API error");
    };
    assert!(message.contains("nemotron-3-super:cloud"), "{message}");
    assert!(message.contains("https://ollama.com/upgrade"), "{message}");
    assert!(message.contains("local"), "{message}");
    // 402 is load-bearing twice: `retryable()` is false, so the turn does not
    // back off over a billing fact, and `FallbackProvider` reads 402 as a
    // quota death and moves the chain past this model.
    assert_eq!(status, 402);
    assert!(!err.retryable(), "waiting does not buy a subscription");
}

#[test]
fn a_signed_out_daemon_is_told_to_sign_in_and_that_it_is_free() {
    let body: serde_json::Value = serde_json::from_str(UNAUTHORIZED_BODY).unwrap();
    let message = error_in_success_body(&body).expect("an error hides in this 200");
    let err = classify_ollama_error(message);
    let ProviderError::Api {
        status,
        ref message,
        ..
    } = err
    else {
        panic!("a refusal is an API error");
    };
    assert!(message.contains("ollama signin"), "{message}");
    assert!(message.contains("no card"), "{message}");
    assert_eq!(status, 401);
    assert!(!err.retryable(), "retrying a signed-out daemon is a loop");
}

/// Both shapes are in the wild: the bare string from the daemon, the nested
/// object proxied from upstream.
#[test]
fn both_error_shapes_are_recognised_and_a_clean_body_is_not() {
    let nested = serde_json::json!({"error": {"message": "boom"}});
    assert_eq!(error_in_success_body(&nested), Some("boom"));
    let bare = serde_json::json!({"error": "boom"});
    assert_eq!(error_in_success_body(&bare), Some("boom"));

    let fine = serde_json::json!({"choices": [{"message": {"content": "hi"}}]});
    assert_eq!(error_in_success_body(&fine), None);
    assert_eq!(error_in_success_body(&serde_json::json!({})), None);
}

/// The wiring, at the shape the wire actually uses. A streaming turn gets a
/// 403 whose *body* is the JSON above, so `translate_error` has to dig the
/// message out of `api_error`'s text before it can say anything useful.
#[test]
fn a_streaming_refusal_is_translated_through_the_error_body() {
    // What `api_error` builds from a 403: the whole response text as message.
    let raw = crate::ProviderError::Api {
        status: 403,
        message: SUBSCRIPTION_BODY.to_string(),
        retry_after: None,
    };
    let provider = crate::OpenAiProvider::ollama("http://127.0.0.1:11434/v1".to_string());
    let crate::ProviderError::Api {
        status, message, ..
    } = provider.translate_error(raw)
    else {
        panic!("still an API error");
    };
    assert_eq!(
        status, 402,
        "403 is re-mapped so the chain can move past it"
    );
    assert!(message.contains("https://ollama.com/upgrade"), "{message}");
    assert!(!message.contains("api_error"), "raw JSON leaked: {message}");
}

/// Only Ollama is a proxy, so only Ollama has someone else's failure to
/// explain. Dressing up an error smith did not understand is how a message
/// stops being true.
#[test]
fn another_flavour_error_is_never_rewritten() {
    let raw = crate::ProviderError::Api {
        status: 403,
        message: SUBSCRIPTION_BODY.to_string(),
        retry_after: None,
    };
    let provider = crate::OpenAiProvider::openrouter("k".to_string(), "http://x/v1".to_string());
    let crate::ProviderError::Api {
        status, message, ..
    } = provider.translate_error(raw)
    else {
        panic!("still an API error");
    };
    assert_eq!(status, 403);
    assert_eq!(message, SUBSCRIPTION_BODY);
}

/// A body that is not JSON, or JSON with no error in it, is left alone.
#[test]
fn a_body_with_nothing_to_translate_passes_through() {
    let provider = crate::OpenAiProvider::ollama("http://127.0.0.1:11434/v1".to_string());
    for body in ["upstream exploded", r#"{"choices":[]}"#] {
        let raw = crate::ProviderError::Api {
            status: 500,
            message: body.to_string(),
            retry_after: None,
        };
        let crate::ProviderError::Api {
            status, message, ..
        } = provider.translate_error(raw)
        else {
            panic!("still an API error");
        };
        assert_eq!(status, 500);
        assert_eq!(message, body);
    }
}

/// Anything unrecognised is passed through rather than dressed up. A guess
/// about someone else's error message is worse than the message.
#[test]
fn an_unrecognised_message_survives_verbatim() {
    let ProviderError::Api { message, .. } = classify_ollama_error("model runner has crashed")
    else {
        panic!("still an API error");
    };
    assert_eq!(message, "model runner has crashed");
}
