//! The endpoints the console's navigation can open.
//!
//! The left rail offers one-click access to the dashboards behind a session —
//! the 9Router gateway, the Ollama daemon, an OpenRouter account page. Those
//! URLs are *configuration*, not constants: a gateway on a non-default port
//! or behind a path prefix is ordinary, and a page that hardcoded
//! `localhost:20128` would be quietly wrong for exactly the users who
//! customised anything. So the server resolves them once at startup from the
//! same `Config` the provider stack was built from, and the browser renders
//! what it is given.
//!
//! Two rules keep the list honest:
//!
//! - **Nothing unconfigured is offered.** A link to an Ollama nobody runs is
//!   a dead end that looks like a feature. An entry appears only when its
//!   provider is serving this session, is named in `[fallback] providers`, or
//!   carries settings of its own.
//! - **Off-machine links are marked.** `external` drives `rel="noreferrer"`
//!   in the page: the console's own URL carries the session token in its
//!   query string, and a navigation is the one thing that could hand it to
//!   another origin.

use serde::Serialize;
use smith_config::{Config, DEFAULT_NINEROUTER_BASE_URL, DEFAULT_OLLAMA_BASE_URL};

use crate::orchestrator::ProviderKind;

/// Where a link sits in the rail. Providers first, then services the tools
/// use, then documentation — the order the console renders them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkGroup {
    Provider,
    Service,
    Reference,
}

/// One navigable endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConsoleLink {
    /// Stable key the page uses to pick an icon.
    pub id: String,
    pub label: String,
    pub url: String,
    /// The one line under the label: a port for something local, a domain for
    /// something remote.
    pub detail: String,
    pub group: LinkGroup,
    /// The URL leaves this machine.
    pub external: bool,
    /// This provider is the one serving the session right now.
    pub active: bool,
}

/// Every endpoint worth offering for this configuration.
pub fn links_for(config: &Config, active: ProviderKind) -> Vec<ConsoleLink> {
    let mut links = Vec::new();
    let in_fallback =
        |kind: ProviderKind| config.fallback.providers.iter().any(|p| p == kind.label());
    let offer =
        |kind: ProviderKind, configured: bool| kind == active || configured || in_fallback(kind);

    // 9Router: a local gateway with a real dashboard, and the one link most
    // worth a click — adding a provider there is what fixes an empty catalogue.
    if offer(
        ProviderKind::NineRouter,
        config.nine_router.api_key.is_some() || config.nine_router.base_url.is_some(),
    ) {
        let url = dashboard_of(
            config
                .nine_router
                .base_url
                .as_deref()
                .unwrap_or(DEFAULT_NINEROUTER_BASE_URL),
        );
        links.push(ConsoleLink {
            detail: authority_of(&url),
            id: "9router".into(),
            label: "9Router".into(),
            url,
            group: LinkGroup::Provider,
            external: false,
            active: active == ProviderKind::NineRouter,
        });
    }

    if offer(ProviderKind::Ollama, config.ollama.base_url.is_some()) {
        let url = dashboard_of(
            config
                .ollama
                .base_url
                .as_deref()
                .unwrap_or(DEFAULT_OLLAMA_BASE_URL),
        );
        links.push(ConsoleLink {
            detail: authority_of(&url),
            id: "ollama".into(),
            label: "Ollama".into(),
            url,
            group: LinkGroup::Provider,
            external: false,
            active: active == ProviderKind::Ollama,
        });
    }

    if offer(
        ProviderKind::Openrouter,
        config.openrouter.api_key.is_some(),
    ) {
        links.push(ConsoleLink {
            id: "openrouter".into(),
            label: "OpenRouter".into(),
            // The activity page, not the marketing home: what a running
            // session makes you want to check is spend and rate limits.
            url: "https://openrouter.ai/activity".into(),
            detail: "openrouter.ai".into(),
            group: LinkGroup::Provider,
            external: true,
            active: active == ProviderKind::Openrouter,
        });
    }

    if offer(ProviderKind::Anthropic, config.anthropic.api_key.is_some()) {
        links.push(ConsoleLink {
            id: "anthropic".into(),
            label: "Anthropic".into(),
            url: "https://console.anthropic.com/settings/usage".into(),
            detail: "console.anthropic.com".into(),
            group: LinkGroup::Provider,
            external: true,
            active: active == ProviderKind::Anthropic,
        });
    }

    if offer(ProviderKind::Openai, config.openai.api_key.is_some()) {
        links.push(ConsoleLink {
            id: "openai".into(),
            label: "OpenAI".into(),
            url: "https://platform.openai.com/usage".into(),
            detail: "platform.openai.com".into(),
            group: LinkGroup::Provider,
            external: true,
            active: active == ProviderKind::Openai,
        });
    }

    // A SearXNG instance is the user's own machine and the first tier
    // `web_search` tries — worth reaching when a search comes back empty.
    if let Some(searxng) = config.search.searxng_url.as_deref() {
        let url = searxng.trim_end_matches('/').to_string();
        links.push(ConsoleLink {
            detail: authority_of(&url),
            id: "searxng".into(),
            label: "SearXNG".into(),
            url,
            group: LinkGroup::Service,
            external: !is_loopback(searxng),
            active: false,
        });
    }

    links.push(ConsoleLink {
        id: "repo".into(),
        label: "smith on GitHub".into(),
        url: "https://github.com/pedro-canedo/smith".into(),
        detail: "issues · releases".into(),
        group: LinkGroup::Reference,
        external: true,
        active: false,
    });

    links
}

/// A gateway's OpenAI-compatible base URL is not a page — `…/v1` answers
/// JSON. Its dashboard is the same URL without that suffix, and *only* that
/// suffix: a gateway mounted under a path prefix keeps the prefix.
fn dashboard_of(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

/// The `host:port` of a URL, for the line under a local link. Parsed by hand
/// rather than by pulling in a URL crate for one substring, and falling back
/// to the whole string means a shape this does not recognise still renders
/// something true.
fn authority_of(url: &str) -> String {
    url.split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

fn is_loopback(url: &str) -> bool {
    let authority = authority_of(url);
    let host = authority
        .rsplit_once(':')
        .map_or(authority.as_str(), |(h, _)| h);
    host == "localhost" || host == "127.0.0.1" || host == "[::1]"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(links: &[ConsoleLink]) -> Vec<&str> {
        links.iter().map(|l| l.id.as_str()).collect()
    }

    #[test]
    fn an_unconfigured_provider_is_not_offered() {
        let links = links_for(&Config::default(), ProviderKind::Anthropic);
        // Anthropic is serving the session, so it is offered despite the key
        // living in the environment rather than the file; nothing else is.
        assert_eq!(ids(&links), vec!["anthropic", "repo"]);
    }

    #[test]
    fn a_provider_configured_but_idle_is_still_offered() {
        let mut config = Config::default();
        config.ollama.base_url = Some("http://127.0.0.1:11434/v1".into());
        let links = links_for(&config, ProviderKind::Anthropic);
        assert!(ids(&links).contains(&"ollama"));
    }

    #[test]
    fn a_fallback_entry_is_offered_before_it_ever_serves_a_turn() {
        let mut config = Config::default();
        config.fallback.providers = vec!["9router".into()];
        let links = links_for(&config, ProviderKind::Anthropic);
        assert!(ids(&links).contains(&"9router"));
    }

    #[test]
    fn only_the_serving_provider_is_marked_active() {
        let mut config = Config::default();
        config.fallback.providers = vec!["9router".into(), "openrouter".into()];
        let links = links_for(&config, ProviderKind::NineRouter);
        let active: Vec<&str> = links
            .iter()
            .filter(|l| l.active)
            .map(|l| l.id.as_str())
            .collect();
        assert_eq!(active, vec!["9router"]);
    }

    #[test]
    fn a_gateway_link_points_at_the_dashboard_not_the_api_root() {
        let mut config = Config::default();
        config.nine_router.base_url = Some("http://localhost:9999/v1".into());
        let links = links_for(&config, ProviderKind::NineRouter);
        let gateway = links.iter().find(|l| l.id == "9router").unwrap();
        assert_eq!(gateway.url, "http://localhost:9999");
        assert_eq!(gateway.detail, "localhost:9999");
    }

    #[test]
    fn a_gateway_behind_a_path_prefix_keeps_the_prefix() {
        assert_eq!(dashboard_of("http://box.lan/gw/v1"), "http://box.lan/gw");
        assert_eq!(dashboard_of("http://box.lan/gw/v1/"), "http://box.lan/gw");
        // Nothing to strip is not an error — some deployments are already
        // pointed at a root.
        assert_eq!(dashboard_of("http://box.lan/gw"), "http://box.lan/gw");
    }

    #[test]
    fn a_remote_searxng_is_external_and_a_local_one_is_not() {
        let mut config = Config::default();
        config.search.searxng_url = Some("http://127.0.0.1:8888/".into());
        let local = links_for(&config, ProviderKind::Anthropic);
        assert!(!local.iter().find(|l| l.id == "searxng").unwrap().external);

        config.search.searxng_url = Some("https://searx.example.org".into());
        let remote = links_for(&config, ProviderKind::Anthropic);
        assert!(remote.iter().find(|l| l.id == "searxng").unwrap().external);
    }

    #[test]
    fn every_off_machine_link_is_marked_external() {
        // The page hangs `rel="noreferrer"` on this flag, so a link that
        // leaves the machine while claiming to be local would leak the
        // console's URL — the one place the session token is written down.
        let mut config = Config::default();
        config.openrouter.api_key = Some("k".into());
        config.openai.api_key = Some("k".into());
        config.anthropic.api_key = Some("k".into());
        for link in links_for(&config, ProviderKind::Openrouter) {
            let off_machine = !link.url.starts_with("http://127.0.0.1")
                && !link.url.starts_with("http://localhost");
            assert_eq!(
                link.external, off_machine,
                "{} is marked external={} but its URL is {}",
                link.id, link.external, link.url
            );
        }
    }
}
