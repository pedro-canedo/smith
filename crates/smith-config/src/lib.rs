use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod memory;

pub use memory::{MemoryCache, MemoryScope, MEMORY_FILE_NAME};

// --- user-authored extension files ----------------------------------------
// Custom slash commands, skills and personas. All three are markdown on disk
// discovered in a project directory and a global one; see `extend`.
pub mod extend;

pub use extend::commands::{CommandSet, CustomCommand};
pub use extend::persona::{Persona, PersonaMode};
pub use extend::skills::{Skill, SkillCatalog};
pub use extend::Origin;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not determine the home directory")]
    NoHomeDir,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("nothing to remember — the note was empty")]
    EmptyNote,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub anthropic: ProviderSecrets,
    #[serde(default)]
    pub openai: ProviderSecrets,
    #[serde(default)]
    pub openrouter: OpenRouterSettings,
    /// TOML section `[9router]` — a bare key may start with a digit, but a
    /// Rust identifier may not, hence the rename.
    #[serde(default, rename = "9router")]
    pub nine_router: NineRouterSettings,
    /// Where smith itself goes when the active provider's *account* quota
    /// exhausts mid-session. Distinct from `[openrouter] fallback_models`,
    /// which is OpenRouter's own server-side chain between its models.
    #[serde(default)]
    pub fallback: FallbackSettings,
    #[serde(default)]
    pub ollama: OllamaSettings,
    /// API key for Exa (https://dashboard.exa.ai), the primary `web_search`
    /// backend. Optional — without it, `web_search` still tries Exa's
    /// keyless endpoint first and falls back to DuckDuckGo lite.
    #[serde(default)]
    pub exa: ProviderSecrets,
    /// API key for Tavily (https://app.tavily.com), a `web_search` backend
    /// with a free tier (1,000 credits/month, no card). Optional — without it
    /// the tier is skipped, exactly like Exa.
    #[serde(default)]
    pub tavily: ProviderSecrets,
    /// `web_search` backend settings that are not credentials.
    #[serde(default)]
    pub search: SearchSettings,
    /// `[web]` — the local web console (`--web`, `docs/web-console.md`).
    #[serde(default, skip_serializing_if = "WebSettings::is_default")]
    pub web: WebSettings,
    /// Which palette the TUI paints in. Must stay ahead of `runtime` for the
    /// ordering reason below *and* because its own `colors` field serializes
    /// as a nested table: a plain-table field written after it would land
    /// inside `[theme.colors]`.
    #[serde(default)]
    pub theme: ThemeSettings,
    /// Key bindings for the TUI's discretionary commands, `action = "key"`.
    /// Serializes as a plain table of scalars, so it must stay ahead of
    /// `runtime` for the same reason `theme` does.
    #[serde(default, skip_serializing_if = "KeySettings::is_empty")]
    pub keys: KeySettings,
    /// Third-party binaries `smith setup` provisioned into `~/.smith/runtime`.
    /// Must stay ahead of `mcp_servers`: TOML forbids a plain table after an
    /// array of tables, so a field serialized after it would produce a file
    /// this same struct could not read back.
    #[serde(default)]
    pub runtime: RuntimeSettings,
    /// Shell commands run at fixed points in a turn. Between `runtime` and
    /// `mcp_servers` for the ordering reason above: its own fields are arrays
    /// of tables, so anything serialized after it must be a table too.
    #[serde(default)]
    pub hooks: HookSettings,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
}

/// `[hooks]` — user code smith runs at three points in a turn. See
/// `docs/hooks.md` for the JSON contract and `docs/authorization.md` for where
/// `PreToolUse` sits relative to the plan gate and the permission prompt.
///
/// One array per event rather than one array with an `event` field: the event
/// decides what the hook is *handed* and what its answer *means* (a timeout
/// denies a `PreToolUse` and is only a warning on a `PostToolUse`), so a typo
/// in an `event = "..."` string would silently move a hook from a gate to a
/// logger. A typo in a table name is a parse error instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookSettings {
    /// Before a tool runs. May deny the call and may rewrite its arguments.
    #[serde(default)]
    pub pre_tool_use: Vec<HookCommand>,
    /// After a tool ran. May add text to the result; may not deny it.
    #[serde(default)]
    pub post_tool_use: Vec<HookCommand>,
    /// Before a user message is sent to the provider. May rewrite it or refuse
    /// the turn. Never fires for a subagent's prompt.
    #[serde(default)]
    pub user_prompt_submit: Vec<HookCommand>,
}

/// One `[[hooks.*]]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookCommand {
    /// Run through `sh -c` (`cmd /C` on Windows), with the event's JSON on
    /// stdin and its answer expected on stdout.
    pub command: String,
    /// `|`-separated exact tool names, or `*`/omitted for every tool.
    /// Deliberately not a regex — a typo that matches nothing is exactly the
    /// failure mode a policy hook must not have. Ignored for
    /// `user_prompt_submit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    /// Defaults to 5000. A `pre_tool_use` or `user_prompt_submit` hook that
    /// exceeds it *denies*; a `post_tool_use` hook that does is only a warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct General {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// "ask" | "session" | "skip" — see smith_core::PermissionPolicy. `None`
    /// means the default (`ask`).
    pub permission_policy: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderSecrets {
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenRouterSettings {
    pub api_key: Option<String>,
    /// Override of the public endpoint, for proxies.
    pub base_url: Option<String>,
    /// Server-side per-request chain (`models` + `route: "fallback"` in the
    /// request body). First entry is the model smith drives; the rest are
    /// OpenRouter's own fallbacks. Empty = plain single-model requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NineRouterSettings {
    pub api_key: Option<String>,
    /// Defaults to the gateway's own default port on localhost.
    pub base_url: Option<String>,
    /// Model id to request through the gateway.
    pub model: Option<String>,
}

/// `[fallback] providers = ["9router", "ollama"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FallbackSettings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OllamaSettings {
    pub base_url: Option<String>,
    /// Which model a **fallback** entry should ask for.
    ///
    /// `[general] model` is the primary's model; a chain entry needs its own,
    /// exactly as `[9router] model` already does. Without this every fallback
    /// Ollama entry asked for the hardcoded default — and `setup_openrouter`
    /// writes an Ollama fallback for almost everyone, so almost everyone's
    /// chain pointed at a model they had probably never pulled.
    pub model: Option<String>,
}

/// `[web]` — the local web console served beside the TUI when enabled.
///
/// Off by default, and headless never starts it regardless: the console is
/// an interactive surface, and CI must stay byte-identical with or without
/// this table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebSettings {
    /// Start the console with every interactive session (`--web` for one
    /// session at a time).
    pub enabled: Option<bool>,
    /// Pin the port. Unset means an ephemeral one, which is the safer
    /// default — predictable is the one property a privileged loopback
    /// endpoint should not have.
    pub port: Option<u16>,
    /// Open the browser on the console URL at startup.
    pub open_browser: Option<bool>,
}

impl WebSettings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// `[search]` — how `web_search` looks things up.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchSettings {
    /// Pin `web_search` to exactly one backend (`searxng`, `exa`, `tavily`,
    /// `bing`, `bing-browser`, `google-news`, `duckduckgo`). Unset means the
    /// full fall-through chain.
    ///
    /// A pin is absolute, Hermes-style: only that backend runs, and if it is
    /// missing its key or URL the search fails with a message naming exactly
    /// what to set — never a silent reroute to another backend. That is the
    /// point of pinning: a user who runs their own SearXNG so queries stay on
    /// their infrastructure must not have one quietly sent to Bing because
    /// their instance was down.
    pub backend: Option<String>,
    /// Base URL of a SearXNG instance the user runs, e.g.
    /// `https://searx.example.com`. When set it becomes the *first* backend
    /// `web_search` tries, ahead of even a paid Exa key: it is the user's own
    /// infrastructure, so it has no shared IP reputation, no anti-bot layer
    /// and no rate limit they did not choose.
    ///
    /// The instance must have JSON output enabled — SearXNG ships with it off,
    /// and answers `format=json` with HTTP 403 until `json` is added under
    /// `search: formats:` in its `settings.yml`.
    pub searxng_url: Option<String>,
    /// Bing market tag for the free search tier, e.g. `en-US` or `pt-BR`.
    ///
    /// Not cosmetic: Bing answers a request whose market does not match the
    /// query's language with ten well-formed results that have nothing to do
    /// with the query. Defaults to `en-US`, which suits the English technical
    /// queries an agent mostly issues; set it if yours are usually in another
    /// language.
    pub market: Option<String>,
}

/// `[theme]` — which palette the TUI paints in, and any single colours the
/// user wants to move.
///
/// The values are kept as plain strings here on purpose: this crate must not
/// know what a `ratatui::style::Color` is (nothing below `smith-tui` does),
/// and the "no colour literal outside `theme.rs`" rule is exactly the rule a
/// second parser in a second crate would break. `smith-tui::theme::Theme::
/// resolve` is the one place that turns these strings into colours, and the
/// one place that rejects them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThemeSettings {
    /// `dark` (the default), `light` or `high_contrast`. An unknown name is a
    /// startup error, never a silent fall back to the default.
    pub name: Option<String>,
    /// Per-token overrides by hex string, e.g. `ember = "#ff8c3c"`. Keys are
    /// the design system's token names (`base`, `raised`, `primary`, …);
    /// an unknown key is an error for the same reason an unknown name is.
    ///
    /// Must stay the **last** field: it serializes as a nested table, so a
    /// scalar written after it would land inside `[theme.colors]`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub colors: std::collections::BTreeMap<String, String>,
}

/// `[keys]` — `action = "key"`, e.g. `toggle_sidebar = "ctrl+t"`.
///
/// Held as free-form strings rather than parsed here on purpose: this crate
/// is a leaf that knows nothing about `crossterm`, and the set of valid
/// actions belongs to the TUI. `smith-tui::KeyMap::from_overrides` validates
/// it, and an unknown action or unparseable key is a startup error naming the
/// offender.
pub type KeySettings = std::collections::BTreeMap<String, String>;

/// Where `smith setup` put the runtimes it provisioned.
///
/// Only ever *written* by the provisioning step, never hand-edited in the
/// normal case — a user who wants their own browser sets `SMITH_CHROMIUM_PATH`
/// instead, which still wins over anything recorded here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeSettings {
    /// Absolute path to the provisioned headless browser binary.
    pub chromium_path: Option<String>,
    /// The Chrome for Testing build `chromium_path` came from, so an upgrade
    /// can tell "already current" from "a newer build exists" without
    /// launching the binary.
    pub chromium_version: Option<String>,
    /// Absolute path to the provisioned Node.js binary (for the 9Router
    /// gateway), and the version it came from.
    pub node_path: Option<String>,
    pub node_version: Option<String>,
    /// Directory the 9router npm package was installed into, and its pinned
    /// version.
    pub ninerouter_dir: Option<String>,
    pub ninerouter_version: Option<String>,
}

/// One entry in `[[mcp_servers]]`: an MCP server smith should connect to at
/// startup and pull tools, resources and prompts from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    /// The program to spawn for a stdio-transport server. `#[serde(default)]`
    /// because a `url` server has no command — an entry with neither is what
    /// is rejected, not an entry missing this one field.
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    // --- added by the MCP transports work; keep contiguous -----------------
    /// Endpoint of a URL-based server. Its presence is what selects a network
    /// transport, so every existing `command`-only entry keeps working with no
    /// edit at all.
    #[serde(default)]
    pub url: Option<String>,
    /// Forces a transport instead of inferring one: `stdio`, `http`
    /// (Streamable HTTP) or `sse` (the older HTTP+SSE pair). Unset means
    /// infer — `command` implies stdio, and a `url` is tried as Streamable
    /// HTTP first, then as HTTP+SSE.
    #[serde(default)]
    pub transport: Option<String>,
    /// Extra HTTP headers for a URL server — in practice `Authorization`.
    /// Ignored by the stdio transport.
    ///
    /// Must stay the **last** field: it serializes as a nested table, and TOML
    /// forbids a scalar key after a table inside the same `[[mcp_servers]]`
    /// element. `skip_serializing_if` additionally keeps a stdio entry's
    /// round-tripped bytes byte-identical to what they were before this field
    /// existed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
}

pub const OLLAMA_HOST: &str = "http://127.0.0.1:11434";
pub const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_NINEROUTER_BASE_URL: &str = "http://localhost:20128/v1";
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434/v1";

/// `~/.smith` (or `$SMITH_HOME`) — the config file, the runtimes smith
/// downloaded, and, since history moved out of projects, one directory per
/// project under `projects/`. The only place secrets live.
pub fn config_dir() -> Result<PathBuf, ConfigError> {
    // `SMITH_HOME` names the `.smith` directory itself, the way `CARGO_HOME`
    // does — not the home it usually sits in.
    //
    // It exists because there is otherwise no way to move this root, and on
    // Windows there is no way to *test* against it either: `directories`
    // resolves the profile through `SHGetKnownFolderPath`, which ignores
    // `HOME` and `USERPROFILE` alike. The integration tests set `HOME` and
    // believed they were hermetic; on Windows they were reading, and after
    // session history moved here would have been writing, the real profile.
    if let Some(root) = std::env::var_os("SMITH_HOME") {
        if !root.is_empty() {
            return Ok(PathBuf::from(root));
        }
    }
    let dirs = directories::BaseDirs::new().ok_or(ConfigError::NoHomeDir)?;
    Ok(dirs.home_dir().join(".smith"))
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?.join("config.toml"))
}

/// Per-project overrides: `<project>/.smith/config.toml`.
pub fn project_config_path(project_dir: &std::path::Path) -> PathBuf {
    project_dir.join(".smith").join("config.toml")
}

/// `~/.smith/runtime` — third-party binaries smith downloaded for itself.
///
/// Deliberately under `~/.smith` rather than an OS cache directory: a cache is
/// something the system may delete at will, and silently losing a 100 MB
/// browser the user explicitly asked `smith setup` to fetch would look like a
/// regression rather than housekeeping.
pub fn runtime_dir() -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?.join("runtime"))
}

/// `~/.smith/projects/<name>-<hash>` — where one project's session history
/// lives.
///
/// Central rather than `<project>/.smith/sessions.db`, so that running smith
/// somewhere does not leave a multi-megabyte database in that directory
/// forever. Still **per project**, not global: `/resume` listing another
/// project's conversations would be worse than the tidiness is worth.
///
/// The directory name is the project's own basename followed by a hash of its
/// absolute path — readable enough to find by eye, unique enough that two
/// checkouts both called `api` do not share a history.
///
/// What stays behind in `<project>/.smith/` is everything that *is* project
/// data: `/rewind` checkpoints and staging hold copies of the project's own
/// files, and `scratch/` is announced to the model as a path inside the jail
/// that `resolve` confines writes to. Moving those out would move a security
/// boundary, not just a file.
pub fn project_store_dir(project_dir: &std::path::Path) -> Result<PathBuf, ConfigError> {
    Ok(config_dir()?
        .join("projects")
        .join(project_store_name(project_dir)))
}

/// The directory name on its own — split out so it is testable without a home
/// directory for `config_dir` to find.
pub fn project_store_name(project_dir: &std::path::Path) -> String {
    // Canonical where possible so `/home/me/p` and a symlink to it are one
    // project. A path that does not exist yet cannot be canonicalised, and
    // falling back to it verbatim is right: it is the name the caller used.
    let canonical =
        std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
    let name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{slug}-{:016x}", path_hash(&canonical.to_string_lossy()))
}

/// FNV-1a over the path.
///
/// Hand-rolled rather than `DefaultHasher`, whose output std explicitly does
/// not promise to be stable across releases — and a hash that changes under
/// people is a directory of orphaned histories. Not a cryptographic choice:
/// nothing here defends against a chosen path, it only has to be stable and
/// spread.
fn path_hash(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// Moves a pre-existing `<project>/.smith/sessions.db` to where it now lives.
///
/// Called once, before the store is opened. Returns the path it moved *from*
/// when it moved something, so the caller can say so — a session history that
/// silently relocates looks identical to one that was lost.
///
/// Refuses to overwrite: if a central database already exists, the legacy file
/// is left exactly where it is. Two histories is a situation someone can look
/// at; one history overwritten by another is not.
pub fn adopt_legacy_session_db(
    project_dir: &std::path::Path,
) -> Result<Option<PathBuf>, ConfigError> {
    let legacy = project_dir.join(".smith").join("sessions.db");
    if !legacy.is_file() {
        return Ok(None);
    }
    let target_dir = project_store_dir(project_dir)?;
    adopt_into(&legacy, &target_dir)
}

/// The move itself, against an explicit destination so it can be tested.
fn adopt_into(
    legacy: &std::path::Path,
    target_dir: &std::path::Path,
) -> Result<Option<PathBuf>, ConfigError> {
    let target = target_dir.join("sessions.db");
    if target.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(target_dir)?;
    // Rename first: it is atomic within a filesystem and cannot half-copy a
    // database. `~/.smith` on another mount than the project is ordinary
    // enough (an encrypted home, a network checkout) to need the fallback.
    if std::fs::rename(legacy, &target).is_err() {
        std::fs::copy(legacy, &target)?;
        std::fs::remove_file(legacy)?;
    }
    Ok(Some(legacy.to_path_buf()))
}

impl Config {
    /// Global config only. Prefer `load_layered` — this exists for `save`,
    /// which must never write a project's values back into the global file.
    pub fn load() -> Result<Config, ConfigError> {
        let path = config_path()?;
        Self::load_path(&path)
    }

    fn load_path(path: &std::path::Path) -> Result<Config, ConfigError> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }

    /// Whether one layer on disk parses, without merging it into anything.
    ///
    /// Exists for `smith doctor`. `load_layered` swallows a parse error into
    /// `Config::default()`, which is right for a normal run — smith should
    /// start — but means a typo'd file is silently *ignored* rather than
    /// reported. This is how the diagnostic tells those two apart, and it
    /// lives here so nothing outside this crate has to know the file is TOML.
    ///
    /// `Ok(false)` means the file does not exist, which is not an error.
    pub fn check_path(path: &std::path::Path) -> Result<bool, ConfigError> {
        if !path.exists() {
            return Ok(false);
        }
        Self::load_path(path).map(|_| true)
    }

    /// Global config with the project's `.smith/config.toml` layered on top.
    ///
    /// A project file only has to state what differs — a repo that needs a
    /// specific model shouldn't have to restate every API key, and shouldn't
    /// be able to accidentally blank one by omission. Hence a field-wise
    /// merge rather than "whichever file exists wins".
    ///
    /// Secrets are read from the project layer too, which is a deliberate
    /// call: a project file is where a per-repo key naturally goes, and
    /// refusing it there would just push people back to the global file. It
    /// does mean `.smith/config.toml` can hold a credential — `.smith/` is
    /// gitignored, and that is the assumption this rests on.
    pub fn load_layered(project_dir: &std::path::Path) -> Result<Config, ConfigError> {
        let mut config = Self::load()?;
        let project = Self::load_path(&project_config_path(project_dir))?;
        config.merge_over(project);
        Ok(config)
    }

    /// Applies `other`'s set fields over `self`. `None`/empty means "not
    /// specified", never "unset the global value".
    fn merge_over(&mut self, other: Config) {
        let General {
            provider,
            model,
            permission_policy,
        } = other.general;
        self.general.provider = provider.or(self.general.provider.take());
        self.general.model = model.or(self.general.model.take());
        self.general.permission_policy =
            permission_policy.or(self.general.permission_policy.take());

        for (target, incoming) in [
            (&mut self.anthropic, other.anthropic),
            (&mut self.openai, other.openai),
            (&mut self.exa, other.exa),
            (&mut self.tavily, other.tavily),
        ] {
            target.api_key = incoming.api_key.or(target.api_key.take());
        }
        self.ollama.base_url = other.ollama.base_url.or(self.ollama.base_url.take());
        self.ollama.model = other.ollama.model.or(self.ollama.model.take());

        let OpenRouterSettings {
            api_key,
            base_url,
            fallback_models,
        } = other.openrouter;
        self.openrouter.api_key = api_key.or(self.openrouter.api_key.take());
        self.openrouter.base_url = base_url.or(self.openrouter.base_url.take());
        // Wholesale when stated: a chain is an ordered whole, and merging two
        // orders element-wise would produce one nobody wrote.
        if !fallback_models.is_empty() {
            self.openrouter.fallback_models = fallback_models;
        }

        let NineRouterSettings {
            api_key,
            base_url,
            model,
        } = other.nine_router;
        self.nine_router.api_key = api_key.or(self.nine_router.api_key.take());
        self.nine_router.base_url = base_url.or(self.nine_router.base_url.take());
        self.nine_router.model = model.or(self.nine_router.model.take());

        let FallbackSettings { providers } = other.fallback;
        if !providers.is_empty() {
            self.fallback.providers = providers;
        }

        let SearchSettings {
            backend,
            searxng_url,
            market,
        } = other.search;
        self.search.backend = backend.or(self.search.backend.take());
        self.search.searxng_url = searxng_url.or(self.search.searxng_url.take());
        self.search.market = market.or(self.search.market.take());

        let WebSettings {
            enabled,
            port,
            open_browser,
        } = other.web;
        self.web.enabled = enabled.or(self.web.enabled.take());
        self.web.port = port.or(self.web.port.take());
        self.web.open_browser = open_browser.or(self.web.open_browser.take());

        // A project may restyle smith without restating the whole palette, so
        // the overrides merge per token rather than wholesale — the same rule
        // the scalar fields above follow, one level down.
        let ThemeSettings { name, colors } = other.theme;
        self.theme.name = name.or(self.theme.name.take());
        self.theme.colors.extend(colors);

        // Per key, not wholesale: a project rebinding one command should not
        // silently drop the user's global bindings for the others.
        self.keys.extend(other.keys);

        let RuntimeSettings {
            chromium_path,
            chromium_version,
            node_path,
            node_version,
            ninerouter_dir,
            ninerouter_version,
        } = other.runtime;
        self.runtime.chromium_path = chromium_path.or(self.runtime.chromium_path.take());
        self.runtime.chromium_version = chromium_version.or(self.runtime.chromium_version.take());
        self.runtime.node_path = node_path.or(self.runtime.node_path.take());
        self.runtime.node_version = node_version.or(self.runtime.node_version.take());
        self.runtime.ninerouter_dir = ninerouter_dir.or(self.runtime.ninerouter_dir.take());
        self.runtime.ninerouter_version =
            ninerouter_version.or(self.runtime.ninerouter_version.take());

        // Hooks are **not** merged from the project layer, and that is a
        // security decision rather than an oversight. A hook is an arbitrary
        // shell command run on every tool call; honouring one from
        // `<project>/.smith/config.toml` would make `git clone && smith` a
        // code-execution vector for whoever wrote the repository. `~/.smith/
        // config.toml` is the only file that can define one, because it is the
        // only one the user certainly wrote themselves. (`other.hooks` is
        // therefore dropped here on purpose — see `docs/hooks.md`.)

        // MCP servers are a list, not a scalar: a project declaring servers
        // means "these too", not "forget the global ones". Same name in both
        // layers -> the project's definition wins.
        for server in other.mcp_servers {
            match self.mcp_servers.iter_mut().find(|s| s.name == server.name) {
                Some(existing) => *existing = server,
                None => self.mcp_servers.push(server),
            }
        }
    }

    /// Writes the **global** file. Project overrides are hand-edited on
    /// purpose: `/model --save` saving into whatever repo you happened to be
    /// in would be a surprise, and a bad one if the value was a key.
    ///
    /// Written to a sibling temp file and renamed over the target, because
    /// this is the only file holding the user's API keys and a truncating
    /// write has a window where it holds none of them. A crash, a full disk or
    /// two `smith setup` runs racing each other all land in that window. The
    /// rename is same-directory, so it is atomic everywhere smith runs.
    ///
    /// The 0600 goes on the temp file *before* any content does — a file that
    /// is briefly world-readable while it holds a key is the whole problem,
    /// just narrower.
    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        set_permissions(&dir, 0o700)?;

        let path = config_path()?;
        let text = toml::to_string_pretty(self)?;
        write_private_atomic(&path, &text)?;

        Ok(())
    }

    /// Changes one setting in the **global** file and writes only that file.
    ///
    /// The layered config a session runs on is the global file with the
    /// project's merged over it, and `save` writes whatever struct it is
    /// handed. So `config.clone()` + one field + `save()` — which is what
    /// `--save` used to do — took every project override with it and made it
    /// global. A project that set `permission_policy = "skip"` for itself
    /// then silently disabled the permission prompt for **every other
    /// project on the machine** the first time someone ran `/model --save`,
    /// and a project-scoped API key was copied into the global file the same
    /// way.
    ///
    /// Reading the global layer back off disk is what makes the write
    /// narrow: `edit` sees only what is already global, so nothing merged
    /// can leak into it. It also picks up a change made in another window
    /// since this session started, instead of overwriting it with a stale
    /// snapshot.
    pub fn update_global(edit: impl FnOnce(&mut Config)) -> Result<(), ConfigError> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        set_permissions(&dir, 0o700)?;
        Self::update_at(&config_path()?, edit)
    }

    /// The half of [`update_global`] that does not resolve a home directory,
    /// so the narrowing can be tested rather than asserted in a comment.
    fn update_at(
        path: &std::path::Path,
        edit: impl FnOnce(&mut Config),
    ) -> Result<(), ConfigError> {
        let mut global = Self::load_path(path)?;
        edit(&mut global);
        write_private_atomic(path, &toml::to_string_pretty(&global)?)?;
        Ok(())
    }
}

/// Writes `text` to `path` through a sibling temp file, 0600 before content.
///
/// Split out of `save` so it can be tested at all: `save` resolves its own
/// path from the home directory, and a test that exercised it would write to
/// the developer's real `~/.smith/config.toml`.
fn write_private_atomic(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    // Per-process name so concurrent saves cannot share a temp file. They
    // still race on the rename, and that is fine: last writer wins with a
    // whole file, which is what the truncating write could not promise.
    let temp = path.with_extension(format!("toml.{}.new", std::process::id()));

    let write = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        set_permissions(&temp, 0o600)?;
        {
            use std::io::Write;
            file.write_all(text.as_bytes())?;
            // Durable before it is visible: a rename that publishes a file
            // whose bytes are still in the page cache is a rename that can
            // publish an empty config after a power cut.
            file.sync_all()?;
        }
        std::fs::rename(&temp, path)
    })();

    if let Err(e) = write {
        // Never leave a half-written file holding a key beside the real one.
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    set_permissions(path, 0o600)
}

#[cfg(unix)]
fn set_permissions(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_permissions(_path: &std::path::Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Pins the serde rename: the section is `[9router]` (a TOML bare key may
    /// start with a digit), not `[nine_router]`.
    #[test]
    fn the_9router_section_round_trips_under_its_digit_led_name() {
        let mut config = Config::default();
        config.nine_router.api_key = Some("k".into());
        config.nine_router.model = Some("auto".into());
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("[9router]"), "{text}");
        assert!(!text.contains("[nine_router]"), "{text}");
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.nine_router.api_key.as_deref(), Some("k"));
    }

    /// The TOML ordering trap: a config carrying the new sections *and* an
    /// array-of-tables must serialize to something this same struct reads
    /// back — a plain table emitted after `[[mcp_servers]]` would not.
    #[test]
    fn new_sections_survive_alongside_mcp_servers() {
        let mut config = Config::default();
        config.openrouter.api_key = Some("or".into());
        config.openrouter.fallback_models = vec!["a:free".into(), "b:free".into()];
        config.fallback.providers = vec!["9router".into(), "ollama".into()];
        config.mcp_servers.push(McpServerConfig {
            name: "docs".into(),
            command: "server".into(),
            ..Default::default()
        });
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.openrouter.fallback_models, ["a:free", "b:free"]);
        assert_eq!(parsed.fallback.providers, ["9router", "ollama"]);
        assert_eq!(parsed.mcp_servers.len(), 1);
    }

    /// A chain is an ordered whole: the project layer replaces it or leaves
    /// it alone, never merges element-wise.
    #[test]
    fn fallback_chains_merge_wholesale_not_elementwise() {
        let mut global = Config::default();
        global.openrouter.fallback_models = vec!["g1".into(), "g2".into()];
        global.fallback.providers = vec!["ollama".into()];

        // Silent project keeps the global chain.
        let mut merged = global.clone();
        merged.merge_over(Config::default());
        assert_eq!(merged.openrouter.fallback_models, ["g1", "g2"]);
        assert_eq!(merged.fallback.providers, ["ollama"]);

        // A stated project chain replaces it outright.
        let mut project = Config::default();
        project.openrouter.fallback_models = vec!["p1".into()];
        let mut merged = global.clone();
        merged.merge_over(project);
        assert_eq!(merged.openrouter.fallback_models, ["p1"]);
    }

    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let mut config = Config::default();
        config.general.provider = Some("ollama".into());
        config.general.model = Some("llama3.2".into());
        config.ollama.base_url = Some("http://127.0.0.1:11434/v1".into());

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(parsed.general.provider.as_deref(), Some("ollama"));
        assert_eq!(parsed.general.model.as_deref(), Some("llama3.2"));
        assert_eq!(
            parsed.ollama.base_url.as_deref(),
            Some("http://127.0.0.1:11434/v1")
        );
    }

    #[test]
    fn missing_fields_default_to_none() {
        let parsed: Config = toml::from_str("").unwrap();
        assert!(parsed.general.provider.is_none());
        assert!(parsed.anthropic.api_key.is_none());
    }

    fn write_project(dir: &std::path::Path, toml: &str) {
        let path = project_config_path(dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, toml).unwrap();
    }

    fn global() -> Config {
        toml::from_str(
            r#"
            [general]
            provider = "anthropic"
            model = "claude-sonnet-5"
            permission_policy = "ask"
            [anthropic]
            api_key = "global-anthropic-key"
            [openai]
            api_key = "global-openai-key"
            [[mcp_servers]]
            name = "shared"
            command = "shared-server"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn a_project_overrides_only_what_it_states() {
        let mut config = global();
        config.merge_over(
            toml::from_str(
                r#"
                [general]
                model = "claude-opus-5"
                "#,
            )
            .unwrap(),
        );

        assert_eq!(config.general.model.as_deref(), Some("claude-opus-5"));
        // Everything unstated survives — a repo pinning a model must not have
        // to restate every credential to avoid losing them.
        assert_eq!(config.general.provider.as_deref(), Some("anthropic"));
        assert_eq!(
            config.anthropic.api_key.as_deref(),
            Some("global-anthropic-key")
        );
        assert_eq!(config.openai.api_key.as_deref(), Some("global-openai-key"));
    }

    #[test]
    fn an_empty_project_file_changes_nothing() {
        let mut config = global();
        config.merge_over(Config::default());
        assert_eq!(config.general.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(
            config.anthropic.api_key.as_deref(),
            Some("global-anthropic-key")
        );
        assert_eq!(config.mcp_servers.len(), 1);
    }

    #[test]
    fn a_project_may_set_its_own_key() {
        let mut config = global();
        config.merge_over(
            toml::from_str(
                r#"
                [anthropic]
                api_key = "project-key"
                "#,
            )
            .unwrap(),
        );
        assert_eq!(config.anthropic.api_key.as_deref(), Some("project-key"));
    }

    #[test]
    fn mcp_servers_are_added_and_replaced_by_name_not_wholesale() {
        let mut config = global();
        config.merge_over(
            toml::from_str(
                r#"
                [[mcp_servers]]
                name = "shared"
                command = "project-override"
                [[mcp_servers]]
                name = "project-only"
                command = "extra-server"
                "#,
            )
            .unwrap(),
        );

        assert_eq!(
            config.mcp_servers.len(),
            2,
            "the global server must survive"
        );
        let shared = config
            .mcp_servers
            .iter()
            .find(|s| s.name == "shared")
            .unwrap();
        assert_eq!(shared.command, "project-override");
        assert!(config.mcp_servers.iter().any(|s| s.name == "project-only"));
    }

    #[test]
    fn a_project_may_point_at_its_own_provisioned_browser() {
        let mut config = global();
        config.runtime.chromium_path = Some("/home/u/.smith/runtime/global".into());
        config.merge_over(
            toml::from_str(
                r#"
                [runtime]
                chromium_path = "/opt/project-chrome"
                "#,
            )
            .unwrap(),
        );
        assert_eq!(
            config.runtime.chromium_path.as_deref(),
            Some("/opt/project-chrome")
        );
    }

    #[test]
    fn an_unstated_runtime_section_keeps_the_provisioned_browser() {
        let mut config = global();
        config.runtime.chromium_path = Some("/home/u/.smith/runtime/chrome".into());
        config.runtime.chromium_version = Some("151.0.7922.76".into());
        config.merge_over(Config::default());
        assert_eq!(
            config.runtime.chromium_path.as_deref(),
            Some("/home/u/.smith/runtime/chrome")
        );
        assert_eq!(
            config.runtime.chromium_version.as_deref(),
            Some("151.0.7922.76")
        );
    }

    /// TOML cannot express a plain table after an array of tables. If
    /// `runtime` ever moves below `mcp_servers` in the struct, `save` starts
    /// writing files that `load` rejects — and only a round-trip with both
    /// present catches it.
    #[test]
    fn a_config_with_both_a_runtime_and_mcp_servers_round_trips() {
        let mut config = global();
        config.runtime.chromium_path = Some("/home/u/.smith/runtime/chrome".into());
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).expect("must parse back: {text}");
        assert_eq!(
            parsed.runtime.chromium_path.as_deref(),
            Some("/home/u/.smith/runtime/chrome")
        );
        assert_eq!(parsed.mcp_servers.len(), 1);
    }

    /// Same hazard one level down: `headers` is a table inside an
    /// `[[mcp_servers]]` element, so any scalar field serialized after it
    /// would land in the wrong table. And a `command`-only entry must still
    /// serialize to exactly what it did before `url`/`headers` existed.
    #[test]
    fn a_url_server_with_headers_round_trips_and_a_stdio_one_is_unchanged() {
        let mut config = Config {
            mcp_servers: vec![
                McpServerConfig {
                    name: "local".into(),
                    command: "mcp-fs".into(),
                    args: vec!["--root".into(), "/tmp".into()],
                    ..Default::default()
                },
                McpServerConfig {
                    name: "remote".into(),
                    url: Some("https://mcp.example.com/mcp".into()),
                    transport: Some("http".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        config.mcp_servers[1]
            .headers
            .insert("Authorization".into(), "Bearer t".into());

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).expect("must parse back: {text}");

        assert_eq!(parsed.mcp_servers[0].command, "mcp-fs");
        assert!(parsed.mcp_servers[0].url.is_none());
        assert!(parsed.mcp_servers[0].headers.is_empty());
        // An entry with no headers writes no `headers` table at all.
        assert_eq!(text.matches("headers").count(), 1);

        assert_eq!(
            parsed.mcp_servers[1].url.as_deref(),
            Some("https://mcp.example.com/mcp")
        );
        assert_eq!(parsed.mcp_servers[1].transport.as_deref(), Some("http"));
        assert_eq!(
            parsed.mcp_servers[1].headers.get("Authorization").unwrap(),
            "Bearer t"
        );
    }

    /// An existing `[[mcp_servers]]` entry written before URL transports
    /// existed still loads, with the new fields simply absent.
    #[test]
    fn a_pre_existing_stdio_entry_loads_untouched() {
        let config: Config = toml::from_str(
            r#"
            [[mcp_servers]]
            name = "files"
            command = "mcp-server-filesystem"
            args = ["/home/u"]
            "#,
        )
        .unwrap();
        let server = &config.mcp_servers[0];
        assert_eq!(server.command, "mcp-server-filesystem");
        assert_eq!(server.args, vec!["/home/u".to_string()]);
        assert!(server.url.is_none() && server.transport.is_none());
        assert!(server.headers.is_empty());
    }

    #[test]
    fn a_project_may_restyle_one_token_without_restating_the_palette() {
        let mut config = global();
        config.theme.name = Some("dark".into());
        config.theme.colors.insert("ember".into(), "#ff8c3c".into());
        config.theme.colors.insert("plan".into(), "#c684ff".into());
        config.merge_over(
            toml::from_str(
                r##"
                [theme]
                name = "light"
                [theme.colors]
                plan = "#6c30a2"
                "##,
            )
            .unwrap(),
        );

        assert_eq!(config.theme.name.as_deref(), Some("light"));
        assert_eq!(config.theme.colors.get("plan").unwrap(), "#6c30a2");
        // The token the project did not mention keeps the global value.
        assert_eq!(config.theme.colors.get("ember").unwrap(), "#ff8c3c");
    }

    #[test]
    fn an_unstated_theme_section_changes_nothing() {
        let mut config = global();
        config.theme.name = Some("high_contrast".into());
        config.merge_over(Config::default());
        assert_eq!(config.theme.name.as_deref(), Some("high_contrast"));
    }

    /// `[theme.colors]` is a nested table, so anything serialized after it
    /// would be written into it. Same hazard as `mcp_servers.headers`, and
    /// only a round-trip with the later sections present catches it.
    #[test]
    fn a_config_with_a_theme_and_everything_after_it_round_trips() {
        let mut config = global();
        config.theme.name = Some("light".into());
        config.theme.colors.insert("base".into(), "#faf9f7".into());
        config.runtime.chromium_path = Some("/home/u/.smith/runtime/chrome".into());
        config.hooks.pre_tool_use.push(HookCommand {
            command: "true".into(),
            matcher: None,
            timeout_ms: None,
        });

        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).expect("must parse back: {text}");
        assert_eq!(parsed.theme.name.as_deref(), Some("light"));
        assert_eq!(parsed.theme.colors.get("base").unwrap(), "#faf9f7");
        assert_eq!(
            parsed.runtime.chromium_path.as_deref(),
            Some("/home/u/.smith/runtime/chrome")
        );
        assert_eq!(parsed.hooks.pre_tool_use.len(), 1);
        assert_eq!(parsed.mcp_servers.len(), 1);
    }

    /// A config written before `[theme]` existed still loads, with the
    /// section simply absent.
    #[test]
    fn a_config_without_a_theme_section_loads() {
        let parsed: Config = toml::from_str("[general]\nmodel = \"m\"\n").unwrap();
        assert!(parsed.theme.name.is_none());
        assert!(parsed.theme.colors.is_empty());
        // And an empty section writes no `colors` table at all.
        let text = toml::to_string_pretty(&parsed).unwrap();
        assert!(!text.contains("theme.colors"), "{text}");
    }

    #[test]
    fn the_runtime_dir_sits_under_the_config_dir() {
        // Both derive from the home directory, which the test environment may
        // not have; when it does, they must agree.
        if let (Ok(config), Ok(runtime)) = (config_dir(), runtime_dir()) {
            assert_eq!(runtime.parent(), Some(config.as_path()));
        }
    }

    #[test]
    fn load_layered_reads_the_project_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        write_project(
            dir.path(),
            r#"
            [general]
            model = "from-project"
            "#,
        );

        // `load_layered` also reads the real global file, which the test
        // environment may or may not have — assert only the project layer.
        let config = Config::load_layered(dir.path()).unwrap();
        assert_eq!(config.general.model.as_deref(), Some("from-project"));
    }

    #[test]
    fn load_layered_is_fine_with_no_project_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Config::load_layered(dir.path()).is_ok());
    }

    /// `[web]` merges field-wise like every other table: a project turning
    /// the console on must not erase a globally pinned port, and vice versa.
    #[test]
    fn web_settings_merge_field_wise_across_layers() {
        let mut global = Config {
            web: WebSettings {
                enabled: None,
                port: Some(4321),
                open_browser: Some(false),
            },
            ..Config::default()
        };
        let project = Config {
            web: WebSettings {
                enabled: Some(true),
                port: None,
                open_browser: None,
            },
            ..Config::default()
        };
        global.merge_over(project);
        assert_eq!(global.web.enabled, Some(true));
        assert_eq!(global.web.port, Some(4321));
        assert_eq!(global.web.open_browser, Some(false));
    }

    #[test]
    fn a_saved_config_leaves_no_temp_file_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_private_atomic(&path, "provider = \"ollama\"\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "provider = \"ollama\"\n"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "config.toml")
            .collect();
        assert!(leftovers.is_empty(), "temp file survived: {leftovers:?}");
    }

    /// The point of the rename: a reader either sees the whole old file or the
    /// whole new one. A truncating write has a window where it sees neither,
    /// and this is the only file holding the user's API keys.
    #[test]
    fn a_rewrite_never_leaves_the_file_shorter_than_either_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_private_atomic(&path, "a = 1\nb = 2\nc = 3\n").unwrap();
        write_private_atomic(&path, "a = 9\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 9\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_config_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_private_atomic(&path, "api_key = \"sk-secret\"\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }

    // ---- where a project's history lives -----------------------------------

    #[test]
    fn two_projects_with_the_same_basename_do_not_share_a_history() {
        // The reason the name carries a hash at all: `~/work/api` and
        // `~/clients/acme/api` are both "api", and one silently reading the
        // other's conversations would be worse than any tidiness gained.
        let a = project_store_name(std::path::Path::new("/nowhere/work/api"));
        let b = project_store_name(std::path::Path::new("/nowhere/clients/acme/api"));
        assert_ne!(a, b);
        assert!(a.starts_with("api-"), "{a}");
        assert!(b.starts_with("api-"), "{b}");
    }

    #[test]
    fn the_same_project_always_lands_in_the_same_place() {
        let path = std::path::Path::new("/nowhere/projetos/smith");
        assert_eq!(project_store_name(path), project_store_name(path));
    }

    #[test]
    fn a_name_that_is_not_a_filename_is_made_into_one() {
        let name = project_store_name(std::path::Path::new("/nowhere/my project (v2)"));
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{name} must be safe as a directory name"
        );
    }

    #[test]
    fn adopting_moves_the_old_database_and_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy.db");
        std::fs::write(&legacy, b"history").unwrap();
        let target_dir = dir.path().join("central");

        let moved = adopt_into(&legacy, &target_dir).unwrap();
        assert_eq!(moved.as_deref(), Some(legacy.as_path()));
        assert!(!legacy.exists(), "the old file must be gone");
        assert_eq!(
            std::fs::read(target_dir.join("sessions.db")).unwrap(),
            b"history"
        );
    }

    #[test]
    fn adopting_refuses_to_overwrite_a_history_that_is_already_there() {
        // Two histories is a situation someone can look at; one overwritten by
        // the other is not.
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy.db");
        std::fs::write(&legacy, b"old").unwrap();
        let target_dir = dir.path().join("central");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("sessions.db"), b"current").unwrap();

        assert_eq!(adopt_into(&legacy, &target_dir).unwrap(), None);
        assert!(legacy.exists(), "the legacy file must be left alone");
        assert_eq!(
            std::fs::read(target_dir.join("sessions.db")).unwrap(),
            b"current"
        );
    }
}

#[cfg(test)]
mod update_global_tests {
    use super::*;

    /// `--save` writes the global file, and only what is already global.
    ///
    /// This is the regression: a session runs on the *layered* config — global
    /// with the project's merged over it — and `--save` used to clone that and
    /// write the whole thing back. A project setting `permission_policy =
    /// "skip"` for itself therefore turned the permission prompt off for every
    /// other project on the machine the first time anyone ran `/model --save`
    /// in it, silently, with no mention of permissions anywhere in the command
    /// they typed. Observed in the wild: a global config reading `skip` that
    /// its owner had never set globally.
    #[test]
    fn a_project_override_never_reaches_the_global_file() {
        let dir = tempfile::tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        std::fs::write(
            &global_path,
            "[general]\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-5\"\n",
        )
        .unwrap();

        // What the session is actually running on: the project turned the
        // prompt off for itself and supplied its own key.
        let mut layered = Config::load_path(&global_path).unwrap();
        layered.general.permission_policy = Some("skip".into());
        layered.anthropic.api_key = Some("a-project-scoped-key".into());

        // The user switches model and asks for it to be remembered.
        Config::update_at(&global_path, |global| {
            global.general.model = Some("gpt-4.1".into());
        })
        .unwrap();

        let written = std::fs::read_to_string(&global_path).unwrap();
        assert!(written.contains("gpt-4.1"), "{written}");
        assert!(
            !written.contains("skip"),
            "the project's permission policy became global: {written}"
        );
        assert!(
            !written.contains("a-project-scoped-key"),
            "the project's API key was copied into the global file: {written}"
        );
        // And the layered config the session runs on is untouched by the save.
        assert_eq!(layered.general.permission_policy.as_deref(), Some("skip"));
    }

    /// Writing one field leaves every other global setting alone, including
    /// one another window changed since this session started.
    #[test]
    fn only_the_edited_field_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[general]\nprovider = \"ollama\"\nmodel = \"qwen\"\n\n[openrouter]\napi_key = \"keep-me\"\n",
        )
        .unwrap();

        Config::update_at(&path, |global| {
            global.general.permission_policy = Some("ask".into());
        })
        .unwrap();

        let reloaded = Config::load_path(&path).unwrap();
        assert_eq!(reloaded.general.permission_policy.as_deref(), Some("ask"));
        assert_eq!(reloaded.general.provider.as_deref(), Some("ollama"));
        assert_eq!(reloaded.openrouter.api_key.as_deref(), Some("keep-me"));
    }

    /// A first save on a machine with no config file writes one.
    #[test]
    fn a_missing_global_file_is_created_rather_than_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::update_at(&path, |global| {
            global.general.model = Some("qwen".into());
        })
        .unwrap();
        assert_eq!(
            Config::load_path(&path).unwrap().general.model.as_deref(),
            Some("qwen")
        );
    }
}
