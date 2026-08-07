//! The `smith setup` wizard.
//!
//! Not a questionnaire but a **menu of sections**, each owning one layer of
//! configuration (provider & model, web search, browser, permissions). The
//! main menu shows every section's current state inline, drilling in is one
//! keypress, and Esc backs out of any level without losing what other
//! sections already saved — the navigation model the linear wizard this
//! replaced could not offer (it asked its questions in one fixed order and a
//! wrong answer early meant starting over).
//!
//! Persistence is per-section, not per-run: a section that completes is saved
//! to disk immediately, so quitting from the menu (Esc or "Done") never
//! throws away finished work, and there is no "are you sure?" state to
//! manage. The only in-memory mutation is inside a section, and a section
//! that is backed out of mutates nothing.

use std::process::Stdio;
use std::time::Duration;

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Password, Select};
use smith_config::{Config, DEFAULT_OLLAMA_BASE_URL, OLLAMA_HOST};

use crate::runtime::{self, BrowserSource, HttpAssetSource};

/// Model lists live in `smith-store::models` — one source, shared with the
/// runtime `/model` command. This file used to say that about the OpenRouter
/// chain and then keep private copies of the other three, which is how the
/// wizard and `/model` came to disagree about what Anthropic offers.
use smith_store::models::{known_models, OPENROUTER_MODELS};

/// What a text prompt should do to an optional setting: the sentinel `-`
/// clears it, blank keeps it, anything else replaces it.
fn apply_optional(existing: Option<String>, entered: &str) -> Option<String> {
    match entered.trim() {
        "" => existing,
        "-" => None,
        other => Some(other.to_string()),
    }
}

/// `set` / `not set` for secrets — the value itself never echoes back.
fn key_status(key: Option<&str>) -> &'static str {
    match key {
        Some(k) if !k.trim().is_empty() => "set",
        _ => "not set",
    }
}

/// One line of state for the main menu's provider row.
fn provider_summary(config: &Config) -> String {
    match (&config.general.provider, &config.general.model) {
        (Some(p), Some(m)) => format!("{p} · {m}"),
        (Some(p), None) => p.clone(),
        _ => "not configured".to_string(),
    }
}

/// One line of state for the main menu's web-search row.
fn search_summary(config: &Config) -> String {
    if let Some(pin) = config.search.backend.as_deref().filter(|p| !p.is_empty()) {
        return format!("pinned: {pin}");
    }
    let mut parts: Vec<&str> = Vec::new();
    if config.search.searxng_url.is_some() {
        parts.push("searxng");
    }
    if key_status(config.tavily.api_key.as_deref()) == "set" {
        parts.push("tavily");
    }
    if key_status(config.exa.api_key.as_deref()) == "set" {
        parts.push("exa");
    }
    if parts.is_empty() {
        "free tiers only (works out of the box)".to_string()
    } else {
        parts.join(" + ")
    }
}

fn permission_summary(config: &Config) -> String {
    config
        .general
        .permission_policy
        .clone()
        .unwrap_or_else(|| "ask (default)".to_string())
}

/// Whether a browser could plausibly open on this machine.
///
/// Not a capability check — nothing here can prove a browser exists. It is a
/// check for a *display*, which is the thing whose absence makes the offer
/// useless: over a plain SSH session the link opens nowhere, and a menu row
/// that leads to a dead end is worse than one that is missing.
///
/// WSL counts even without `DISPLAY`, because the Windows host opens the link.
fn browser_plausible() -> bool {
    if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
        return true;
    }
    [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "WSL_DISTRO_NAME",
        "WSL_INTEROP",
    ]
    .iter()
    .any(|var| std::env::var_os(var).is_some())
}

fn browser_summary(config: &Config) -> String {
    match runtime::find_browser(&config.runtime) {
        Some(_) => "available".to_string(),
        None => "not installed".to_string(),
    }
}

/// Drives `smith setup` / `smith setup model`. `jump_to_model` skips straight
/// to model selection for the already-configured provider (falling back to
/// the full wizard if nothing is configured yet).
pub async fn run(jump_to_model: bool) -> color_eyre::Result<()> {
    let theme = ColorfulTheme::default();
    let mut config = Config::load().unwrap_or_default();

    if jump_to_model {
        match config.general.provider.clone() {
            Some(provider) => {
                // `known_models` answers for every provider id and returns
                // an empty slice for anything else, which `select_model`
                // already renders as "Other (type a model name)".
                let models = known_models(&provider);
                if let Some(model) = select_model(&theme, models)? {
                    config.general.model = Some(model);
                    save(&config)?;
                }
                return Ok(());
            }
            None => println!("No provider configured yet — let's set one up first.\n"),
        }
    }

    println!("smith setup — pick a section to configure. Esc backs out of any level;");
    println!("each section saves as soon as it completes.\n");

    // The browser row exists only where a browser plausibly does. Offering it
    // over SSH would send someone to a link nothing can open — and this menu
    // is the *only* way anyone finds the feature, since `smith setup web` is
    // a subcommand nobody guesses.
    let offer_web = browser_plausible();

    loop {
        let mut items = Vec::new();
        if offer_web {
            items.push("Configure in a browser   (a page on this machine)".to_string());
        }
        items.extend([
            format!("Provider & model   [{}]", provider_summary(&config)),
            format!("Web search         [{}]", search_summary(&config)),
            format!("Browser            [{}]", browser_summary(&config)),
            format!("Permissions        [{}]", permission_summary(&config)),
            "Done".to_string(),
        ]);
        let choice = Select::with_theme(&theme)
            .with_prompt("Section")
            .items(&items)
            .default(0)
            .interact_opt()?;

        // One offset rather than two index tables: the rows below shift by
        // exactly one when the browser row is present, and a second table is
        // how the labels and the arms drift apart.
        let Some(row) = choice else { break };
        let section = row as isize - isize::from(offer_web);

        let changed = match section {
            -1 => {
                crate::webconfig::run(false, None)
                    .await
                    .map_err(color_eyre::eyre::Error::msg)?;
                // The page writes the global config itself, so re-read rather
                // than keep a copy that is now stale.
                config = Config::load().unwrap_or_default();
                false
            }
            0 => section_provider(&theme, &mut config).await?,
            1 => section_search(&theme, &mut config)?,
            2 => section_browser(&theme, &mut config).await?,
            3 => section_permissions(&theme, &mut config)?,
            _ => break, // Done.
        };
        if changed {
            save(&config)?;
        }
    }

    println!("\nConfig: {}", smith_config::config_path()?.display());
    println!("Run `smith` to start chatting.");
    Ok(())
}

fn save(config: &Config) -> color_eyre::Result<()> {
    config.save()?;
    println!("  ✓ saved\n");
    Ok(())
}

// ---- section: provider & model ---------------------------------------------

/// `Ok(true)` when the section completed and `config` changed; `Ok(false)`
/// when the user backed out, in which case `config` is untouched.
async fn section_provider(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    let Some(provider) = select_provider(theme).await? else {
        return Ok(false);
    };
    match provider.as_str() {
        "openrouter" => setup_openrouter(theme, config).await,
        "9router" => setup_ninerouter(theme, config).await,
        "anthropic" => setup_api_provider(theme, config, "anthropic", known_models("anthropic")),
        "openai" => setup_api_provider(theme, config, "openai", known_models("openai")),
        "ollama" => setup_ollama(theme, config).await,
        other => color_eyre::eyre::bail!("unknown provider: {other}"),
    }
}

/// What a 500 ms probe found on this machine, which decides where the
/// provider cursor starts and what the Ollama row says about itself.
enum OllamaState {
    /// Daemon answering, with this many models already pulled or linked.
    Ready(usize),
    /// Binary installed, daemon down — `ensure_ollama_running` handles that.
    Installed,
    Absent,
}

impl OllamaState {
    async fn probe() -> Self {
        match ollama_model_count().await {
            Some(n) if n > 0 => Self::Ready(n),
            // Answering with nothing pulled is not "ready": picking it would
            // hand the user a model list with nothing in it.
            Some(_) => Self::Installed,
            None if ollama_binary_present() => Self::Installed,
            None => Self::Absent,
        }
    }

    fn row(&self) -> String {
        match self {
            Self::Ready(n) => {
                format!("Ollama (local + cloud) — {n} models ready, no key needed")
            }
            Self::Installed => {
                "Ollama (local + cloud) — daemon not running, smith will start it".to_string()
            }
            Self::Absent => "Ollama (local) — not installed".to_string(),
        }
    }

    /// Ollama is row 0, so it is the cursor whenever it is usable at all.
    fn cursor(&self) -> usize {
        match self {
            Self::Ready(_) | Self::Installed => 0,
            Self::Absent => 1,
        }
    }
}

/// How many models `/api/tags` reports, or `None` if the daemon did not answer.
async fn ollama_model_count() -> Option<usize> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .ok()?;
    let body: serde_json::Value = client
        .get(format!("{OLLAMA_HOST}/api/tags"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    Some(body.get("models")?.as_array()?.len())
}

async fn select_provider(theme: &ColorfulTheme) -> color_eyre::Result<Option<String>> {
    // Keyless first, then free-with-account. This used to be "free first", and
    // both OpenRouter and 9Router are free — but both need the user to go make
    // an account somewhere and carry a key back, and a first-time cursor should
    // land on the option that can answer a question without leaving the
    // terminal. Which one that is depends on the machine, so it is probed
    // rather than assumed: on a box with no Ollama the cursor moves on instead
    // of aiming the newcomer at a connection refused.
    let ollama = OllamaState::probe().await;
    let items = [
        ollama.row(),
        "Free — OpenRouter (cloud, free models, needs a free account)".to_string(),
        "Anthropic (Claude)".to_string(),
        "OpenAI".to_string(),
        "9Router (advanced — local gateway, needs its own dashboard setup)".to_string(),
    ];
    let idx = Select::with_theme(theme)
        .with_prompt("Provider")
        .items(&items)
        .default(ollama.cursor())
        .interact_opt()?;
    Ok(idx.map(|i| provider_at(i).to_string()))
}

/// The menu's row order, in one place so the labels above and the ids here
/// cannot drift apart — they did once, and a mis-ordered arm sends someone
/// into the wrong provider's setup with no sign anything went wrong.
fn provider_at(index: usize) -> &'static str {
    match index {
        0 => "ollama",
        1 => "openrouter",
        2 => "anthropic",
        3 => "openai",
        _ => "9router",
    }
}

fn setup_api_provider(
    theme: &ColorfulTheme,
    config: &mut Config,
    name: &str,
    models: &[&str],
) -> color_eyre::Result<bool> {
    let existing_key = if name == "anthropic" {
        config.anthropic.api_key.clone()
    } else {
        config.openai.api_key.clone()
    };
    let prompt = match &existing_key {
        Some(_) => format!("API key for {name} (already set — blank keeps it)"),
        None => format!("API key for {name}"),
    };
    let entered: String = Password::with_theme(theme)
        .with_prompt(prompt)
        .allow_empty_password(true)
        .interact()?;
    let key = if entered.is_empty() {
        existing_key
    } else {
        Some(entered)
    };
    let Some(key) = key else {
        println!("An API key is required for {name} — section left unchanged.");
        return Ok(false);
    };

    let Some(model) = select_model(theme, models)? else {
        return Ok(false);
    };

    if name == "anthropic" {
        config.anthropic.api_key = Some(key);
    } else {
        config.openai.api_key = Some(key);
    }
    config.general.provider = Some(name.to_string());
    config.general.model = Some(model);
    Ok(true)
}

/// `None` when the user backed out with Esc.
fn select_model(theme: &ColorfulTheme, models: &[&str]) -> color_eyre::Result<Option<String>> {
    let mut items: Vec<&str> = models.to_vec();
    items.push("Other (type a model name)");
    let Some(idx) = Select::with_theme(theme)
        .with_prompt("Model")
        .items(&items)
        .default(0)
        .interact_opt()?
    else {
        return Ok(None);
    };
    if idx == models.len() {
        let custom: String = Input::with_theme(theme)
            .with_prompt("Model name")
            .interact_text()?;
        Ok(Some(custom))
    } else {
        Ok(Some(models[idx].to_string()))
    }
}

/// The free cloud path. Walks the user through a free key, validates it live
/// against the catalogue, and seeds both fallback layers.
async fn setup_openrouter(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    println!("OpenRouter aggregates many models behind one key; the `:free` ones cost nothing.");
    println!("Create a free key at https://openrouter.ai/keys (a minute, no card).");
    println!("Free-tier limits: 20 req/min; 50 free-model requests/day, or 1000/day after a");
    println!("one-time $10 top-up. smith falls back automatically when the day runs out.\n");

    // `build_provider` prefers `OPENROUTER_API_KEY` over anything saved, so
    // prompting for a key that would be ignored is worse than not asking: the
    // user types a secret and smith uses a different one.
    let from_env = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let existing_key = config.openrouter.api_key.clone().or(from_env.clone());
    let prompt = match (&from_env, &existing_key) {
        (Some(_), _) => {
            println!("Using OPENROUTER_API_KEY from the environment; it outranks anything saved.");
            "OpenRouter API key (env is in use — blank keeps it)".to_string()
        }
        (None, Some(_)) => "OpenRouter API key (already set — blank keeps it)".to_string(),
        (None, None) => "OpenRouter API key".to_string(),
    };
    let entered: String = Password::with_theme(theme)
        .with_prompt(prompt)
        .allow_empty_password(true)
        .interact()?;
    let Some(key) = (if entered.is_empty() {
        existing_key
    } else {
        Some(entered)
    }) else {
        println!("An API key is required — section left unchanged.");
        return Ok(false);
    };

    // Validate the key against the live catalogue, and use the answer to
    // refresh the curated chain: free models rotate, and a chain of retired
    // ids would make the server-side fallback a no-op.
    let base_url = config
        .openrouter
        .base_url
        .clone()
        .unwrap_or_else(|| smith_config::DEFAULT_OPENROUTER_BASE_URL.to_string());
    let probe = smith_provider::OpenAiProvider::openrouter(key.clone(), base_url);
    let live = probe.list_free_tool_models().await;

    let chain: Vec<String> = if live.is_empty() {
        println!("Could not reach the OpenRouter catalogue — using the built-in list.");
        println!("(If the key is wrong you will see a 401 on the first message; re-run setup.)");
        OPENROUTER_MODELS.iter().map(|m| m.to_string()).collect()
    } else {
        // Curated order first (it encodes quality), then whatever else is
        // live — so a fully-rotated catalogue still yields a working chain.
        let mut chain: Vec<String> = OPENROUTER_MODELS
            .iter()
            .map(|m| m.to_string())
            .filter(|m| live.contains(m))
            .collect();
        for model in &live {
            if !chain.contains(model) {
                chain.push(model.clone());
            }
        }
        println!(
            "Key OK — {} free tool-capable models live right now.",
            live.len()
        );
        chain
    };

    let Some(primary) = chain.first().cloned() else {
        println!("No free tool-capable models available — section left unchanged.");
        return Ok(false);
    };
    println!("Primary model: {primary}");
    println!(
        "Server-side fallback chain: {}",
        chain
            .iter()
            .skip(1)
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );

    config.openrouter.api_key = Some(key);
    config.openrouter.fallback_models = chain;
    config.general.provider = Some("openrouter".to_string());
    config.general.model = Some(primary);

    // The second layer: where smith itself goes when the *account* quota dies.
    // This used to be a `Confirm` defaulted to yes, which is a prompt that is
    // not asking anything. It is computed now, from what is actually on the
    // machine — and only entries that could work are written, because an
    // unusable entry is a hard error at startup, not a skip.
    let mut providers = Vec::new();
    if config.nine_router.api_key.is_some() {
        providers.push("9router".to_string());
    }
    if ollama_model_count().await.is_some_and(|n| n > 0) {
        providers.push("ollama".to_string());
    }
    if providers.is_empty() {
        println!("No local fallback available yet — set up Ollama or 9Router to add one.");
    } else {
        config.fallback.providers = providers;
        println!(
            "Falling back to {} when the daily quota runs out; change with `smith setup`.",
            config.fallback.providers.join(", ")
        );
    }
    Ok(true)
}

/// The free local path: a private Node + the pinned 9router package, then the
/// dashboard key. Failure is non-fatal and re-enterable, like the browser
/// section.
async fn setup_ninerouter(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    let base_url = config
        .nine_router
        .base_url
        .clone()
        .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());

    let runtime_root = smith_config::runtime_dir()?;
    if !crate::node_runtime::ninerouter_healthy(&base_url).await {
        let confirmed = Confirm::with_theme(theme)
            .with_prompt(format!(
                "Download Node.js {} (~50 MB, one time, into ~/.smith/runtime) and install the \
                 9router gateway?",
                crate::node_runtime::NODE_VERSION
            ))
            .default(true)
            .interact_opt()?
            .unwrap_or(false);
        if !confirmed {
            return Ok(false);
        }

        let source = HttpAssetSource::new().map_err(color_eyre::eyre::Error::msg)?;
        let mut out = Vec::new();
        let node = crate::node_runtime::provision_node(&source, &runtime_root, &mut out)
            .await
            .map_err(color_eyre::eyre::Error::msg)?;
        for line in &out {
            println!("  {line}");
        }
        println!(
            "  node {} ready{}",
            node.reported_version,
            if node.reused {
                " (reused existing install)"
            } else {
                ""
            }
        );

        let mut out = Vec::new();
        let gateway =
            crate::node_runtime::provision_ninerouter(&node.binary, &runtime_root, &mut out)
                .await
                .map_err(color_eyre::eyre::Error::msg)?;
        println!(
            "  9router@{} installed at {}{}",
            gateway.version,
            gateway.cli.display(),
            if gateway.reused {
                " (already present)"
            } else {
                ""
            }
        );

        config.runtime.node_path = Some(node.binary.display().to_string());
        config.runtime.node_version = Some(node.version);
        config.runtime.ninerouter_dir = Some(runtime_root.join("9router").display().to_string());
        config.runtime.ninerouter_version = Some(gateway.version);

        // Persisted before the gateway is asked to start, not after the section
        // completes. Fifty megabytes were downloaded, verified and unpacked;
        // that happened whether or not the next step works, and a `?` on the
        // start used to throw the record of it away — leaving an install on
        // disk that the config could not see and `smith doctor` reported as
        // missing. The rest of the section still saves the ordinary way.
        save(config)?;

        println!("Starting the gateway…");
        crate::node_runtime::ensure_ninerouter_running(config)
            .await
            .map_err(color_eyre::eyre::Error::msg)?;
    }
    println!("Gateway answering on {base_url}.");

    // A gateway with no upstreams answers `200 {"data":[]}` and is "healthy"
    // by every check smith had. It then 404s on the first message with
    // `No active credentials for provider: openai`, in the middle of a
    // conversation, which is the worst possible place to learn it. So the
    // section waits here until the dashboard has something in it.
    let upstreams = loop {
        match crate::node_runtime::ninerouter_upstreams(&base_url).await {
            Ok(models) if !models.is_empty() => break models,
            Ok(_) => {
                println!("The gateway is running but routes to nothing yet.");
                println!("Open http://localhost:20128, add a provider under `Providers`,");
                println!("then come back — smith will check again.");
            }
            Err(e) => println!("Could not read the gateway's model list: {e}"),
        }
        let again = Confirm::with_theme(theme)
            .with_prompt("Check again?")
            .default(true)
            .interact_opt()?;
        if again != Some(true) {
            println!("Section left unchanged — a gateway with no providers cannot answer.");
            return Ok(false);
        }
    };
    println!(
        "{} models available through the gateway.\n",
        upstreams.len()
    );

    // Offer them. `auto` used to be written unconditionally, and this gateway
    // does not have a model by that name: it was resolved to a provider called
    // `openai` with no credentials, which is where the 404 came from. It is
    // still offered when the gateway itself lists it.
    let Some(model) = select_model(
        theme,
        &upstreams.iter().map(String::as_str).collect::<Vec<_>>(),
    )?
    else {
        return Ok(false);
    };

    println!("Copy an API key from the dashboard at http://localhost:20128.\n");

    let existing_key = config.nine_router.api_key.clone();
    let prompt = match &existing_key {
        Some(_) => "9Router API key (already set — blank keeps it)".to_string(),
        None => "9Router API key (from the local dashboard)".to_string(),
    };
    let entered: String = Password::with_theme(theme)
        .with_prompt(prompt)
        .allow_empty_password(true)
        .interact()?;
    let Some(key) = (if entered.is_empty() {
        existing_key
    } else {
        Some(entered)
    }) else {
        println!("The gateway requires its dashboard key — section left unchanged.");
        return Ok(false);
    };

    config.nine_router.api_key = Some(key);
    if config.nine_router.base_url.is_none() {
        config.nine_router.base_url = Some(base_url);
    }
    config.general.provider = Some("9router".to_string());
    // Both, because `[9router] model` is what a *fallback* chain entry reads
    // and `[general] model` is what the primary uses — writing only one left
    // the chain asking for something else.
    config.nine_router.model = Some(model.clone());
    config.general.model = Some(model);
    Ok(true)
}

async fn setup_ollama(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    if !ollama_binary_present() {
        println!("Ollama doesn't seem to be installed.");
        println!("Install it from https://ollama.com/download, then re-run `smith setup`.");
        return Ok(false);
    }

    // The daemon has to be up before it can be asked what it has.
    ensure_ollama_running().await?;

    let base_url = config
        .ollama
        .base_url
        .clone()
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());
    let live = smith_provider::ollama_tags(&base_url)
        .await
        .unwrap_or_default();

    let Some(model) = pick_ollama_model(theme, &live)? else {
        return Ok(false);
    };

    // A cloud model is proxied to ollama.com; there are no weights to fetch,
    // and `ollama pull` on one is either a no-op or an error depending on the
    // version. A local one still needs its gigabytes.
    let is_cloud = live
        .iter()
        .find(|m| m.name == model)
        .map(|m| m.is_cloud)
        .unwrap_or_else(|| smith_provider::is_cloud_name(&model));

    if !is_cloud {
        println!("Pulling {model} (this can take a while)...");
        let status = tokio::process::Command::new("ollama")
            .arg("pull")
            .arg(&model)
            .stdin(Stdio::null())
            .status()
            .await?;
        if !status.success() {
            // Not fatal, deliberately: a four-gigabyte download that failed
            // should not throw away a wizard section, the same way a failed
            // browser provision does not. Same treatment as `section_browser`.
            println!("`ollama pull {model}` failed — section left unchanged.");
            println!("Fix the pull (disk space? network?) and re-run `smith setup`.");
            return Ok(false);
        }
    }

    config.general.provider = Some("ollama".to_string());
    config.general.model = Some(model);
    if config.ollama.base_url.is_none() {
        config.ollama.base_url = Some(DEFAULT_OLLAMA_BASE_URL.to_string());
    }
    Ok(true)
}

/// Offers what the daemon actually has, cloud first.
///
/// The wizard used to show nine hardcoded names, so a machine's own models
/// were invisible and one keypress picked something it had never pulled. The
/// static list survives as the fallback for a daemon that did not answer —
/// and says that it is a fallback, the way the OpenRouter section already
/// does when its catalogue call fails.
fn pick_ollama_model(
    theme: &ColorfulTheme,
    live: &[smith_provider::OllamaModel],
) -> color_eyre::Result<Option<String>> {
    let offered = ollama_choices(live);
    if offered.is_empty() {
        println!("Could not read the local model list — showing the built-in one.");
        return select_model(theme, known_models("ollama"));
    }

    let mut items: Vec<String> = offered.iter().map(|c| c.label.clone()).collect();
    items.push("Other (type a model name — it will be pulled if needed)".to_string());

    let Some(idx) = Select::with_theme(theme)
        .with_prompt("Model")
        .items(&items)
        .default(0)
        .interact_opt()?
    else {
        return Ok(None);
    };
    if idx == offered.len() {
        let custom: String = Input::with_theme(theme)
            .with_prompt("Model name, e.g. `llama3.3` or `gpt-oss:20b-cloud`")
            .interact_text()?;
        let custom = custom.trim().to_string();
        return Ok((!custom.is_empty()).then_some(custom));
    }
    Ok(Some(offered[idx].name.clone()))
}

/// One row of the model picker.
struct OllamaChoice {
    name: String,
    label: String,
}

/// What to offer for Ollama: what the daemon already has, plus the free cloud
/// models it could reach.
///
/// The daemon only lists what has been pulled or linked, so a fresh install
/// shows almost nothing — and the cloud models are exactly the ones worth
/// offering there, because they need no VRAM and no download. They are merged
/// in rather than replacing the live list, and the live entry wins on a
/// collision, because a linked model comes with facts (its real window, its
/// capabilities) that a name alone does not carry.
fn ollama_choices(live: &[smith_provider::OllamaModel]) -> Vec<OllamaChoice> {
    let mut out: Vec<OllamaChoice> = Vec::new();

    // Cloud first: no VRAM, no download, so they are what a machine that just
    // installed ollama can actually run. Within each group a model that
    // cannot call tools sorts last — offered, but not where the cursor lands.
    let mut ordered: Vec<&smith_provider::OllamaModel> = live.iter().collect();
    ordered.sort_by_key(|m| (!m.is_cloud, !m.supports_tools));
    for model in ordered {
        out.push(OllamaChoice {
            name: model.name.clone(),
            label: model.summary(),
        });
    }

    for name in smith_store::models::OLLAMA_FREE_CLOUD_MODELS {
        if out.iter().any(|c| c.name == *name) {
            continue;
        }
        out.push(OllamaChoice {
            name: (*name).to_string(),
            label: format!("{name}  cloud · free tier"),
        });
    }
    out
}

// ---- section: web search ----------------------------------------------------

/// Its own submenu, one row per backend layer, looping until "Back". Each row
/// edits exactly one setting so there is never a wrong order to answer in.
fn section_search(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    let mut changed = false;
    loop {
        let items = [
            format!(
                "Backend          [{}]   auto = try every tier in order; a pin never falls back",
                config.search.backend.as_deref().unwrap_or("auto")
            ),
            format!(
                "SearXNG URL      [{}]   your own instance — best free backend",
                config.search.searxng_url.as_deref().unwrap_or("not set")
            ),
            format!(
                "Tavily API key   [{}]   free tier: 1,000 searches/month (app.tavily.com)",
                key_status(config.tavily.api_key.as_deref())
            ),
            format!(
                "Exa API key      [{}]   paid (dashboard.exa.ai)",
                key_status(config.exa.api_key.as_deref())
            ),
            format!(
                "Bing market      [{}]   auto = follow the query's language",
                config.search.market.as_deref().unwrap_or("auto")
            ),
            "Back".to_string(),
        ];
        let choice = Select::with_theme(theme)
            .with_prompt("Web search (free Bing/Google News tiers always work; these upgrade them)")
            .items(&items)
            .default(items.len() - 1)
            .interact_opt()?;

        match choice {
            Some(0) => {
                let backends = [
                    "auto (recommended — full fall-through chain)",
                    "searxng",
                    "tavily",
                    "exa",
                    "bing",
                    "bing-browser",
                    "google-news",
                    "duckduckgo",
                ];
                let current = config
                    .search
                    .backend
                    .as_deref()
                    .and_then(|b| backends.iter().position(|x| *x == b))
                    .unwrap_or(0);
                if let Some(idx) = Select::with_theme(theme)
                    .with_prompt("Backend (a pin runs only that backend and never falls back)")
                    .items(&backends)
                    .default(current)
                    .interact_opt()?
                {
                    let next = (idx > 0).then(|| backends[idx].to_string());
                    changed |= next != config.search.backend;
                    config.search.backend = next;
                }
            }
            Some(1) => {
                let entered: String = Input::with_theme(theme)
                    .with_prompt("SearXNG base URL (blank keeps, '-' clears)")
                    .allow_empty(true)
                    .interact_text()?;
                let next = apply_optional(config.search.searxng_url.take(), &entered);
                changed |= next != config.search.searxng_url;
                config.search.searxng_url = next;
            }
            Some(2) => {
                let entered: String = Password::with_theme(theme)
                    .with_prompt("Tavily API key (blank keeps, '-' clears)")
                    .allow_empty_password(true)
                    .interact()?;
                let next = apply_optional(config.tavily.api_key.take(), &entered);
                changed |= next != config.tavily.api_key;
                config.tavily.api_key = next;
            }
            Some(3) => {
                let entered: String = Password::with_theme(theme)
                    .with_prompt("Exa API key (blank keeps, '-' clears)")
                    .allow_empty_password(true)
                    .interact()?;
                let next = apply_optional(config.exa.api_key.take(), &entered);
                changed |= next != config.exa.api_key;
                config.exa.api_key = next;
            }
            Some(4) => {
                let entered: String = Input::with_theme(theme)
                    .with_prompt("Bing market tag, e.g. pt-BR (blank keeps, '-' returns to auto)")
                    .allow_empty(true)
                    .interact_text()?;
                let next = apply_optional(config.search.market.take(), &entered);
                changed |= next != config.search.market;
                config.search.market = next;
            }
            _ => break,
        }
    }
    Ok(changed)
}

// ---- section: permissions ---------------------------------------------------

fn section_permissions(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    let items = [
        "ask      confirm every mutating tool call (default, safest)",
        "session  auto-allow file edits; still confirm shell commands",
        "skip     auto-allow everything — no safety net, set deliberately",
    ];
    let current = config
        .general
        .permission_policy
        .as_deref()
        .map(|p| match p {
            "session" => 1,
            "skip" => 2,
            _ => 0,
        })
        .unwrap_or(0);
    let Some(idx) = Select::with_theme(theme)
        .with_prompt("Default permission policy (changeable per session with /permission)")
        .items(&items)
        .default(current)
        .interact_opt()?
    else {
        return Ok(false);
    };
    let chosen = ["ask", "session", "skip"][idx];
    let changed = config.general.permission_policy.as_deref() != Some(chosen);
    config.general.permission_policy = Some(chosen.to_string());
    Ok(changed)
}

// ---- section: browser ---------------------------------------------------------

/// The explicit provisioning step: offers to download a headless browser for
/// `web_search`'s Chromium tier, and records where it landed. Never fatal —
/// a browser is an upgrade to one search backend, not a prerequisite.
async fn section_browser(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    match setup_browser(theme, config).await {
        Ok(changed) => Ok(changed),
        Err(e) => {
            eprintln!("\nCould not provision a browser: {e}");
            eprintln!("web_search will still work over plain HTTP, with weaker results.");
            eprintln!("Pick this section again to retry, or run `smith doctor` for details.");
            Ok(false)
        }
    }
}

async fn setup_browser(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    let existing = runtime::find_browser(&config.runtime);
    let prompt = match &existing {
        Some(found) => {
            let origin = match found.source {
                BrowserSource::Env(var) => format!("from {var}"),
                BrowserSource::Provisioned => "already provisioned by smith".to_string(),
                BrowserSource::System => "already installed on this machine".to_string(),
            };
            println!("Found a browser {origin}: {}", found.path.display());
            // Default to leaving a working setup alone; downloading over a
            // browser someone already has is rude and slow.
            "Download smith's own headless browser anyway?"
        }
        None => {
            println!("No Chromium-family browser found.");
            println!(
                "web_search can use one to read search results the way a real visitor would; \
                 without it, it falls back to plain HTTP and weaker results."
            );
            "Download a headless browser now (~100 MB, one time)?"
        }
    };

    if !Confirm::with_theme(theme)
        .with_prompt(prompt)
        .default(existing.is_none())
        .interact()?
    {
        return Ok(false);
    }

    let root = smith_config::runtime_dir()?;
    let source = HttpAssetSource::new().map_err(color_eyre::eyre::Report::msg)?;
    let installed = runtime::provision_chromium(&source, &root, &mut std::io::stdout())
        .await
        .map_err(color_eyre::eyre::Report::msg)?;

    if installed.reused {
        println!("Nothing to download — that install is current.");
    }
    if let Some(integrity) = installed.integrity {
        println!("Integrity: {}", integrity.describe());
    }
    println!("Ready: {}", installed.reported_version);
    println!("  {}", installed.binary.display());

    config.runtime.chromium_path = Some(installed.binary.to_string_lossy().into_owned());
    config.runtime.chromium_version = Some(installed.version);
    Ok(true)
}

// ---- ollama plumbing ----------------------------------------------------------

fn ollama_binary_present() -> bool {
    std::process::Command::new("ollama")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Ensures an Ollama daemon is reachable, starting `ollama serve` in the
/// background (detached — it outlives this `smith setup` invocation) if one
/// isn't already running.
async fn ensure_ollama_running() -> color_eyre::Result<()> {
    if ollama_reachable().await {
        println!("ollama is already running.");
        return Ok(());
    }

    println!("Starting `ollama serve` in the background...");
    std::process::Command::new("ollama")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(300)).await;
        if ollama_reachable().await {
            println!("ollama is up.");
            return Ok(());
        }
    }

    color_eyre::eyre::bail!("timed out waiting for `ollama serve` to come up")
}

async fn ollama_reachable() -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
    else {
        return false;
    };
    client
        .get(format!("{OLLAMA_HOST}/api/tags"))
        .send()
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon lists only what has been pulled, so a fresh install shows
    /// almost nothing — and the free cloud models are exactly what is worth
    /// offering there, since they need no VRAM and no download.
    #[test]
    fn the_free_cloud_models_are_offered_even_when_nothing_is_pulled() {
        let choices = ollama_choices(&[]);
        assert_eq!(
            choices.len(),
            smith_store::models::OLLAMA_FREE_CLOUD_MODELS.len()
        );
        assert!(choices.iter().any(|c| c.name == "nemotron-3-super:cloud"));
        assert!(choices.iter().all(|c| c.label.contains("free tier")));
    }

    /// A linked model comes with facts a name alone does not carry — its real
    /// window, its capabilities — so the live entry wins the collision.
    #[test]
    fn a_linked_model_wins_over_the_curated_entry_of_the_same_name() {
        let live = vec![smith_provider::OllamaModel {
            name: "nemotron-3-super:cloud".to_string(),
            is_cloud: true,
            context_window: Some(262_144),
            size_bytes: None,
            supports_tools: true,
        }];
        let choices = ollama_choices(&live);
        let matching: Vec<_> = choices
            .iter()
            .filter(|c| c.name == "nemotron-3-super:cloud")
            .collect();
        assert_eq!(matching.len(), 1, "offered twice");
        assert!(
            matching[0].label.contains("262k ctx"),
            "the live entry lost its facts: {}",
            matching[0].label
        );
    }

    /// Every curated name has to be one smith recognises as cloud, or setup
    /// would try to download weights for something that has none.
    #[test]
    fn every_curated_cloud_model_reads_as_cloud() {
        for name in smith_store::models::OLLAMA_FREE_CLOUD_MODELS {
            assert!(
                smith_provider::is_cloud_name(name),
                "{name} is curated as cloud but does not read as one"
            );
        }
    }

    /// The browser row shifts every other row by one, which is exactly the
    /// kind of arithmetic that silently sends someone into the wrong section.
    /// One offset, asserted both ways.
    #[test]
    fn the_menu_rows_map_to_the_same_sections_with_or_without_the_browser_row() {
        let section = |row: usize, offered: bool| row as isize - isize::from(offered);

        // Without the browser row, the sections start at 0.
        assert_eq!(section(0, false), 0, "provider");
        assert_eq!(section(3, false), 3, "permissions");

        // With it, row 0 is the browser and everything else keeps its meaning.
        assert_eq!(section(0, true), -1, "browser");
        assert_eq!(section(1, true), 0, "provider");
        assert_eq!(section(4, true), 3, "permissions");
    }

    /// A machine with no display must not be offered a link nothing can open.
    /// Asserted through the env, because that is the only input.
    #[test]
    fn a_display_is_what_makes_the_browser_row_worth_offering() {
        if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
            assert!(browser_plausible(), "a desktop OS always has one");
            return;
        }
        let has_any = [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "WSL_DISTRO_NAME",
            "WSL_INTEROP",
        ]
        .iter()
        .any(|v| std::env::var_os(v).is_some());
        assert_eq!(
            browser_plausible(),
            has_any,
            "the offer must follow the display, not the platform"
        );
    }

    /// Row order and the id table are two lists that have to agree. They are
    /// next to each other in the file, which is exactly the kind of pairing
    /// that drifts — and a drifted arm drops someone into another provider's
    /// setup with nothing looking wrong.
    #[test]
    fn the_provider_menu_rows_and_their_ids_line_up() {
        assert_eq!(provider_at(0), "ollama");
        assert_eq!(provider_at(1), "openrouter");
        assert_eq!(provider_at(2), "anthropic");
        assert_eq!(provider_at(3), "openai");
        assert_eq!(provider_at(4), "9router");

        // Every id the menu can produce has to be one `section_provider`
        // dispatches on, and one `known_models` answers for.
        for i in 0..5 {
            let id = provider_at(i);
            assert!(
                smith_store::models::is_known_provider(id),
                "menu row {i} yields unknown provider {id}"
            );
        }
    }

    /// Ollama sits at row 0, so a usable Ollama is the cursor. A machine
    /// without it must move on rather than aim a newcomer at a connection
    /// refused on 127.0.0.1:11434.
    #[test]
    fn the_cursor_lands_on_ollama_only_when_ollama_could_answer() {
        assert_eq!(OllamaState::Ready(7).cursor(), 0);
        assert_eq!(OllamaState::Installed.cursor(), 0);
        assert_eq!(OllamaState::Absent.cursor(), 1);
        assert_eq!(provider_at(OllamaState::Absent.cursor()), "openrouter");
    }

    /// The row says what the probe found, because "no key needed" is only
    /// true when there is something to run.
    #[test]
    fn the_ollama_row_reports_what_the_probe_found() {
        let ready = OllamaState::Ready(7).row();
        assert!(ready.contains('7'), "{ready}");
        assert!(ready.contains("no key needed"), "{ready}");

        assert!(OllamaState::Installed.row().contains("not running"));

        let absent = OllamaState::Absent.row();
        assert!(absent.contains("not installed"), "{absent}");
        assert!(
            !absent.contains("no key needed"),
            "a machine without ollama must not be promised a keyless provider: {absent}"
        );
    }

    /// A daemon answering with an empty list is not ready: choosing it hands
    /// the user a model picker with nothing in it.
    #[test]
    fn a_daemon_with_no_models_is_not_reported_as_ready() {
        assert!(matches!(OllamaState::Ready(0).cursor(), 0));
        assert!(!OllamaState::Installed.row().contains("models ready"));
    }

    #[test]
    fn apply_optional_keeps_clears_and_replaces() {
        let existing = Some("old".to_string());
        assert_eq!(apply_optional(existing.clone(), ""), existing);
        assert_eq!(apply_optional(existing.clone(), "   "), existing);
        assert_eq!(apply_optional(existing.clone(), "-"), None);
        assert_eq!(
            apply_optional(existing, "new-value"),
            Some("new-value".to_string())
        );
        assert_eq!(apply_optional(None, ""), None);
    }

    #[test]
    fn key_status_never_echoes_the_secret() {
        assert_eq!(key_status(Some("tvly-abc123")), "set");
        assert_eq!(key_status(Some("   ")), "not set");
        assert_eq!(key_status(None), "not set");
    }

    #[test]
    fn provider_summary_reflects_configuration_states() {
        let mut config = Config::default();
        assert_eq!(provider_summary(&config), "not configured");
        config.general.provider = Some("ollama".into());
        config.general.model = Some("qwen2.5".into());
        assert_eq!(provider_summary(&config), "ollama · qwen2.5");
    }

    #[test]
    fn search_summary_names_what_is_configured() {
        let mut config = Config::default();
        assert_eq!(
            search_summary(&config),
            "free tiers only (works out of the box)"
        );
        config.tavily.api_key = Some("k".into());
        config.search.searxng_url = Some("https://sx.example".into());
        assert_eq!(search_summary(&config), "searxng + tavily");
    }

    /// A pin dominates the summary: with one set, the keys beside it are
    /// dormant and listing them would misstate what a search will do.
    #[test]
    fn search_summary_shows_a_pin_over_everything_else() {
        let mut config = Config::default();
        config.tavily.api_key = Some("k".into());
        config.search.backend = Some("searxng".into());
        assert_eq!(search_summary(&config), "pinned: searxng");
    }

    #[test]
    fn permission_summary_defaults_to_ask() {
        let mut config = Config::default();
        assert_eq!(permission_summary(&config), "ask (default)");
        config.general.permission_policy = Some("session".into());
        assert_eq!(permission_summary(&config), "session");
    }
}
