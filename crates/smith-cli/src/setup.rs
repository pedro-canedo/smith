use std::process::Stdio;
use std::time::Duration;

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password, Select};
use smith_store::{Config, DEFAULT_OLLAMA_BASE_URL, OLLAMA_HOST};

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

    config.save()?;
    println!(
        "\nSaved to {}",
        smith_store::config::config_path()?.display()
    );
    println!("Run `smith` to start chatting.");
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
