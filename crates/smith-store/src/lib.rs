//! Session/history persistence (SQLite) and the model catalogue.
//!
//! Configuration deliberately lives in `smith-config` rather than here:
//! reading a TOML file should not pull in `rusqlite`, whose `bundled` feature
//! compiles SQLite from C and dominates a cold build — and `smith setup`,
//! which touches no database at all, was paying that cost.

pub mod models;
pub mod session;

pub use models::{is_known_provider, known_models};
pub use session::{
    SessionError, SessionStore, SessionSummary, TurnRecord, TurnTotals, SCHEMA_VERSION,
};
