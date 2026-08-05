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

/// Drives the `smith setup` / `smith setup model` wizard. `jump_to_model`
/// skips straight to model selection for the already-configured provider
/// (falling back to the full wizard if nothing is configured yet).
pub async fn run(jump_to_model: bool) -> color_eyre::Result<()> {
    let theme = ColorfulTheme::default();
    let mut config = Config::load().unwrap_or_default();

    let provider = if jump_to_model {
        match config.general.provider.clone() {
            Some(p) => p,
            None => {
                println!("No provider configured yet — let's set one up first.\n");
                select_provider(&theme)?
            }
        }
    } else {
        select_provider(&theme)?
    };

    match provider.as_str() {
        "anthropic" => setup_api_provider(&theme, &mut config, "anthropic", ANTHROPIC_MODELS)?,
        "openai" => setup_api_provider(&theme, &mut config, "openai", OPENAI_MODELS)?,
        "ollama" => setup_ollama(&theme, &mut config).await?,
        other => color_eyre::eyre::bail!("unknown provider: {other}"),
    }

    // Deliberately after the provider is settled and deliberately in `setup`
    // at all: this is the one place a ~100 MB download can be offered, sized
    // and consented to, instead of ambushing a turn that happened to call
    // `web_search`. Never fatal — a browser is an upgrade to one search
    // backend, not a prerequisite for using smith.
    if let Err(e) = setup_browser(&theme, &mut config).await {
        eprintln!("\nCould not provision a browser: {e}");
        eprintln!("web_search will still work over plain HTTP, with weaker results.");
        eprintln!("Re-run `smith setup` to try again, or run `smith doctor` for details.");
    }

    config.save()?;
    println!("\nSaved to {}", smith_config::config_path()?.display());
    println!("Run `smith` to start chatting.");
    Ok(())
}

/// The explicit provisioning step: offers to download a headless browser for
/// `web_search`'s Chromium tier, and records where it landed.
async fn setup_browser(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<()> {
    println!("\n--- web_search browser ---");

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
        println!("Skipped. Run `smith setup` again whenever you want it.");
        return Ok(());
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
    Ok(())
}

fn select_provider(theme: &ColorfulTheme) -> color_eyre::Result<String> {
    let items = ["Anthropic (Claude)", "OpenAI", "Ollama (local)"];
    let idx = Select::with_theme(theme)
        .with_prompt("Which provider do you want to configure?")
        .items(&items)
        .default(0)
        .interact()?;
    Ok(match idx {
        0 => "anthropic",
        1 => "openai",
        _ => "ollama",
    }
    .to_string())
}

fn setup_api_provider(
    theme: &ColorfulTheme,
    config: &mut Config,
    name: &str,
    models: &[&str],
) -> color_eyre::Result<()> {
    let existing_key = if name == "anthropic" {
        config.anthropic.api_key.clone()
    } else {
        config.openai.api_key.clone()
    };
    let prompt = match &existing_key {
        Some(_) => format!("API key for {name} (already set — leave blank to keep it)"),
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
        color_eyre::eyre::bail!("an API key is required for {name}");
    };

    let model = select_model(theme, models)?;

    if name == "anthropic" {
        config.anthropic.api_key = Some(key);
    } else {
        config.openai.api_key = Some(key);
    }
    config.general.provider = Some(name.to_string());
    config.general.model = Some(model);
    Ok(())
}

fn select_model(theme: &ColorfulTheme, models: &[&str]) -> color_eyre::Result<String> {
    let mut items: Vec<&str> = models.to_vec();
    items.push("Other (type a model name)");
    let idx = Select::with_theme(theme)
        .with_prompt("Which model?")
        .items(&items)
        .default(0)
        .interact()?;
    if idx == models.len() {
        let custom: String = Input::with_theme(theme)
            .with_prompt("Model name")
            .interact_text()?;
        Ok(custom)
    } else {
        Ok(models[idx].to_string())
    }
}

async fn setup_ollama(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<()> {
    if !ollama_binary_present() {
        println!("Ollama doesn't seem to be installed.");
        println!("Install it from https://ollama.com/download, then re-run `smith setup`.");
        color_eyre::eyre::bail!("ollama binary not found on PATH");
    }

    let model = select_model(theme, OLLAMA_MODELS)?;

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
    Ok(())
}

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
