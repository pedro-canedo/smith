//! Session/history persistence (SQLite) and global + project config loading.

pub mod config;
pub mod models;
pub mod session;

pub use config::{Config, ConfigError, DEFAULT_OLLAMA_BASE_URL, OLLAMA_HOST};
pub use models::{is_known_provider, known_models};
pub use session::{SessionError, SessionStore, SessionSummary};
