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

const ANTHROPIC_MODELS: &[&str] = &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"];
const OPENAI_MODELS: &[&str] = &["gpt-4.1", "gpt-4.1-mini", "gpt-4o", "o3"];
const OLLAMA_MODELS: &[&str] = &[
    "llama3.3",
    "llama3.2",
    "qwen2.5",
    "qwen3",
    "mistral",
    "gemma2",
    "phi4",
    "deepseek-r1",
    "codellama",
];

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
                let models = match provider.as_str() {
                    "anthropic" => ANTHROPIC_MODELS,
                    "openai" => OPENAI_MODELS,
                    _ => OLLAMA_MODELS,
                };
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

    loop {
        let items = [
            format!("Provider & model   [{}]", provider_summary(&config)),
            format!("Web search         [{}]", search_summary(&config)),
            format!("Browser            [{}]", browser_summary(&config)),
            format!("Permissions        [{}]", permission_summary(&config)),
            "Done".to_string(),
        ];
        let choice = Select::with_theme(&theme)
            .with_prompt("Section")
            .items(&items)
            .default(0)
            .interact_opt()?;

        let changed = match choice {
            Some(0) => section_provider(&theme, &mut config).await?,
            Some(1) => section_search(&theme, &mut config)?,
            Some(2) => section_browser(&theme, &mut config).await?,
            Some(3) => section_permissions(&theme, &mut config)?,
            _ => break, // Done, or Esc at the top level.
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
    let Some(provider) = select_provider(theme)? else {
        return Ok(false);
    };
    match provider.as_str() {
        "anthropic" => setup_api_provider(theme, config, "anthropic", ANTHROPIC_MODELS),
        "openai" => setup_api_provider(theme, config, "openai", OPENAI_MODELS),
        "ollama" => setup_ollama(theme, config).await,
        other => color_eyre::eyre::bail!("unknown provider: {other}"),
    }
}

fn select_provider(theme: &ColorfulTheme) -> color_eyre::Result<Option<String>> {
    let items = ["Anthropic (Claude)", "OpenAI", "Ollama (local)"];
    let idx = Select::with_theme(theme)
        .with_prompt("Provider")
        .items(&items)
        .default(0)
        .interact_opt()?;
    Ok(idx.map(|i| {
        match i {
            0 => "anthropic",
            1 => "openai",
            _ => "ollama",
        }
        .to_string()
    }))
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

async fn setup_ollama(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    if !ollama_binary_present() {
        println!("Ollama doesn't seem to be installed.");
        println!("Install it from https://ollama.com/download, then re-run `smith setup`.");
        return Ok(false);
    }

    let Some(model) = select_model(theme, OLLAMA_MODELS)? else {
        return Ok(false);
    };

    ensure_ollama_running().await?;

    println!("Pulling {model} (this can take a while)...");
    let status = tokio::process::Command::new("ollama")
        .arg("pull")
        .arg(&model)
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        color_eyre::eyre::bail!("`ollama pull {model}` failed");
    }

    config.general.provider = Some("ollama".to_string());
    config.general.model = Some(model);
    if config.ollama.base_url.is_none() {
        config.ollama.base_url = Some(DEFAULT_OLLAMA_BASE_URL.to_string());
    }
    Ok(true)
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
