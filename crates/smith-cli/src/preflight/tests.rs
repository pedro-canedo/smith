use super::*;
use crate::node_runtime::parse_node_major;

fn config_with(provider: Option<&str>, fallback: &[&str]) -> Config {
    let mut config = Config::default();
    config.general.provider = provider.map(str::to_string);
    config.fallback.providers = fallback.iter().map(|s| s.to_string()).collect();
    config
}

#[test]
fn a_fallback_entry_counts_as_in_play() {
    // The case this whole module exists for: 9router is not the primary, so
    // `smith setup` never walked its section, but a turn will reach for it the
    // moment the primary runs out of quota.
    let config = config_with(Some("openrouter"), &["openrouter", "9router"]);
    assert!(in_play(&config, ProviderKind::NineRouter));
    assert!(!in_play(&config, ProviderKind::Ollama));
}

#[test]
fn a_provider_nobody_named_is_not_in_play() {
    let config = config_with(Some("anthropic"), &[]);
    assert!(!in_play(&config, ProviderKind::NineRouter));
    assert!(!in_play(&config, ProviderKind::Ollama));
}

#[tokio::test]
async fn an_anthropic_config_is_never_told_about_node() {
    // Pinning a non-browser backend keeps the Chromium tier out of the survey
    // too, so this config needs nothing at all.
    let mut config = config_with(Some("anthropic"), &[]);
    config.search.backend = Some("tavily".into());
    assert!(survey(&config).await.is_empty());
}

#[test]
fn node_versions_parse_the_way_nodejs_org_writes_them() {
    assert_eq!(parse_node_major("v22.22.3\n"), Some(22));
    assert_eq!(parse_node_major("24.19.0"), Some(24));
    assert_eq!(parse_node_major("v25.0.0-nightly20260101abc"), Some(25));
    assert_eq!(parse_node_major("v18.0.0-rc.1"), Some(18));
    assert_eq!(parse_node_major(""), None);
    assert_eq!(parse_node_major("not a version"), None);
}

#[test]
fn the_minimum_is_the_gateways_own_floor_not_the_pinned_download() {
    // 9router's package.json says `{"node": ">=18.0.0"}`. If these two ever
    // collapse into one number, a machine with a working Node 22 starts
    // downloading 50 MB again for no reason.
    assert!(MIN_NODE_MAJOR < parse_node_major(NODE_VERSION).unwrap());
}

#[test]
fn a_manual_need_says_so_rather_than_offering_to_do_it() {
    let need = Need {
        key: NeedKey::Ollama,
        name: "ollama",
        detail: "nothing is answering".into(),
        fix: Fix::Manual {
            command: "brew install ollama".into(),
        },
    };
    assert!(!need.is_auto());
    assert!(need.prompt().contains("brew install ollama"));
}

#[test]
fn an_auto_need_puts_its_cost_in_the_prompt() {
    // The number is the whole point of the prompt: "download Node?" and
    // "download Node (~50 MB, once)?" are different questions.
    let need = Need {
        key: NeedKey::Node,
        name: "node",
        detail: "none found".into(),
        fix: Fix::Auto {
            action: "Download a private Node".into(),
            cost: Some("~50 MB, once".into()),
        },
    };
    assert!(need.is_auto());
    assert!(need.prompt().contains("~50 MB, once"));
}

#[tokio::test]
async fn applying_a_manual_need_refuses_instead_of_pretending() {
    let mut config = Config::default();
    let need = Need {
        key: NeedKey::Ollama,
        name: "ollama",
        detail: String::new(),
        fix: Fix::Manual {
            command: "brew install ollama".into(),
        },
    };
    let mut out = Vec::new();
    assert!(apply(&mut config, &need, &mut out).await.is_err());
}
