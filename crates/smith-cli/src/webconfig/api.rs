//! The five JSON handlers behind the config page.
//!
//! One rule shapes all of them: **a secret goes browser → disk and never
//! back**. `state` reports whether a key is set, never any part of its value,
//! and there is no endpoint that could produce one — not a suffix, not a
//! length, not a hash. That is the whole contract, and it is enforced by
//! there being no code that reads a key into a response rather than by a
//! filter that could be forgotten.
//!
//! The second rule is about what may be written at all. `[hooks]` and
//! `[[mcp_servers]]` are refused permanently: hooks are shell commands that
//! run on every tool call, and `smith-config`'s own merge already refuses to
//! take them from the project layer for that reason. An HTML form that could
//! write them onto a loopback port would be the worst endpoint in this design.

use serde_json::{json, Value};
use smith_config::Config;

/// Routes to a handler and renders `(status, json)`.
pub async fn dispatch(req: super::request::Request) -> (u16, String) {
    use super::request::Route;

    let result = match req.route {
        Route::State => state().await,
        Route::Models => models(req.query.get("provider").map(String::as_str)).await,
        Route::Config => write_config(&req.body),
        Route::Test => test(&req.body).await,
        // Handled by the server loop, which has to shut down rather than
        // reply — it never reaches here.
        Route::Close | Route::Page => Ok(json!({ "ok": true })),
    };

    match result {
        Ok(value) => (200, value.to_string()),
        Err(message) => (200, json!({ "ok": false, "error": message }).to_string()),
    }
}

/// What the page renders itself from. Redacted by construction.
async fn state() -> Result<Value, String> {
    let config = Config::load().unwrap_or_default();

    // `key_set` is the entire contract. There is deliberately no branch here
    // that could return part of a value.
    let key = |value: Option<&String>, env: &str| {
        let from_env = std::env::var(env).is_ok_and(|v| !v.trim().is_empty());
        let from_config = value.is_some_and(|v| !v.trim().is_empty());
        json!({
            "key_set": from_env || from_config,
            // Which one wins matters to the user: `build_provider` prefers the
            // environment, so a saved key that is being shadowed is worth
            // saying out loud rather than letting them edit it in vain.
            "source": if from_env { "environment" } else if from_config { "config" } else { "none" },
        })
    };

    Ok(json!({
        "ok": true,
        "provider": config.general.provider,
        "model": config.general.model,
        "permission_policy": config.general.permission_policy,
        "providers": {
            "anthropic": key(config.anthropic.api_key.as_ref(), "ANTHROPIC_API_KEY"),
            "openai": key(config.openai.api_key.as_ref(), "OPENAI_API_KEY"),
            "openrouter": key(config.openrouter.api_key.as_ref(), "OPENROUTER_API_KEY"),
            "9router": key(config.nine_router.api_key.as_ref(), "NINEROUTER_API_KEY"),
            "ollama": json!({ "key_set": true, "source": "not needed" }),
        },
        "search": {
            "backend": config.search.backend,
            "searxng_url": config.search.searxng_url,
            "tavily": key(config.tavily.api_key.as_ref(), "TAVILY_API_KEY"),
            "exa": key(config.exa.api_key.as_ref(), "EXA_API_KEY"),
        },
        "config_path": smith_config::config_path().ok().map(|p| p.display().to_string()),
    }))
}

/// Live catalogue for one provider, so the page offers what exists rather
/// than a list that rots in the binary.
async fn models(provider: Option<&str>) -> Result<Value, String> {
    let config = Config::load().unwrap_or_default();
    let provider = provider.ok_or("no provider named")?;

    let models: Vec<Value> = match provider {
        "ollama" => {
            let base = config
                .ollama
                .base_url
                .unwrap_or_else(|| smith_config::DEFAULT_OLLAMA_BASE_URL.to_string());
            smith_provider::ollama_tags(&base)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|m| {
                    json!({
                        "id": m.name,
                        "detail": m.summary(),
                        "cloud": m.is_cloud,
                        "tools": m.supports_tools,
                    })
                })
                .collect()
        }
        "9router" => {
            let base = config
                .nine_router
                .base_url
                .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());
            crate::node_runtime::ninerouter_upstreams(&base)
                .await?
                .into_iter()
                .map(|id| json!({ "id": id, "detail": id, "cloud": true, "tools": true }))
                .collect()
        }
        // The rest have no probe worth making from here: a catalogue call
        // needs the key, and offering the curated list costs nothing.
        other => smith_store::models::known_models(other)
            .iter()
            .map(|id| json!({ "id": id, "detail": id, "cloud": false, "tools": true }))
            .collect(),
    };

    Ok(json!({ "ok": true, "provider": provider, "models": models }))
}

/// A partial write.
///
/// The semantics are `setup`'s `apply_optional`, as a JSON contract, so the
/// two frontends cannot disagree about what a blank field means:
///
/// - field absent: unchanged
/// - `""`: unchanged. A form submits empty boxes, and that is not a request
///   to delete a key.
/// - `null`: cleared
/// - a string: replaced
fn write_config(body: &str) -> Result<Value, String> {
    let incoming: Value = serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
    let object = incoming.as_object().ok_or("expected a JSON object")?;

    // Refused permanently, not merely unimplemented. Hooks are shell commands
    // that run on every tool call.
    for forbidden in ["hooks", "mcp_servers"] {
        if object.contains_key(forbidden) {
            return Err(format!(
                "`{forbidden}` cannot be set from the browser — it runs commands, \
                 so it is edited in ~/.smith/config.toml by hand, on purpose"
            ));
        }
    }

    let mut config = Config::load().unwrap_or_default();
    let mut changed: Vec<String> = Vec::new();

    let mut apply = |field: &str, slot: &mut Option<String>| {
        let Some(value) = object.get(field) else {
            return;
        };
        match value {
            Value::Null => {
                *slot = None;
                changed.push(field.to_string());
            }
            Value::String(s) if s.trim().is_empty() => {}
            Value::String(s) => {
                *slot = Some(s.trim().to_string());
                changed.push(field.to_string());
            }
            _ => {}
        }
    };

    apply("provider", &mut config.general.provider);
    apply("model", &mut config.general.model);
    apply("permission_policy", &mut config.general.permission_policy);
    apply("anthropic_api_key", &mut config.anthropic.api_key);
    apply("openai_api_key", &mut config.openai.api_key);
    apply("openrouter_api_key", &mut config.openrouter.api_key);
    apply("ninerouter_api_key", &mut config.nine_router.api_key);
    apply("ninerouter_model", &mut config.nine_router.model);
    apply("ollama_model", &mut config.ollama.model);
    apply("search_backend", &mut config.search.backend);
    apply("searxng_url", &mut config.search.searxng_url);
    apply("tavily_api_key", &mut config.tavily.api_key);
    apply("exa_api_key", &mut config.exa.api_key);

    // A provider smith cannot parse would leave the CLI in the dead end the
    // first-run check exists to prevent, so it is refused here rather than
    // discovered at the next start.
    if let Some(name) = config.general.provider.as_deref() {
        if !smith_store::models::is_known_provider(name) {
            return Err(format!("`{name}` is not a provider smith knows"));
        }
    }

    config.save().map_err(|e| e.to_string())?;

    // The terminal stays the audit log: the browser can write, but what it
    // wrote is echoed where the user launched it from. Field *names* only —
    // printing a value here would put a key in a scrollback buffer.
    for field in &changed {
        println!("  saved: {field}");
    }

    Ok(json!({ "ok": true, "saved": changed }))
}

/// One probe against a provider, reported as the user would see it.
async fn test(body: &str) -> Result<Value, String> {
    let incoming: Value = serde_json::from_str(body).map_err(|e| format!("bad JSON: {e}"))?;
    let provider = incoming
        .get("provider")
        .and_then(Value::as_str)
        .ok_or("no provider named")?;

    let config = Config::load().unwrap_or_default();
    let kind = crate::orchestrator::ProviderKind::from_config_str(provider)
        .ok_or_else(|| format!("`{provider}` is not a provider smith knows"))?;

    // Building is the test. It resolves the key the same way a real run does,
    // so "it built" means "a turn would have got as far as the network".
    match crate::orchestrator::build_provider(kind, &config) {
        Ok(_) => Ok(json!({ "ok": true, "detail": "configured and ready" })),
        Err(e) => Ok(json!({ "ok": false, "error": e })),
    }
}
