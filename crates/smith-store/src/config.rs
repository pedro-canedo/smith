use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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

impl Config {
    pub fn load() -> Result<Config, ConfigError> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)?;
        Ok(toml::from_str(&text)?)
    }

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
}
