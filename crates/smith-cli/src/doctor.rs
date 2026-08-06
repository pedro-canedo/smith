//! `smith doctor` — check the things that silently make smith not work, and
//! say exactly how to fix each one.
//!
//! The rule every check here obeys: **a failure without a remedy is a bug.**
//! "Ollama not reachable" tells someone nothing they didn't already know from
//! the error they came here about; "Ollama is installed but the daemon isn't
//! answering — run `ollama serve`" ends the investigation. `Check::remedy` is
//! therefore mandatory for anything that is not `Ok`, and a test enforces it
//! across a whole real run.
//!
//! Secrets never appear in output. Checks report *where* a key came from and
//! never what it is, and the whole report is passed through
//! `smith_core::Redactor` on the way out as a backstop — defence in depth, not
//! the primary mechanism, because the primary mechanism is not putting the key
//! into the string in the first place.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use smith_config::{Config, McpServerConfig};
use smith_core::Redactor;

use crate::orchestrator::ProviderKind;
use crate::runtime::{self, BrowserSource};

/// Ceiling on any single network probe. `doctor` is a diagnostic; one
/// unreachable endpoint must not make it look hung.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// An MCP server gets longer — some of them are a Node process starting cold.
const MCP_TIMEOUT: Duration = Duration::from_secs(20);

/// Exit code when at least one check FAILed, so `smith doctor` is usable as a
/// CI gate. Matches the `2` the rest of the CLI uses for a config error.
pub const EXIT_FAILED: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Ok,
    /// Works, but degraded or about to stop working.
    Warn,
    /// Broken: something the user asked smith to do cannot happen.
    Fail,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    /// What was found. One line, present tense.
    pub detail: String,
    /// What to do about it. Required whenever `status` is not `Ok`.
    pub remedy: Option<String>,
}

impl Check {
    pub fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: Status::Ok,
            detail: detail.into(),
            remedy: None,
        }
    }

    /// `warn` and `fail` take their remedy by value rather than as an
    /// `Option`, so "forgot the remedy" is a compile error rather than a
    /// disappointing line of output.
    pub fn warn(
        name: impl Into<String>,
        detail: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: Status::Warn,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }

    pub fn fail(
        name: impl Into<String>,
        detail: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn push(&mut self, check: Check) {
        self.checks.push(check);
    }

    pub fn worst(&self) -> Status {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Ok)
    }

    /// Non-zero exactly when something FAILed. A `Warn` deliberately is not a
    /// failure: `smith doctor` in CI should go red for "this cannot work",
    /// not for "you have no browser for web_search".
    pub fn exit_code(&self) -> u8 {
        if self.worst() == Status::Fail {
            EXIT_FAILED
        } else {
            0
        }
    }

    /// The whole report as text. `redactor` scrubs any secret that reached a
    /// detail string despite the checks' own care.
    pub fn render(&self, redactor: &Redactor) -> String {
        let mut out = String::new();
        for check in &self.checks {
            let _ = writeln!(
                out,
                "{:<4} {:<22} {}",
                check.status.label(),
                check.name,
                check.detail
            );
            if let Some(remedy) = &check.remedy {
                for line in remedy.lines() {
                    let _ = writeln!(out, "       -> {line}");
                }
            }
        }

        let fails = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count();
        let warns = self
            .checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count();
        let total = self.checks.len();
        let _ = writeln!(out);
        let _ = match (fails, warns) {
            (0, 0) => writeln!(out, "{total} checks, all OK."),
            (0, w) => writeln!(out, "{total} checks, {w} warning(s), no failures."),
            (f, w) => writeln!(out, "{total} checks, {f} failure(s), {w} warning(s)."),
        };

        redactor.redact(&out).into_owned()
    }
}

/// Runs every check and prints the report. The only entry point `main` needs.
pub async fn run() -> u8 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let config = Config::load_layered(&cwd).unwrap_or_default();
    let report = diagnose(&cwd, &config).await;
    print!("{}", report.render(&redactor_for(&config)));
    report.exit_code()
}

/// Every credential this process can see, so none can reach stdout.
///
/// The one in `orchestrator` — no longer a hand-kept mirror. The mirror
/// existed because that function was private, and the moment a new provider
/// key joined one list but not the other, a doctor report would have printed
/// it. Deleting the copy is what makes that impossible.
fn redactor_for(config: &Config) -> Redactor {
    crate::orchestrator::secret_redactor(config)
}

/// The full check list, in the order it prints.
pub async fn diagnose(cwd: &Path, config: &Config) -> Report {
    let mut report = Report::default();

    report.push(check_config_layers(cwd));
    let provider = resolve_provider(config);
    report.push(check_provider_selected(config, provider));

    let key = resolve_api_key(config, provider);
    report.push(check_api_key(provider, &key));
    report.push(check_provider_reachable(config, provider, key.value()).await);
    if provider == ProviderKind::Openrouter {
        if let Some(key) = key.value() {
            report.push(check_openrouter_quota(config, key).await);
        }
    }

    // Always run, not only when Ollama is the configured provider: it is the
    // zero-cost path someone falls back to, and "can I use it?" is worth
    // answering before they need to.
    report.push(check_ollama(config).await);
    // Only when 9router is actually in play (primary or fallback entry):
    // unlike Ollama it is not a thing most machines have, and a permanent
    // warn on every clean install would train people to ignore the report.
    if provider == ProviderKind::NineRouter
        || config.fallback.providers.iter().any(|p| p == "9router")
    {
        report.push(check_ninerouter(config).await);
    }
    report.push(check_browser(config).await);

    let (writable, schema) = check_project_dir(cwd);
    report.push(writable);
    report.push(schema);

    report.extend_mcp(config).await;
    report
}

impl Report {
    async fn extend_mcp(&mut self, config: &Config) {
        if config.mcp_servers.is_empty() {
            self.push(Check::ok("mcp", "no MCP servers configured"));
            return;
        }
        for server in &config.mcp_servers {
            self.push(check_mcp_server(server).await);
        }
    }
}

/// Whether the 9router gateway is installed and answering, with the remedy
/// for each way it can not be.
async fn check_ninerouter(config: &Config) -> Check {
    let base_url = config
        .nine_router
        .base_url
        .clone()
        .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());

    if crate::node_runtime::ninerouter_healthy(&base_url).await {
        return Check::ok("9router", format!("gateway answering on {base_url}"));
    }

    let node = crate::node_runtime::find_node(&config.runtime);
    let installed = config
        .runtime
        .ninerouter_dir
        .as_deref()
        .map(|dir| crate::node_runtime::ninerouter_cli(std::path::Path::new(dir)).is_file())
        .unwrap_or(false);

    match (node.is_some(), installed) {
        (false, _) => Check::fail(
            "9router",
            "no Node runtime for the gateway",
            "Run `smith setup` and pick the 9Router option — it downloads a private Node \
             into ~/.smith/runtime.",
        ),
        (true, false) => Check::fail(
            "9router",
            "the gateway package is not installed",
            "Run `smith setup` and pick the 9Router option.",
        ),
        (true, true) => Check::warn(
            "9router",
            format!("installed but not answering on {base_url}"),
            "smith starts it automatically at session start; if that keeps failing, run it by \
             hand to see why: `node <ninerouter_dir>/node_modules/9router/cli.js`.",
        ),
    }
}

/// How much of the OpenRouter quota is left — the question a rate-limited
/// user actually runs doctor to answer. `GET /api/v1/key` reports usage,
/// limit and free-tier status for the key making the request.
async fn check_openrouter_quota(config: &Config, key: &str) -> Check {
    let base = config
        .openrouter
        .base_url
        .clone()
        .unwrap_or_else(|| smith_config::DEFAULT_OPENROUTER_BASE_URL.to_string());
    let url = format!("{}/key", base.trim_end_matches('/'));

    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return Check::warn(
            "openrouter quota",
            "could not build an HTTP client to ask with",
            "This does not affect normal runs.",
        );
    };

    let response = match client.get(&url).bearer_auth(key).send().await {
        Ok(r) => r,
        Err(e) => {
            return Check::warn(
                "openrouter quota",
                format!("{url} unreachable: {e}"),
                "Quota could not be checked; requests may still work.",
            )
        }
    };
    if !response.status().is_success() {
        return Check::warn(
            "openrouter quota",
            format!("{url} answered {}", response.status()),
            "Quota could not be checked; requests may still work.",
        );
    }
    let Ok(body) = response.json::<serde_json::Value>().await else {
        return Check::warn(
            "openrouter quota",
            "unparseable /key response",
            "Quota could not be checked; requests may still work.",
        );
    };

    Check::ok("openrouter quota", describe_openrouter_key(&body))
}

/// Renders `/api/v1/key`'s payload as one factual line. Pure, so the shapes
/// OpenRouter actually returns can be pinned in tests.
fn describe_openrouter_key(body: &serde_json::Value) -> String {
    let data = body.get("data").unwrap_or(body);
    let usage = data.get("usage").and_then(|v| v.as_f64());
    let limit = data.get("limit").and_then(|v| v.as_f64());
    let free_tier = data
        .get("is_free_tier")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut parts: Vec<String> = Vec::new();
    match (usage, limit) {
        (Some(u), Some(l)) => parts.push(format!("${u:.2} used of ${l:.2} credit limit")),
        (Some(u), None) => parts.push(format!("${u:.2} used, no credit limit set")),
        _ => parts.push("usage not reported".to_string()),
    }
    if free_tier {
        parts.push(
            "free tier: 20 req/min and 50 free-model requests/day (1000/day after a one-time \
             $10 top-up)"
                .to_string(),
        );
    }
    parts.join(" — ")
}

// ---------------------------------------------------------------------------
// config
// ---------------------------------------------------------------------------

fn check_config_layers(cwd: &Path) -> Check {
    let global = smith_config::config_path().ok();
    let project = smith_config::project_config_path(cwd);

    let mut layers = Vec::new();
    let mut broken = Vec::new();

    for (label, path) in [("global", global.as_ref()), ("project", Some(&project))] {
        let Some(path) = path else { continue };
        match Config::check_path(path) {
            Ok(true) => layers.push(format!("{label} ({})", path.display())),
            Ok(false) => {}
            Err(e) => broken.push(format!("{} is not valid: {e}", path.display())),
        }
    }

    if !broken.is_empty() {
        return Check::fail(
            "config",
            broken.join("; "),
            // The specific danger: `load_layered`'s `unwrap_or_default` makes
            // a broken layer silently ignored, so smith runs with the *wrong*
            // settings rather than refusing — exactly the situation someone
            // runs doctor to understand.
            "Fix the TOML syntax in the file named above, or move it aside. Until it parses, \
             smith falls back to defaults and quietly ignores everything in it.",
        );
    }

    if layers.is_empty() {
        return Check::warn(
            "config",
            format!(
                "no config file yet (looked for {} and {})",
                global
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.smith/config.toml".into()),
                project.display()
            ),
            "Run `smith setup` to create one, or export ANTHROPIC_API_KEY / OPENAI_API_KEY and \
             run smith directly.",
        );
    }

    Check::ok(
        "config",
        format!(
            "layers in effect: {} (project wins field by field)",
            layers.join(", ")
        ),
    )
}

fn resolve_provider(config: &Config) -> ProviderKind {
    config
        .general
        .provider
        .as_deref()
        .and_then(ProviderKind::from_config_str)
        .unwrap_or(ProviderKind::Anthropic)
}

fn check_provider_selected(config: &Config, provider: ProviderKind) -> Check {
    let model = config
        .general
        .model
        .clone()
        .unwrap_or_else(|| provider.default_model().to_string());
    match config.general.provider.as_deref() {
        Some(name) if ProviderKind::from_config_str(name).is_none() => Check::fail(
            "provider",
            format!("config names an unknown provider `{name}`"),
            "Set `provider` under [general] to one of: anthropic, openai, openrouter, 9router, ollama — or re-run \
             `smith setup`. Until then smith silently falls back to anthropic.",
        ),
        Some(_) => Check::ok("provider", format!("{} / {model}", provider.label())),
        None => Check::warn(
            "provider",
            format!(
                "none configured; defaulting to {} / {model}",
                provider.label()
            ),
            "Run `smith setup` to pick a provider and model explicitly.",
        ),
    }
}

// ---------------------------------------------------------------------------
// api key
// ---------------------------------------------------------------------------

/// Where a key was found — never the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// Found in this environment variable.
    Env(&'static str),
    /// Found in a config layer.
    ConfigFile,
    /// This provider needs no key.
    NotNeeded,
    /// Nothing anywhere.
    Missing,
}

/// A resolved key plus its provenance.
///
/// The value is deliberately private: reaching for it takes a conscious
/// `value()` call, which is the moment someone notices they are about to
/// handle a secret.
#[derive(Debug, Clone)]
pub struct ResolvedKey {
    source: KeySource,
    value: Option<String>,
}

impl ResolvedKey {
    pub fn source(&self) -> &KeySource {
        &self.source
    }

    fn value(&self) -> Option<&str> {
        self.value.as_deref()
    }
}

/// Resolves a key in the same precedence `orchestrator::build_provider` uses —
/// environment first, config second. Reporting a different one than the code
/// actually uses would make doctor worse than useless.
pub fn resolve_api_key(config: &Config, provider: ProviderKind) -> ResolvedKey {
    let (var, from_config) = match provider {
        ProviderKind::Anthropic => ("ANTHROPIC_API_KEY", config.anthropic.api_key.clone()),
        ProviderKind::Openai => ("OPENAI_API_KEY", config.openai.api_key.clone()),
        ProviderKind::Openrouter => ("OPENROUTER_API_KEY", config.openrouter.api_key.clone()),
        ProviderKind::NineRouter => ("NINEROUTER_API_KEY", config.nine_router.api_key.clone()),
        // A local daemon has no credential; "no key" is the healthy state.
        ProviderKind::Ollama => {
            return ResolvedKey {
                source: KeySource::NotNeeded,
                value: None,
            }
        }
    };
    if let Some(value) = std::env::var(var).ok().filter(|v| !v.trim().is_empty()) {
        return ResolvedKey {
            source: KeySource::Env(var),
            value: Some(value),
        };
    }
    if let Some(value) = from_config.filter(|v| !v.trim().is_empty()) {
        return ResolvedKey {
            source: KeySource::ConfigFile,
            value: Some(value),
        };
    }
    ResolvedKey {
        source: KeySource::Missing,
        value: None,
    }
}

pub fn check_api_key(provider: ProviderKind, key: &ResolvedKey) -> Check {
    match key.source() {
        KeySource::NotNeeded => Check::ok("api key", "ollama needs no API key"),
        // Says where, never what: the value is not interpolated anywhere in
        // this function.
        KeySource::Env(var) => Check::ok("api key", format!("found in ${var}")),
        KeySource::ConfigFile => Check::ok(
            "api key",
            format!("found in config ([{}] api_key)", provider.label()),
        ),
        KeySource::Missing => {
            let var = match provider {
                ProviderKind::Anthropic => "ANTHROPIC_API_KEY",
                ProviderKind::Openai => "OPENAI_API_KEY",
                ProviderKind::Openrouter => "OPENROUTER_API_KEY",
                ProviderKind::NineRouter => "NINEROUTER_API_KEY",
                ProviderKind::Ollama => unreachable!("ollama resolves to NotNeeded above"),
            };
            Check::fail(
                "api key",
                format!("no key for {}", provider.label()),
                format!("Run `smith setup`, or export {var}=<your key>."),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// provider reachability
// ---------------------------------------------------------------------------

async fn check_provider_reachable(
    config: &Config,
    provider: ProviderKind,
    key: Option<&str>,
) -> Check {
    let Ok(client) = reqwest::Client::builder().timeout(PROBE_TIMEOUT).build() else {
        return Check::warn(
            "provider reachable",
            "could not build an HTTP client to test with",
            "This is a smith bug — please report it. It does not affect normal runs.",
        );
    };

    let (url, request) = match provider {
        ProviderKind::Anthropic => {
            let url = "https://api.anthropic.com/v1/models".to_string();
            let mut req = client.get(&url).header("anthropic-version", "2023-06-01");
            if let Some(key) = key {
                req = req.header("x-api-key", key);
            }
            (url, req)
        }
        ProviderKind::Openai => {
            let url = "https://api.openai.com/v1/models".to_string();
            let mut req = client.get(&url);
            if let Some(key) = key {
                req = req.bearer_auth(key);
            }
            (url, req)
        }
        ProviderKind::Openrouter => {
            let base = config
                .openrouter
                .base_url
                .clone()
                .unwrap_or_else(|| smith_config::DEFAULT_OPENROUTER_BASE_URL.to_string());
            let url = format!("{}/models", base.trim_end_matches('/'));
            let mut req = client.get(&url);
            if let Some(key) = key {
                req = req.bearer_auth(key);
            }
            (url, req)
        }
        ProviderKind::NineRouter => {
            let base = config
                .nine_router
                .base_url
                .clone()
                .unwrap_or_else(|| smith_config::DEFAULT_NINEROUTER_BASE_URL.to_string());
            let url = format!("{}/models", base.trim_end_matches('/'));
            let mut req = client.get(&url);
            if let Some(key) = key {
                req = req.bearer_auth(key);
            }
            (url, req)
        }
        ProviderKind::Ollama => {
            let base = config
                .ollama
                .base_url
                .clone()
                .unwrap_or_else(|| smith_config::DEFAULT_OLLAMA_BASE_URL.to_string());
            let url = format!("{}/models", base.trim_end_matches('/'));
            let req = client.get(&url);
            (url, req)
        }
    };

    match request.send().await {
        Ok(response) if response.status().is_success() => {
            Check::ok("provider reachable", format!("{url} answered 200"))
        }
        // A 401/403 is a *reachable* endpoint refusing the credential, which
        // is a different problem from a network one and needs a different fix.
        //
        // And "no key was sent" is a third thing again: telling someone their
        // key is expired when they never had one sends them to regenerate a
        // credential that does not exist. The `api key` check above already
        // said the real thing, so this defers to it rather than competing.
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) && key.is_none() => {
            Check::warn(
                "provider reachable",
                format!(
                    "{url} is reachable, but refused an unauthenticated request ({})",
                    response.status()
                ),
                "Expected — smith had no key to send. Fix the `api key` check above and this \
                 clears up with it.",
            )
        }
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) => Check::fail(
            "provider reachable",
            format!("{url} rejected the key ({})", response.status()),
            "The key smith is using is wrong, expired, or lacks access. Replace it with \
             `smith setup`, or fix the value in the source the `api key` check named above. \
             Regenerate it at the provider's console rather than pasting it anywhere to test.",
        ),
        Ok(response) => Check::warn(
            "provider reachable",
            format!("{url} answered {}", response.status()),
            "The endpoint is reachable but unhappy. If it persists, check the provider's status \
             page — smith retries transient errors on its own.",
        ),
        Err(e) if matches!(provider, ProviderKind::Ollama) => Check::fail(
            "provider reachable",
            format!("cannot reach {url}: {e}"),
            "Ollama is the configured provider but its daemon isn't answering. Start it with \
             `ollama serve`, or point [ollama] base_url at the right host.",
        ),
        Err(e) => Check::fail(
            "provider reachable",
            format!("cannot reach {url}: {e}"),
            "Check your network, VPN and proxy settings. If you are offline, `smith setup` can \
             switch you to ollama and run models locally.",
        ),
    }
}

// ---------------------------------------------------------------------------
// ollama
// ---------------------------------------------------------------------------

/// The exact install command for this platform.
///
/// Guidance only, and deliberately so: installing Ollama registers a
/// background daemon, edits `PATH`, and on Linux wants `sudo`. Dropping a
/// browser into a cache directory affects nothing outside that directory;
/// this reaches into the system, and it stays the user's call.
pub fn ollama_install_command(os: &str) -> &'static str {
    match os {
        "macos" => "brew install ollama   (or download from https://ollama.com/download)",
        "linux" => {
            "curl -fsSL https://ollama.com/install.sh | sh   (needs sudo; registers a systemd service)"
        }
        "windows" => "winget install Ollama.Ollama   (or download from https://ollama.com/download)",
        _ => "See https://ollama.com/download for your platform",
    }
}

async fn check_ollama(config: &Config) -> Check {
    let installed = tokio::process::Command::new("ollama")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()
        .filter(|o| o.status.success());

    let base = config
        .ollama
        .base_url
        .clone()
        .unwrap_or_else(|| smith_config::DEFAULT_OLLAMA_BASE_URL.to_string());
    let reachable = ollama_daemon_reachable().await;
    let configured = matches!(resolve_provider(config), ProviderKind::Ollama);

    match (installed, reachable) {
        (Some(output), true) => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Check::ok(
                "ollama",
                format!(
                    "installed and answering ({})",
                    if version.is_empty() { base } else { version }
                ),
            )
        }
        (Some(_), false) => Check::warn(
            "ollama",
            "installed, but the daemon isn't answering",
            "Start it with `ollama serve` (it stays in the foreground), or start the service your \
             installer registered. Then re-run `smith doctor`.",
        ),
        // No binary but something answering: a remote or containerised daemon.
        // Entirely valid, and not a problem.
        (None, true) => Check::ok(
            "ollama",
            format!("no local binary, but a daemon is answering at {base}"),
        ),
        (None, false) if configured => Check::fail(
            "ollama",
            "configured as the provider, but not installed",
            format!(
                "Install it, then `ollama pull <model>`:\n{}\nsmith will not install it for you: \
                 it registers a background daemon and changes PATH.",
                ollama_install_command(std::env::consts::OS)
            ),
        ),
        (None, false) => Check::ok(
            "ollama",
            "not installed (not needed — it isn't the configured provider)",
        ),
    }
}

async fn ollama_daemon_reachable() -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
    else {
        return false;
    };
    client
        .get(format!("{}/api/tags", smith_config::OLLAMA_HOST))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

// ---------------------------------------------------------------------------
// browser
// ---------------------------------------------------------------------------

async fn check_browser(config: &Config) -> Check {
    let Some(found) = runtime::find_browser(&config.runtime) else {
        return Check::warn(
            "web_search browser",
            "no Chromium-family browser found",
            "Run `smith setup` and accept the browser download (~100 MB), or install Chrome or \
             Chromium yourself. This is only a fallback: web_search's main free tier is plain \
             HTTP, and the browser exists to reach the same endpoint from a host where that is \
             intercepted or fingerprinted.",
        );
    };

    let origin = match found.source {
        BrowserSource::Env(var) => format!("${var}"),
        BrowserSource::Provisioned => "provisioned by smith setup".to_string(),
        BrowserSource::System => "found on PATH".to_string(),
    };

    match runtime::probe_version(&found.path).await {
        Ok(version) => Check::ok(
            "web_search browser",
            format!("{version} ({origin}: {})", found.path.display()),
        ),
        Err(e) => {
            let remedy = match found.source {
                BrowserSource::Env(var) => format!(
                    "${var} points at something that does not run. Fix or unset it — smith \
                     honours it verbatim and will not silently substitute another browser."
                ),
                BrowserSource::Provisioned => {
                    "The provisioned browser is broken or was deleted. Re-run `smith setup` — it \
                     reinstalls rather than trusting whatever is there."
                        .to_string()
                }
                BrowserSource::System => {
                    "The browser on PATH does not run. Reinstall it, set SMITH_CHROMIUM_PATH to a \
                     working one, or let `smith setup` provision smith's own."
                        .to_string()
                }
            };
            Check::warn(
                "web_search browser",
                format!("{} does not run: {e}", found.path.display()),
                remedy,
            )
        }
    }
}

// ---------------------------------------------------------------------------
// project directory and session store
// ---------------------------------------------------------------------------

/// `.smith/` writability and the session DB's schema version.
fn check_project_dir(cwd: &Path) -> (Check, Check) {
    let dir = cwd.join(".smith");
    match probe_writable(&dir) {
        Ok(()) => (
            Check::ok(".smith dir", format!("{} is writable", dir.display())),
            check_session_db(cwd),
        ),
        Err(e) => (
            Check::fail(
                ".smith dir",
                format!("cannot write to {}: {e}", dir.display()),
                format!(
                    "smith keeps session history, checkpoints and per-project config here. Fix \
                     the permissions (`chmod u+rwx {}`), or run smith from a directory you own.",
                    dir.display()
                ),
            ),
            // No point opening a database under a directory that cannot be
            // written: a second failure here has the same single cause and
            // would only bury the real one.
            Check::warn(
                "session db",
                "not checked — .smith is not writable",
                "Fix the .smith directory above, then re-run `smith doctor`.",
            ),
        ),
    }
}

/// Whether `dir` can be written to, **without creating it**.
///
/// A diagnostic that materialises state is a bad diagnostic: an earlier
/// version of this created `.smith/` wherever it was run, which is enough to
/// make `MemoryScope::discover` treat that directory as a project root from
/// then on. Running `smith doctor` somewhere must not change what running
/// `smith` there does.
///
/// So when `.smith` does not exist yet, the question becomes "could it be
/// created?", answered against the parent — which is the same thing a first
/// real run needs, and leaves nothing behind either way.
fn probe_writable(dir: &Path) -> Result<(), String> {
    let (target, probe_name) = if dir.is_dir() {
        (
            dir.to_path_buf(),
            format!(".doctor-probe-{}", std::process::id()),
        )
    } else {
        let parent = dir
            .parent()
            .ok_or_else(|| "no parent directory to create it in".to_string())?;
        (
            parent.to_path_buf(),
            format!(".smith-doctor-probe-{}", std::process::id()),
        )
    };
    let probe = target.join(probe_name);
    std::fs::write(&probe, b"ok").map_err(|e| e.to_string())?;
    std::fs::remove_file(&probe).map_err(|e| e.to_string())?;
    Ok(())
}

fn check_session_db(cwd: &Path) -> Check {
    // `SessionStore::open` *creates* the database and runs every migration
    // into it. In a normal run that is exactly right; in a diagnostic it means
    // asking "is the store healthy?" leaves a store behind in a project that
    // had none. A project with no history yet is a perfectly healthy state and
    // needs no file to prove it.
    if !cwd.join(".smith").join("sessions.db").exists() {
        return Check::ok(
            "session db",
            "none yet — it is created on the first conversation in this project",
        );
    }

    match smith_store::SessionStore::open(cwd) {
        Ok(store) => match store.schema_version() {
            Ok(version) => Check::ok("session db", format!("schema version {version}")),
            Err(e) => Check::fail(
                "session db",
                format!("opened, but its schema version is unreadable: {e}"),
                "The database may be corrupt. Move `.smith/sessions.db` aside — smith creates a \
                 fresh one. Past conversations in the old file are lost; nothing else is.",
            ),
        },
        Err(e) => {
            // The one open failure with a *specific* cause and a specific fix.
            // `SessionError::SchemaTooNew` exists precisely so a database from
            // a newer build is distinguishable from a corrupt one — and the
            // two have opposite remedies, since deleting it here would throw
            // away history an upgrade would have read back fine.
            let too_new = matches!(e, smith_store::SessionError::SchemaTooNew { .. });
            Check::fail(
                "session db",
                e.to_string(),
                if too_new {
                    "This project's history was written by a newer smith. Upgrade smith to at \
                     least that version. Only move `.smith/sessions.db` aside if you are willing \
                     to lose that history — an upgrade would have read it."
                } else {
                    "Move `.smith/sessions.db` aside and re-run — smith creates a fresh store. If \
                     it fails again, `.smith/` itself is likely not writable."
                },
            )
        }
    }
}

// ---------------------------------------------------------------------------
// mcp
// ---------------------------------------------------------------------------

async fn check_mcp_server(server: &McpServerConfig) -> Check {
    let name = format!("mcp:{}", server.name);
    // What to call the server in a diagnostic: a URL entry has no command, and
    // telling its owner to check `PATH` would send them somewhere with nothing
    // in it.
    let target = match server.url.as_deref() {
        Some(url) if !url.is_empty() => url.to_string(),
        _ => server.command.clone(),
    };
    let is_url = server.url.as_deref().is_some_and(|u| !u.is_empty());
    let connect = smith_mcp::McpClient::connect(&server.name, server);

    let client = match tokio::time::timeout(MCP_TIMEOUT, connect).await {
        Ok(Ok(client)) => client,
        Ok(Err(e)) => {
            let spawn_failed = matches!(e, smith_mcp::McpError::Spawn(_));
            return Check::fail(
                name,
                format!("cannot start `{target}`: {e}"),
                if is_url {
                    format!(
                        "smith could not complete the MCP handshake against {target}. Check the \
                         URL, that the server is running, and any credentials it needs in \
                         `headers` (the [[mcp_servers]] entry \"{}\").",
                        server.name
                    )
                } else if spawn_failed {
                    format!(
                        "`{target}` is not on PATH or is not executable. Install it, or correct \
                         `command` in the [[mcp_servers]] entry named \"{}\".",
                        server.name
                    )
                } else {
                    format!(
                        "The process started but did not complete the MCP handshake. Run `{} {}` \
                         by hand to see what it prints on stderr.",
                        server.command,
                        server.args.join(" ")
                    )
                },
            );
        }
        Err(_) => {
            return Check::fail(
                name,
                format!(
                    "`{target}` did not respond within {}s",
                    MCP_TIMEOUT.as_secs()
                ),
                if is_url {
                    format!("{target} accepted the connection but never answered `initialize`.")
                } else {
                    format!(
                        "The server hung during startup. Run `{} {}` by hand — one that waits on \
                         stdin, or asks for credentials interactively, will hang smith the same \
                         way at every launch.",
                        server.command,
                        server.args.join(" ")
                    )
                },
            )
        }
    };

    match tokio::time::timeout(MCP_TIMEOUT, client.list_tools()).await {
        Ok(Ok(tools)) if tools.is_empty() => Check::warn(
            name,
            "starts, but publishes no tools",
            format!(
                "smith connects to \"{}\" and gets nothing to call. Check the server's own \
                 configuration — many publish tools only once their credentials are set.",
                server.name
            ),
        ),
        Ok(Ok(tools)) => {
            let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).take(6).collect();
            let more = tools.len().saturating_sub(names.len());
            let listed = if more > 0 {
                format!("{} (+{more} more)", names.join(", "))
            } else {
                names.join(", ")
            };
            Check::ok(name, format!("{} tool(s): {listed}", tools.len()))
        }
        Ok(Err(e)) => Check::fail(
            name,
            format!("started, but tools/list failed: {e}"),
            format!(
                "The server speaks MCP but refused to list its tools. Check its logs, and that \
                 any credentials it needs are present in the environment smith launches it from \
                 (the [[mcp_servers]] entry \"{}\").",
                server.name
            ),
        ),
        Err(_) => Check::fail(
            name,
            format!(
                "tools/list did not answer within {}s",
                MCP_TIMEOUT.as_secs()
            ),
            format!(
                "The server starts but stalls on tools/list, so every smith launch waits on it. \
                 Remove the \"{}\" entry from [[mcp_servers]] until it is fixed.",
                server.name
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from(toml_text: &str) -> Config {
        toml::from_str(toml_text).unwrap()
    }

    // -- status and exit code ----------------------------------------------

    #[test]
    fn an_all_ok_report_exits_zero() {
        let mut report = Report::default();
        report.push(Check::ok("a", "fine"));
        report.push(Check::ok("b", "fine"));
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.worst(), Status::Ok);
    }

    /// A warning is a degraded setup, not a broken one. Exiting non-zero for
    /// "no browser for web_search" would make `smith doctor` unusable as a CI
    /// gate, which is the entire reason it has an exit code.
    #[test]
    fn warnings_alone_still_exit_zero() {
        let mut report = Report::default();
        report.push(Check::ok("a", "fine"));
        report.push(Check::warn("b", "degraded", "do the thing"));
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.worst(), Status::Warn);
    }

    #[test]
    fn any_failure_exits_non_zero() {
        let mut report = Report::default();
        report.push(Check::ok("a", "fine"));
        report.push(Check::warn("b", "degraded", "do the thing"));
        report.push(Check::fail("c", "broken", "fix the thing"));
        assert_eq!(report.exit_code(), EXIT_FAILED);
        assert_ne!(report.exit_code(), 0);
        assert_eq!(report.worst(), Status::Fail);
    }

    #[test]
    fn an_empty_report_is_not_a_failure() {
        assert_eq!(Report::default().exit_code(), 0);
    }

    // -- rendering ----------------------------------------------------------

    #[test]
    fn renders_a_status_a_name_and_a_detail_for_every_check() {
        let mut report = Report::default();
        report.push(Check::ok("config", "layers in effect: global"));
        report.push(Check::fail(
            "api key",
            "no key for anthropic",
            "Run `smith setup`.",
        ));
        let text = report.render(&Redactor::default());

        assert!(text.contains("OK   config"), "{text}");
        assert!(text.contains("layers in effect: global"), "{text}");
        assert!(text.contains("FAIL api key"), "{text}");
        assert!(text.contains("-> Run `smith setup`."), "{text}");
    }

    #[test]
    fn the_summary_counts_failures_and_warnings() {
        let mut report = Report::default();
        report.push(Check::ok("a", "fine"));
        report.push(Check::warn("b", "meh", "r"));
        report.push(Check::fail("c", "bad", "r"));
        let text = report.render(&Redactor::default());
        assert!(
            text.contains("3 checks, 1 failure(s), 1 warning(s)."),
            "{text}"
        );
    }

    #[test]
    fn an_all_ok_report_says_so() {
        let mut report = Report::default();
        report.push(Check::ok("a", "fine"));
        assert!(report
            .render(&Redactor::default())
            .contains("1 checks, all OK."));
    }

    #[test]
    fn warnings_without_failures_are_summarised_as_such() {
        let mut report = Report::default();
        report.push(Check::warn("a", "meh", "r"));
        assert!(report
            .render(&Redactor::default())
            .contains("1 checks, 1 warning(s), no failures."));
    }

    /// A multi-line remedy has to stay attached to its check rather than
    /// running back to the left margin — the Ollama install remedy is two
    /// lines, and the second is the important one.
    #[test]
    fn a_multi_line_remedy_stays_indented_under_its_check() {
        let mut report = Report::default();
        report.push(Check::fail("ollama", "not installed", "line one\nline two"));
        let text = report.render(&Redactor::default());
        assert!(text.contains("       -> line one\n"), "{text}");
        assert!(text.contains("       -> line two\n"), "{text}");
    }

    // -- the invariant this module exists for -------------------------------

    /// Runs the whole diagnosis against a deliberately broken configuration
    /// and holds every non-OK result to the rule. This is what catches a new
    /// check added without a remedy — the failure mode the module docs call a
    /// bug.
    #[tokio::test]
    async fn no_check_in_a_real_run_fails_without_telling_you_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_from(
            r#"
            [general]
            provider = "ollama"
            [ollama]
            base_url = "http://127.0.0.1:1/v1"
            [[mcp_servers]]
            name = "definitely-not-installed"
            command = "smith-doctor-no-such-command"
            "#,
        );

        let report = diagnose(dir.path(), &config).await;

        assert!(!report.checks.is_empty());
        for check in &report.checks {
            if check.status == Status::Ok {
                continue;
            }
            let remedy = check.remedy.as_deref().unwrap_or("");
            assert!(
                remedy.trim().len() > 20,
                "check `{}` ({:?}) has no usable remedy: {remedy:?}",
                check.name,
                check.status
            );
        }
        // An unreachable provider and a missing MCP server are both failures,
        // so this run has to be usable as a CI red.
        assert_eq!(report.exit_code(), EXIT_FAILED);
    }

    /// Every check the run produced is named, so the report can't quietly
    /// stop covering one of the things the spec lists.
    #[tokio::test]
    async fn the_report_covers_every_area_the_spec_names() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_from(
            r#"
            [[mcp_servers]]
            name = "x"
            command = "smith-doctor-no-such-command"
            "#,
        );

        let report = diagnose(dir.path(), &config).await;
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();

        for expected in [
            "config",
            "provider",
            "api key",
            "provider reachable",
            "ollama",
            "web_search browser",
            ".smith dir",
            "session db",
        ] {
            assert!(
                names.contains(&expected),
                "missing `{expected}` in {names:?}"
            );
        }
        assert!(names.iter().any(|n| n.starts_with("mcp:")), "{names:?}");
    }

    // -- secrets ------------------------------------------------------------

    /// Where the key came from, never the key itself.
    #[test]
    fn the_api_key_check_names_its_source_and_never_the_key() {
        const KEY: &str = "sk-ant-api03-super-secret-value";
        let config = config_from(&format!("[anthropic]\napi_key = \"{KEY}\""));
        let resolved = resolve_api_key(&config, ProviderKind::Anthropic);
        // Only meaningful when the environment isn't supplying one instead.
        if resolved.source() != &KeySource::ConfigFile {
            return;
        }

        let check = check_api_key(ProviderKind::Anthropic, &resolved);
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("config"), "{}", check.detail);
        assert!(!check.detail.contains(KEY), "leaked: {}", check.detail);
    }

    /// Belt and braces: even if a future check interpolated a key into a
    /// detail, `render` must not put it on stdout.
    #[test]
    fn the_redactor_scrubs_a_key_that_leaked_into_a_detail() {
        const KEY: &str = "sk-ant-api03-super-secret-value";
        let mut report = Report::default();
        report.push(Check::fail("careless", format!("key was {KEY}"), "fix it"));

        let text = report.render(&Redactor::new([KEY.to_string()]));

        assert!(!text.contains(KEY), "leaked: {text}");
        assert!(text.contains("[redacted]"), "{text}");
    }

    /// With no key at all, the missing key is the one diagnosis worth making.
    /// A second FAIL saying the key is "wrong, expired, or lacks access" would
    /// send someone to regenerate a credential they never had.
    #[tokio::test]
    async fn an_unauthenticated_probe_defers_to_the_api_key_check() {
        let config = config_from("[general]\nprovider = \"anthropic\"");
        let check = check_provider_reachable(&config, ProviderKind::Anthropic, None).await;
        if check.detail.contains("cannot reach") {
            // Offline; nothing to assert about the endpoint's answer.
            return;
        }
        assert_ne!(check.status, Status::Fail, "{}", check.detail);
        assert!(
            check.remedy.as_deref().unwrap_or("").contains("api key"),
            "{check:?}"
        );
    }

    #[test]
    fn a_missing_key_is_a_failure_that_names_the_variable_to_set() {
        let key = ResolvedKey {
            source: KeySource::Missing,
            value: None,
        };
        let check = check_api_key(ProviderKind::Anthropic, &key);
        assert_eq!(check.status, Status::Fail);
        assert!(check.remedy.as_ref().unwrap().contains("ANTHROPIC_API_KEY"));

        let check = check_api_key(ProviderKind::Openai, &key);
        assert!(check.remedy.as_ref().unwrap().contains("OPENAI_API_KEY"));
    }

    /// Ollama has no credential; reporting a missing key would send someone
    /// hunting for one that does not exist.
    #[test]
    fn ollama_needs_no_key_and_is_not_reported_as_missing_one() {
        let config = config_from("[general]\nprovider = \"ollama\"");
        let resolved = resolve_api_key(&config, ProviderKind::Ollama);
        assert_eq!(resolved.source(), &KeySource::NotNeeded);
        assert_eq!(
            check_api_key(ProviderKind::Ollama, &resolved).status,
            Status::Ok
        );
    }

    /// `build_provider` prefers the environment, so doctor has to report the
    /// key that will actually be used rather than the other one.
    #[test]
    fn the_environment_wins_over_the_config_file_just_as_it_does_at_runtime() {
        let config = config_from("[anthropic]\napi_key = \"from-config-file-value\"");
        let resolved = resolve_api_key(&config, ProviderKind::Anthropic);
        match std::env::var("ANTHROPIC_API_KEY") {
            Ok(v) if !v.trim().is_empty() => {
                assert_eq!(resolved.source(), &KeySource::Env("ANTHROPIC_API_KEY"))
            }
            _ => assert_eq!(resolved.source(), &KeySource::ConfigFile),
        }
    }

    // -- provider selection -------------------------------------------------

    #[test]
    fn an_unknown_provider_name_is_a_failure_not_a_silent_fallback() {
        let config = config_from("[general]\nprovider = \"claude-but-typoed\"");
        let check = check_provider_selected(&config, resolve_provider(&config));
        assert_eq!(check.status, Status::Fail);
        assert!(check
            .remedy
            .as_ref()
            .unwrap()
            .contains("anthropic, openai, openrouter, 9router, ollama"));
    }

    #[test]
    fn a_configured_provider_and_model_are_reported_together() {
        let config = config_from("[general]\nprovider = \"openai\"\nmodel = \"gpt-4.1\"");
        let check = check_provider_selected(&config, resolve_provider(&config));
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("openai"), "{}", check.detail);
        assert!(check.detail.contains("gpt-4.1"), "{}", check.detail);
    }

    #[test]
    fn no_configured_provider_warns_and_names_the_default() {
        let config = Config::default();
        let check = check_provider_selected(&config, resolve_provider(&config));
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("anthropic"), "{}", check.detail);
    }

    // -- config layers ------------------------------------------------------

    #[test]
    fn an_unparseable_project_config_is_a_failure_that_says_which_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = smith_config::project_config_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "this is [not valid toml").unwrap();

        let check = check_config_layers(dir.path());

        assert_eq!(check.status, Status::Fail);
        assert!(
            check.detail.contains(&path.display().to_string()),
            "{}",
            check.detail
        );
        // The specific danger: it is ignored rather than fatal, so smith runs
        // with settings the user did not intend.
        assert!(check.remedy.as_ref().unwrap().contains("ignores"));
    }

    #[test]
    fn a_valid_project_config_is_listed_as_a_layer_in_effect() {
        let dir = tempfile::tempdir().unwrap();
        let path = smith_config::project_config_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[general]\nmodel = \"x\"\n").unwrap();

        let check = check_config_layers(dir.path());

        assert_ne!(check.status, Status::Fail);
        assert!(check.detail.contains("project"), "{}", check.detail);
    }

    // -- ollama guidance ----------------------------------------------------

    /// The remedy has to be the command for the machine in front of the user,
    /// not a link to a page listing five of them.
    #[test]
    fn the_ollama_remedy_is_a_command_for_the_detected_platform() {
        assert!(ollama_install_command("macos").contains("brew install ollama"));
        assert!(ollama_install_command("linux").contains("ollama.com/install.sh"));
        assert!(ollama_install_command("windows").contains("winget"));
        // An unknown platform still gets somewhere to go.
        assert!(ollama_install_command("plan9").contains("ollama.com/download"));
    }

    /// Installing Ollama is guidance, never an action: it registers a daemon,
    /// edits PATH and may need sudo. The remedy has to carry the command, and
    /// smith has to not run it.
    #[tokio::test]
    async fn a_missing_ollama_is_explained_rather_than_installed() {
        let config = config_from("[general]\nprovider = \"ollama\"");
        let check = check_ollama(&config).await;
        if check.status == Status::Ok {
            // This machine has Ollama; there is no advice to assert about.
            return;
        }
        let remedy = check.remedy.as_ref().unwrap();
        assert!(
            remedy.contains("ollama serve") || remedy.contains("Install it"),
            "{remedy}"
        );
        assert!(
            remedy.contains("will not install it") || remedy.contains("ollama serve"),
            "{remedy}"
        );
    }

    // -- project directory --------------------------------------------------

    #[test]
    fn a_writable_project_dir_and_an_existing_db_both_pass() {
        let dir = tempfile::tempdir().unwrap();
        // A project that has actually been used, which is when the schema
        // version becomes a real question.
        drop(smith_store::SessionStore::open(dir.path()).unwrap());

        let (writable, schema) = check_project_dir(dir.path());

        assert_eq!(writable.status, Status::Ok, "{}", writable.detail);
        assert_eq!(schema.status, Status::Ok, "{}", schema.detail);
        // The version is what a future `SchemaTooNew` gets compared against.
        assert!(
            schema.detail.contains("schema version"),
            "{}",
            schema.detail
        );
    }

    /// Regression, and the reason `probe_writable` no longer calls
    /// `create_dir_all`: an earlier version created `.smith/` and a
    /// `sessions.db` in whatever directory doctor ran in. That is not merely
    /// untidy — a stray `.smith/` is a marker `MemoryScope::discover` treats
    /// as a project root, so *diagnosing* a directory changed how smith
    /// behaved in it afterwards.
    #[test]
    fn diagnosing_a_project_creates_nothing_in_it() {
        let dir = tempfile::tempdir().unwrap();

        let (writable, schema) = check_project_dir(dir.path());

        assert_eq!(writable.status, Status::Ok, "{}", writable.detail);
        assert_eq!(schema.status, Status::Ok, "{}", schema.detail);
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "doctor created: {leftovers:?}");
    }

    /// A project with no history is healthy, not broken — and saying so is
    /// what lets the check avoid creating a database to find out.
    #[test]
    fn a_project_with_no_history_yet_is_reported_as_fine() {
        let dir = tempfile::tempdir().unwrap();
        let check = check_session_db(dir.path());
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("none yet"), "{}", check.detail);
        assert!(!dir.path().join(".smith").exists());
    }

    /// The probe must not leave its own droppings in an existing `.smith`.
    #[test]
    fn the_writability_probe_cleans_up_after_itself() {
        let dir = tempfile::tempdir().unwrap();
        let smith_dir = dir.path().join(".smith");
        std::fs::create_dir_all(&smith_dir).unwrap();

        probe_writable(&smith_dir).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&smith_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[test]
    #[cfg(unix)]
    fn an_unwritable_project_dir_fails_and_the_db_check_does_not_pile_on() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let smith_dir = dir.path().join(".smith");
        std::fs::create_dir_all(&smith_dir).unwrap();
        std::fs::set_permissions(&smith_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let (writable, schema) = check_project_dir(dir.path());

        // Running as root defeats the mode bits entirely; skip rather than
        // assert something untrue about the check.
        if writable.status == Status::Ok {
            let _ = std::fs::set_permissions(&smith_dir, std::fs::Permissions::from_mode(0o700));
            return;
        }
        assert_eq!(writable.status, Status::Fail);
        assert!(writable.remedy.as_ref().unwrap().contains("chmod"));
        // A second failure would be noise: it has the same single cause.
        assert_eq!(schema.status, Status::Warn);

        let _ = std::fs::set_permissions(&smith_dir, std::fs::Permissions::from_mode(0o700));
    }

    // -- mcp ----------------------------------------------------------------

    #[tokio::test]
    async fn an_mcp_server_that_is_not_installed_says_which_command_is_missing() {
        let check = check_mcp_server(&McpServerConfig {
            name: "ghost".to_string(),
            command: "smith-doctor-definitely-not-a-real-command".to_string(),
            ..Default::default()
        })
        .await;

        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.name, "mcp:ghost");
        assert!(check
            .detail
            .contains("smith-doctor-definitely-not-a-real-command"));
        let remedy = check.remedy.as_ref().unwrap();
        assert!(remedy.contains("PATH"), "{remedy}");
        assert!(remedy.contains("ghost"), "{remedy}");
    }

    #[tokio::test]
    async fn no_configured_mcp_servers_is_a_clean_ok_rather_than_a_missing_check() {
        let mut report = Report::default();
        report.extend_mcp(&Config::default()).await;
        assert_eq!(report.checks.len(), 1);
        assert_eq!(report.checks[0].status, Status::Ok);
    }

    /// A minimal MCP server, so the happy path is exercised rather than only
    /// the failures — otherwise "does it start and list tools" is untested.
    const FAKE_SERVER: &str = r#"
import sys, json
def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    m = req.get("method")
    if m == "initialize":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"protocolVersion": "2024-11-05", "capabilities": {}}})
    elif m == "tools/list":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": TOOLS}})
"#;

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn fake_server(tools_json: &str) -> McpServerConfig {
        McpServerConfig {
            name: "fake".to_string(),
            command: "python3".to_string(),
            args: vec!["-c".to_string(), FAKE_SERVER.replace("TOOLS", tools_json)],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_working_mcp_server_is_reported_with_the_tools_it_publishes() {
        if !python3_available() {
            return;
        }
        let check = check_mcp_server(&fake_server(
            r#"[{"name": "alpha", "description": "a", "inputSchema": {"type": "object"}},
                {"name": "beta", "description": "b", "inputSchema": {"type": "object"}}]"#,
        ))
        .await;

        assert_eq!(check.status, Status::Ok, "{}", check.detail);
        assert!(check.detail.contains("2 tool(s)"), "{}", check.detail);
        assert!(check.detail.contains("alpha"), "{}", check.detail);
        assert!(check.remedy.is_none());
    }

    /// A server that starts but publishes nothing is a real and confusing
    /// state — usually missing credentials — and earns its own advice rather
    /// than a green tick.
    #[tokio::test]
    async fn an_mcp_server_publishing_no_tools_warns_rather_than_passing() {
        if !python3_available() {
            return;
        }
        let check = check_mcp_server(&fake_server("[]")).await;

        assert_eq!(check.status, Status::Warn, "{}", check.detail);
        assert!(check.remedy.as_ref().unwrap().contains("credentials"));
    }

    // -- the browser check --------------------------------------------------

    #[tokio::test]
    async fn a_configured_browser_that_does_not_exist_warns_with_a_fix() {
        let mut config = Config::default();
        config.runtime.chromium_path = Some("/nonexistent/chrome-headless-shell".to_string());

        let check = check_browser(&config).await;

        // Only meaningful when no env override is shadowing the config value.
        if check.detail.contains("nonexistent") {
            assert_eq!(check.status, Status::Warn);
            assert!(check.remedy.as_ref().unwrap().contains("smith setup"));
        }
    }

    /// No browser at all is a warning, never a failure: `web_search` still
    /// works over plain HTTP, so a machine without one must not go red.
    #[tokio::test]
    async fn a_missing_browser_is_a_warning_not_a_failure() {
        assert_ne!(check_browser(&Config::default()).await.status, Status::Fail);
    }
}
