//! The `Runtimes` section: resolve everything this config needs to run.
//!
//! Its own file because it is the one section that *installs* rather than
//! configures, and the approval it asks for is per item rather than per
//! section — a user who wants Node but not a 150 MB browser should get
//! exactly that, and the section-level `Ok(true)/Ok(false)` the other
//! sections return cannot express it.

use dialoguer::theme::ColorfulTheme;
use dialoguer::Confirm;
use smith_config::Config;

use crate::preflight::{self, Fix, Need};

/// One-line state for the main menu.
pub(super) async fn summary(config: &Config) -> String {
    let needs = preflight::survey(config).await;
    if needs.is_empty() {
        return "ready".to_string();
    }
    let manual = needs.iter().filter(|n| !n.is_auto()).count();
    match (needs.len(), manual) {
        (1, 0) => "1 thing missing".to_string(),
        (n, 0) => format!("{n} things missing"),
        (n, m) if n == m => format!("{n} to install by hand"),
        (n, m) => format!("{} to install, {m} by hand", n - m),
    }
}

/// `Ok(true)` when something was installed and `config` changed.
pub(super) async fn run(theme: &ColorfulTheme, config: &mut Config) -> color_eyre::Result<bool> {
    let needs = preflight::survey(config).await;
    if needs.is_empty() {
        println!("Everything this configuration needs is already in place.");
        return Ok(false);
    }

    println!("This configuration needs {} thing(s):\n", needs.len());
    for need in &needs {
        println!("  {:<12} {}", need.name, need.detail);
    }
    println!();

    let mut changed = false;
    for need in &needs {
        match &need.fix {
            // smith cannot run someone else's installer on the user's behalf,
            // so the honest move is the exact command and a pause. Continuing
            // to the next item afterwards, because the remaining ones may well
            // be installable and refusing them too would be punitive.
            Fix::Manual { command } => {
                println!("{} — smith cannot install this one.", need.name);
                println!("  Run:  {command}");
                println!("  Then re-run this section and smith will pick it up.\n");
            }
            Fix::Auto { .. } => {
                let approved = Confirm::with_theme(theme)
                    .with_prompt(format!("{}?", need.prompt()))
                    .default(true)
                    .interact_opt()?
                    .unwrap_or(false);
                if !approved {
                    println!("  skipped\n");
                    continue;
                }
                match install(config, need).await {
                    Ok(()) => changed = true,
                    Err(e) => {
                        println!("  failed: {e}\n");
                        // Later items may depend on this one (the gateway needs
                        // Node), so stopping is kinder than a cascade of
                        // failures that all trace back here.
                        break;
                    }
                }
            }
        }
    }

    if changed {
        // Saved here rather than by the caller's `if changed { save() }`,
        // because what was installed is on disk whether or not the rest of the
        // section goes well, and a config that cannot see it is an install
        // nothing will ever use. The caller saves again; that is idempotent.
        config.save()?;
        println!("Recorded in the config.");
    }
    Ok(changed)
}

async fn install(config: &mut Config, need: &Need) -> Result<(), String> {
    let mut out = Vec::new();
    let result = preflight::apply(config, need, &mut out).await;
    for line in out.iter().filter(|l| !l.is_empty()) {
        println!("  {line}");
    }
    if result.is_ok() {
        println!();
    }
    result
}
