//! Recovering a tool call a model wrote as plain-text JSON.

use super::*;

// --- tolerant tool-name resolution -------------------------------------

/// Case, hyphens and spacing are presentation, not identity.
#[test]
fn tool_names_normalise_across_case_and_separators() {
    let known = defs(&["web_search"]);
    for written in [
        "Web-Search",
        "WEB_SEARCH",
        "web search",
        "webSearch",
        "WebSearch",
        " web_search\n",
    ] {
        let resolved = resolve_tool_name(written, &known);
        assert_eq!(
            resolved.map(|d| d.name.as_str()),
            Some("web_search"),
            "{written} should resolve"
        );
    }
}

/// Normalisation erases separators, not letters: a name that is merely
/// *similar* stays unresolved.
#[test]
fn a_merely_similar_name_is_not_accepted() {
    let known = defs(&["web_search"]);
    for written in ["websearch", "web_serch", "websearcher", "search_web"] {
        assert!(
            resolve_tool_name(written, &known).is_none(),
            "{written} should not resolve"
        );
    }
}

/// The observed failure: the model wrote the bare verb.
#[test]
fn an_unambiguous_bare_verb_resolves_to_the_one_tool_that_matches() {
    let known = defs(&["web_search", "read_file", "run_bash"]);
    assert_eq!(
        resolve_tool_name("search", &known).map(|d| d.name.as_str()),
        Some("web_search")
    );
    assert_eq!(
        resolve_tool_name("read", &known).map(|d| d.name.as_str()),
        Some("read_file")
    );
}

/// The safety property: two plausible tools must fail, not be chosen
/// between. `write` is genuinely ambiguous, and guessing is how the wrong
/// side effect happens.
#[test]
fn an_ambiguous_fragment_resolves_to_nothing() {
    let known = defs(&["write_file", "write_tasks"]);
    assert!(resolve_tool_name("write", &known).is_none());
}

/// Fragments have to be anchored on a `_` boundary, or `run_bash` becomes
/// reachable from any string that happens to share letters with it.
#[test]
fn a_fragment_that_is_not_a_whole_segment_never_matches() {
    let known = defs(&["web_search", "run_bash"]);
    assert!(resolve_tool_name("earch", &known).is_none());
    assert!(resolve_tool_name("_bas", &known).is_none());
    // Three characters carry too little signal to dispatch on.
    assert!(resolve_tool_name("run", &known).is_none());
}

#[test]
fn an_unrelated_name_still_resolves_to_nothing() {
    let known = defs(&["web_search", "run_bash", "read_file"]);
    for written in ["delete_everything", "shell", "browse", ""] {
        assert!(
            resolve_tool_name(written, &known).is_none(),
            "{written} should not resolve"
        );
    }
}

/// End of the road for the observed session: `"action": "search"` with
/// `web_search` registered is a real call.
#[test]
fn the_action_envelope_accepts_a_tolerated_name() {
    let known = defs(&["web_search", "run_bash"]);
    let text = r#"{"action": "search", "query": "guerra na ucrânia"}"#;
    let (name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(name, "web_search");
    assert_eq!(args["query"], "guerra na ucrânia");
}

#[test]
fn an_ambiguous_action_dispatches_nothing() {
    let known = defs(&["write_file", "write_tasks"]);
    let text = r#"{"action": "write", "path": "a.txt"}"#;
    assert!(find_fallback_tool_call(text, &known).is_none());
}

// --- argument alignment ------------------------------------------------

/// The rest of the observed envelope: the model invented `max_results` and
/// `region`. The first is unambiguously the schema's `num_results`; the
/// second matches nothing and is passed through for the tool itself to
/// ignore or reject. Neither is dropped here.
#[test]
fn unknown_argument_keys_are_aliased_when_unambiguous_and_kept_otherwise() {
    let known = vec![def_with_properties(
        "web_search",
        serde_json::json!({"query": {}, "num_results": {}}),
    )];
    let text = r#"{"action": "search", "query": "x", "region": "pt-br", "max_results": 10}"#;
    let (name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(name, "web_search");
    assert_eq!(
        args,
        serde_json::json!({"query": "x", "region": "pt-br", "num_results": 10})
    );
}

/// An alias must never displace a value the model also supplied correctly.
#[test]
fn an_alias_never_overwrites_a_correctly_named_argument() {
    let known = vec![def_with_properties(
        "web_search",
        serde_json::json!({"query": {}, "num_results": {}}),
    )];
    let text = r#"{"action": "web_search", "query": "x", "num_results": 3, "max_results": 9}"#;
    let (_name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(args["num_results"], 3);
    assert_eq!(args["max_results"], 9);
}

/// Two candidates for the same rename is the same ambiguity as two
/// candidate tools, and gets the same answer: leave it alone.
#[test]
fn an_ambiguous_alias_leaves_the_key_as_written() {
    let known = vec![def_with_properties(
        "edit_file",
        serde_json::json!({"old_text": {}, "new_text": {}}),
    )];
    let text = r#"{"action": "edit_file", "text": "hello"}"#;
    let (_name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(args, serde_json::json!({"text": "hello"}));
}

/// Case and separators on argument names get the same treatment as tool
/// names.
#[test]
fn argument_names_normalise_across_case_and_separators() {
    let known = vec![def_with_properties(
        "write_file",
        serde_json::json!({"path": {}, "content": {}}),
    )];
    let text = r#"{"name": "write_file", "arguments": {"Path": "a.txt", "CONTENT": "hi"}}"#;
    let (_name, args, _before, _after) = find_fallback_tool_call(text, &known).unwrap();
    assert_eq!(args, serde_json::json!({"path": "a.txt", "content": "hi"}));
}

/// End to end through a real turn: the model replies with nothing but the
/// JSON envelope, and the search actually runs instead of being left on
/// screen as dead text.
#[tokio::test]
async fn action_envelope_is_recovered_and_actually_executed() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(ScriptedProvider::streams([
        text_reply(r#"{"action": "web_search", "query": "rust 2024 edition"}"#),
        text_reply("Here's what I found."),
    ]));
    let tools = Arc::new(RecordingToolsNamed::new(
        defs(&["web_search"]),
        executed.clone(),
    ));
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
        .with_permission_policy(PermissionPolicy::Skip);

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    agent
        .run_turn(
            "who won yesterday?".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(
        executed.load(std::sync::atomic::Ordering::SeqCst),
        "the JSON action envelope should have dispatched a real search"
    );

    let mut dispatched_query = None;
    while let Ok(event) = events_rx.try_recv() {
        if let AgentEvent::ToolCallStarted {
            tool_name, input, ..
        } = event
        {
            assert_eq!(tool_name, "web_search");
            dispatched_query = input
                .get("query")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        }
    }
    assert_eq!(dispatched_query.as_deref(), Some("rust 2024 edition"));

    // And the results come back into context as a tool result, which is
    // what the model then synthesises its answer from.
    assert_eq!(
        agent.history().last().unwrap().text(),
        "Here's what I found."
    );
}

/// The tool call arrives as plain text content (no `ToolUseStart` at all)
/// — a local model that doesn't use the structured tool-calling channel,
/// e.g. Ollama serving a model with weak function-calling.
#[tokio::test]
async fn text_only_tool_call_is_recovered_and_actually_executed() {
    let executed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = Arc::new(ScriptedProvider::streams([
        text_reply(r#"{"name": "write_file", "arguments": {"path": "a.txt"}}"#),
        text_reply("done"),
    ]));
    let tools = Arc::new(RecordingToolsNamed::new(
        defs(&["write_file"]),
        executed.clone(),
    ));
    let tool_ctx = ToolContext::new(".", "test-session");
    let mut agent = Agent::new(provider, tools, "fake-model".to_string(), tool_ctx)
        .with_permission_policy(PermissionPolicy::Skip);

    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (permission_tx, _permission_rx) = mpsc::unbounded_channel();
    let (question_tx, _question_rx) = mpsc::unbounded_channel();

    agent
        .run_turn(
            "make a file".to_string(),
            events_tx,
            permission_tx,
            question_tx,
            CancellationToken::new(),
        )
        .await;

    assert!(
        executed.load(std::sync::atomic::Ordering::SeqCst),
        "the text-only tool call should have run for real, not been left as dead text"
    );

    let mut saw_tool_call_started = false;
    while let Ok(event) = events_rx.try_recv() {
        if let AgentEvent::ToolCallStarted { tool_name, .. } = event {
            assert_eq!(tool_name, "write_file");
            saw_tool_call_started = true;
        }
    }
    assert!(saw_tool_call_started);
}
