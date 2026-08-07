//! What this configuration needs before it can run a turn, and which of those
//! things smith can supply itself.
//!
//! `doctor` answers "is it working"; this answers "can I make it work". The
//! two are deliberately separate: a diagnosis is safe to run anywhere and says
//! everything it knows, while a fix downloads fifty megabytes and installs a
//! gateway that proxies credentials, so it needs the user in the loop and a
//! reason to run at all.
//!
//! Three rules the rest of the module keeps to:
//!
//! - **Only what this config needs.** A user on Anthropic is not told about
//!   Node. What is "in play" is read from `[general] provider` *and*
//!   `[fallback] providers` — a fallback entry is a thing that will be reached
//!   for mid-turn, which is the worst moment to discover it was never set up.
//! - **Never install without being asked.** Every [`Need`] carries its own
//!   approval prompt, and `apply` is only ever called on one the caller
//!   confirmed.
//! - **Say so when smith cannot do it.** Ollama is a third-party installer and
//!   a system daemon; smith can detect it and print the exact command, and
//!   that is the honest maximum. Pretending otherwise would mean piping
//!   somebody else's script into a shell on the user's behalf.

use smith_config::Config;

use crate::node_runtime::{self, MIN_NODE_MAJOR, NODE_VERSION};
use crate::orchestrator::ProviderKind;

/// Which unmet requirement this is. Carried separately from the prose so
/// callers can act on it without matching on a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NeedKey {
    /// No Node at all, or one too old for the gateway.
    Node,
    /// The gateway itself is not installed.
    NineRouter,
    /// The gateway is installed but not answering.
    NineRouterStopped,
    /// No headless browser for `web_search`'s Chromium tier.
    Chromium,
    /// Ollama is selected but not installed.
    Ollama,
    /// Ollama is running, but the configured model has not been pulled.
    OllamaModel(String),
}

/// Who can resolve a [`Need`].
#[derive(Debug, Clone)]
pub enum Fix {
    /// smith can do it, with the user's approval. `cost` is what the prompt
    /// should warn about — a download size, usually.
    Auto {
        action: String,
        cost: Option<String>,
    },
    /// Someone else's installer has to run. smith prints the command and
    /// stops; see the module doc for why it does not run it.
    Manual { command: String },
}

#[derive(Debug, Clone)]
pub struct Need {
    pub key: NeedKey,
    pub name: &'static str,
    /// What is wrong now, present tense, one line.
    pub detail: String,
    pub fix: Fix,
}

impl Need {
    /// The line the approval prompt asks. Ends without punctuation so the
    /// caller can add its own "?".
    pub fn prompt(&self) -> String {
        match &self.fix {
            Fix::Auto {
                action,
                cost: Some(cost),
            } => format!("{action} ({cost})"),
            Fix::Auto { action, cost: None } => action.clone(),
            Fix::Manual { command } => format!("Run this yourself: {command}"),
        }
    }

    pub fn is_auto(&self) -> bool {
        matches!(self.fix, Fix::Auto { .. })
    }
}

/// Whether a provider id is reached by this config, as the primary choice or
/// as a fallback entry.
fn in_play(config: &Config, kind: ProviderKind) -> bool {
    let named = |s: &str| ProviderKind::from_config_str(s) == Some(kind);
    config.general.provider.as_deref().is_some_and(named)
        || config.fallback.providers.iter().any(|p| named(p))
}

/// Everything this config needs and does not have.
///
/// Empty means a turn can run. Order is dependency order — Node before the
/// gateway, Ollama before its models — so a caller that walks the list and
/// stops at the first refusal does not then offer something that could not
/// work anyway.
pub async fn survey(config: &Config) -> Vec<Need> {
    let mut needs = Vec::new();
    if in_play(config, ProviderKind::NineRouter) {
        survey_gateway(config, &mut needs).await;
    }
    if in_play(config, ProviderKind::Ollama) {
        survey_ollama(config, &mut needs).await;
    }
    survey_browser(config, &mut needs);
    needs
}

async fn survey_gateway(config: &Config, needs: &mut Vec<Need>) {
    let base_url = config
        .nine_router
        .base_url
        .clone()
        .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());

    // A gateway that is already answering needs nothing behind it: whatever
    // Node started it is, by demonstration, good enough.
    if node_runtime::ninerouter_healthy(&base_url).await {
        return;
    }

    let node = node_runtime::resolve_node(&config.runtime).await;
    let node_ok = node.as_ref().is_some_and(|n| n.usable());
    if !node_ok {
        needs.push(Need {
            key: NeedKey::Node,
            name: "node",
            detail: match &node {
                Some(found) => format!(
                    "9router needs Node {MIN_NODE_MAJOR} or newer; found {}",
                    found.describe()
                ),
                None => format!("9router needs Node {MIN_NODE_MAJOR} or newer; none found"),
            },
            fix: Fix::Auto {
                action: format!(
                    "Download a private Node {NODE_VERSION} into ~/.smith/runtime \
                     (your own Node is left alone)"
                ),
                cost: Some("~50 MB, once".into()),
            },
        });
    }

    let installed = config
        .runtime
        .ninerouter_dir
        .as_deref()
        .map(|dir| node_runtime::ninerouter_cli(std::path::Path::new(dir)).is_file())
        .unwrap_or(false);

    if !installed {
        needs.push(Need {
            key: NeedKey::NineRouter,
            name: "9router",
            detail: "the gateway is not installed".into(),
            fix: Fix::Auto {
                action: format!(
                    "Install 9router@{} into ~/.smith/runtime",
                    node_runtime::NINEROUTER_VERSION
                ),
                cost: Some("a few MB".into()),
            },
        });
    } else if node_ok {
        // Installed, a usable Node, and still not answering: it just is not
        // running. Worth its own entry because the fix is a spawn, not a
        // download, and saying "install the gateway" about an installed
        // gateway is how someone ends up reinstalling it for no reason.
        needs.push(Need {
            key: NeedKey::NineRouterStopped,
            name: "9router",
            detail: format!("installed, but nothing is answering on {base_url}"),
            fix: Fix::Auto {
                action: "Start the gateway".into(),
                cost: None,
            },
        });
    }
}

async fn survey_ollama(config: &Config, needs: &mut Vec<Need>) {
    let base = config
        .ollama
        .base_url
        .clone()
        .unwrap_or_else(|| smith_config::DEFAULT_OLLAMA_BASE_URL.to_string());

    if !crate::doctor::ollama_daemon_reachable().await {
        needs.push(Need {
            key: NeedKey::Ollama,
            name: "ollama",
            detail: format!("nothing is answering at {base}"),
            fix: Fix::Manual {
                command: crate::doctor::ollama_install_command(std::env::consts::OS).to_string(),
            },
        });
        // Without a daemon there is no model list to compare against, and
        // guessing would produce a `pull` for a model that may already be
        // there.
        return;
    }

    // Which model this config will actually ask Ollama for: its own when
    // Ollama is the primary provider, `[ollama] fallback_model` when it is a
    // fallback entry and the primary's model name means nothing here.
    let wanted = if config
        .general
        .provider
        .as_deref()
        .and_then(ProviderKind::from_config_str)
        == Some(ProviderKind::Ollama)
    {
        config.general.model.clone()
    } else {
        config.ollama.model.clone()
    };
    let Some(wanted) = wanted.filter(|m| !m.is_empty()) else {
        return;
    };

    // Cloud models are served by Ollama's own hosted side and are not on disk,
    // so `pull` is neither needed nor meaningful for them.
    if smith_provider::ollama::is_cloud_name(&wanted) {
        return;
    }

    let Ok(models) = smith_provider::ollama::ollama_tags(&base).await else {
        return;
    };
    let present = models
        .iter()
        .any(|m| m.name == wanted || m.name.split(':').next() == Some(wanted.as_str()));
    if !present {
        needs.push(Need {
            key: NeedKey::OllamaModel(wanted.clone()),
            name: "ollama model",
            detail: format!("`{wanted}` is configured but has not been pulled"),
            fix: Fix::Auto {
                action: format!("Run `ollama pull {wanted}`"),
                cost: Some("model-sized, often several GB".into()),
            },
        });
    }
}

fn survey_browser(config: &Config, needs: &mut Vec<Need>) {
    // Only when the Chromium tier could actually be reached. A pinned backend
    // that is not the browser one makes this irrelevant, and `web_search`
    // works with no browser at all — the Chromium tier is the fifth of seven.
    if config
        .search
        .backend
        .as_deref()
        .is_some_and(|b| b != "bing-browser")
    {
        return;
    }
    if crate::runtime::find_browser(&config.runtime).is_some() {
        return;
    }
    needs.push(Need {
        key: NeedKey::Chromium,
        name: "browser",
        detail: "web_search's browser tier has no Chromium to drive".into(),
        fix: Fix::Auto {
            action: "Download Chrome for Testing into ~/.smith/runtime".into(),
            cost: Some("~150 MB, once".into()),
        },
    });
}

/// Applies one approved fix, appending progress lines to `out`.
///
/// `config` is updated in place with whatever was provisioned; **the caller
/// saves it**. That is deliberate: a download that succeeded and a config that
/// was never written is an install on disk nothing can see, which is the exact
/// failure `setup_ninerouter` already carries a comment about.
pub async fn apply(config: &mut Config, need: &Need, out: &mut Vec<String>) -> Result<(), String> {
    let Fix::Auto { .. } = need.fix else {
        return Err("this one has to be installed by hand".into());
    };
    let root = smith_config::runtime_dir().map_err(|e| e.to_string())?;

    match &need.key {
        NeedKey::Node => {
            let source = crate::runtime::HttpAssetSource::new()?;
            let node = node_runtime::provision_node(&source, &root, out).await?;
            out.push(format!(
                "node {} ready{}",
                node.reported_version,
                if node.reused {
                    " (already present)"
                } else {
                    ""
                }
            ));
            config.runtime.node_path = Some(node.binary.display().to_string());
            config.runtime.node_version = Some(node.version);
            Ok(())
        }
        NeedKey::NineRouter => {
            let node = node_runtime::resolve_node(&config.runtime)
                .await
                .filter(|n| n.usable())
                .ok_or_else(|| {
                    format!("install Node {MIN_NODE_MAJOR}+ first — the gateway runs on it")
                })?;
            let gateway = node_runtime::provision_ninerouter(&node.path, &root, out).await?;
            out.push(format!(
                "9router@{} installed{}",
                gateway.version,
                if gateway.reused {
                    " (already present)"
                } else {
                    ""
                }
            ));
            config.runtime.ninerouter_dir = Some(root.join("9router").display().to_string());
            config.runtime.ninerouter_version = Some(gateway.version);
            Ok(())
        }
        NeedKey::NineRouterStopped => {
            node_runtime::ensure_ninerouter_running(config).await?;
            out.push("gateway answering".into());
            Ok(())
        }
        NeedKey::Chromium => {
            let source = crate::runtime::HttpAssetSource::new()?;
            let mut sink = Vec::new();
            let browser = crate::runtime::provision_chromium(&source, &root, &mut sink).await?;
            out.extend(String::from_utf8_lossy(&sink).lines().map(str::to_string));
            out.push(format!("{} ready", browser.reported_version));
            config.runtime.chromium_path = Some(browser.binary.display().to_string());
            config.runtime.chromium_version = Some(browser.version);
            Ok(())
        }
        NeedKey::OllamaModel(model) => {
            pull_ollama_model(model, out).await?;
            out.push(format!("{model} pulled"));
            Ok(())
        }
        NeedKey::Ollama => Err("ollama has to be installed by its own installer".into()),
    }
}

/// `ollama pull`, streamed to `out` a line at a time.
///
/// Shelling out rather than driving the HTTP pull API: the CLI is what the
/// user would have run, its progress output is the one they recognise, and a
/// multi-gigabyte download is exactly the wrong place to reimplement someone
/// else's progress reporting.
async fn pull_ollama_model(model: &str, out: &mut Vec<String>) -> Result<(), String> {
    let status = tokio::process::Command::new("ollama")
        .arg("pull")
        .arg(model)
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|e| format!("could not run `ollama pull {model}`: {e}"))?;
    if !status.success() {
        return Err(format!(
            "`ollama pull {model}` failed. Check the name against `ollama list` \
             and https://ollama.com/library."
        ));
    }
    out.push(String::new());
    Ok(())
}

#[cfg(test)]
mod tests;
