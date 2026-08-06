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
    /// Which palette the TUI paints in. Must stay ahead of `runtime` for the
    /// ordering reason below *and* because its own `colors` field serializes
    /// as a nested table: a plain-table field written after it would land
    /// inside `[theme.colors]`.
    #[serde(default)]
    pub theme: ThemeSettings,
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
pub struct OllamaSettings {
    pub base_url: Option<String>,
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
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434/v1";

/// `~/.smith` — the only place secrets live. Session history is stored
/// per-project instead (see the M7 milestone), separately from this file.
pub fn config_dir() -> Result<PathBuf, ConfigError> {
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

        let SearchSettings {
            backend,
            searxng_url,
            market,
        } = other.search;
        self.search.backend = backend.or(self.search.backend.take());
        self.search.searxng_url = searxng_url.or(self.search.searxng_url.take());
        self.search.market = market.or(self.search.market.take());

        // A project may restyle smith without restating the whole palette, so
        // the overrides merge per token rather than wholesale — the same rule
        // the scalar fields above follow, one level down.
        let ThemeSettings { name, colors } = other.theme;
        self.theme.name = name.or(self.theme.name.take());
        self.theme.colors.extend(colors);

        let RuntimeSettings {
            chromium_path,
            chromium_version,
        } = other.runtime;
        self.runtime.chromium_path = chromium_path.or(self.runtime.chromium_path.take());
        self.runtime.chromium_version = chromium_version.or(self.runtime.chromium_version.take());

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
    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        set_permissions(&dir, 0o700)?;

        let path = config_path()?;
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        set_permissions(&path, 0o600)?;

        Ok(())
    }
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
}
