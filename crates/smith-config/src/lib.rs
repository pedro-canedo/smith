use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod memory;

pub use memory::{MemoryCache, MemoryScope, MEMORY_FILE_NAME};

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
    /// `web_search` backend settings that are not credentials.
    #[serde(default)]
    pub search: SearchSettings,
    /// Third-party binaries `smith setup` provisioned into `~/.smith/runtime`.
    /// Must stay ahead of `mcp_servers`: TOML forbids a plain table after an
    /// array of tables, so a field serialized after it would produce a file
    /// this same struct could not read back.
    #[serde(default)]
    pub runtime: RuntimeSettings,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
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

/// One entry in `[[mcp_servers]]`: a stdio-transport MCP server smith should
/// connect to at startup and pull tools from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
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
        ] {
            target.api_key = incoming.api_key.or(target.api_key.take());
        }
        self.ollama.base_url = other.ollama.base_url.or(self.ollama.base_url.take());

        let SearchSettings {
            searxng_url,
            market,
        } = other.search;
        self.search.searxng_url = searxng_url.or(self.search.searxng_url.take());
        self.search.market = market.or(self.search.market.take());

        let RuntimeSettings {
            chromium_path,
            chromium_version,
        } = other.runtime;
        self.runtime.chromium_path = chromium_path.or(self.runtime.chromium_path.take());
        self.runtime.chromium_version = chromium_version.or(self.runtime.chromium_version.take());

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
