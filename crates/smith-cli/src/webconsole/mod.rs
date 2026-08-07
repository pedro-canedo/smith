//! The web console — a browser frontend for the *running* session.
//!
//! A third consumer of the same contract `smith-tui` and `headless` already
//! drive: `Action`s in, `AgentEvent`s out. Nothing here touches permission
//! logic, the plan gate, checkpoints or pricing — it renders events and
//! submits whitelisted actions. `docs/web-console.md` is the design record.
//!
//! # Relationship to `webconfig`
//!
//! Same security posture, different lifetime. The predicates are shared
//! (`crate::webguard`): loopback bind, per-run token, route whitelist, exact
//! `Host`, `Origin`/`Sec-Fetch-Site`, JSON-only writes. What the config
//! server's module doc forbids — a daemon — this is not: the console lives
//! exactly as long as the interactive session that started it, is aborted
//! with the orchestrator, and cannot outlive or restart the command. And
//! unlike the config server, no secret ever crosses this socket: there is no
//! settings surface in P0, and the one URL that carries the token is the one
//! the user themselves pastes into a browser.
//!
//! # Wire protocol
//!
//! REST + SSE, not a websocket: an SSE `data:` line is one stream-json line
//! verbatim (the public `--output-format stream-json` contract), and every
//! inbound need is a small POST. See `sse.rs` for framing and `dto.rs` for
//! the action whitelist.

pub mod api;
pub mod dto;
pub mod server;
pub mod sse;
pub mod state;

/// The app shell: the built `web/` app (Vite + React + Tailwind, Ember
/// palette), bundled to exactly one committed file — the same `include_str!`
/// posture as `webconfig::PAGE`, and the reason the route whitelist has zero
/// static-asset entries. Rebuild with `scripts/build-web.sh`; a palette test
/// below doubles as the staleness tripwire.
pub const PAGE: &str = include_str!("../../../../web/dist/index.html");

use std::path::PathBuf;
use std::sync::Arc;

use smith_core::{Action, SubmittedAnswer};
use tokio::sync::mpsc;

use crate::webguard::{Guard, RouteAuth, RouteSpec};
use state::Tee;

/// Everything a request handler may reach. One struct so the server loop
/// hands each connection a single clone.
#[derive(Clone)]
pub struct Handles {
    pub guard: Arc<Guard>,
    pub tee: Tee,
    pub action_tx: mpsc::UnboundedSender<Action>,
    pub ask_tx: mpsc::UnboundedSender<SubmittedAnswer>,
    /// The live session's id — the one `/s/<id>` must match.
    pub session_id: String,
    /// Where this project's `sessions.db` lives; handlers open their own
    /// read-only connection per request (SQLite serves concurrent readers).
    pub store_dir: PathBuf,
}

/// The console's route table. Dynamic segments are parsed here — there is
/// still no path resolver, just patterns this match names explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// `GET /` — 302 to the live session's shell, token permitting.
    Root,
    /// `GET /s/<id>` — the app shell.
    Shell { session_id: String },
    /// `GET /api/state` — the projection snapshot.
    State,
    /// `GET /api/events` — the SSE stream.
    Events,
    /// `POST /api/action` — an `ActionDto`.
    SubmitAction,
    /// `POST /api/ask/answer` — resolve a pending ask.
    AskAnswer,
    /// `GET /api/sessions` — this project's session list.
    Sessions,
    /// `GET /api/sessions/<id>/messages` — persisted history.
    SessionMessages { session_id: String },
    /// `GET /api/tasks` — the current board snapshot.
    Tasks,
}

impl Route {
    /// The table, as `webguard::Guard::admit` consumes it.
    ///
    /// `Events` takes its token from the query because the `EventSource`
    /// API cannot set a header; `Root`/`Shell` because a link must be
    /// clickable. Everything else is `X-Smith-Token`.
    pub fn lookup(method: &str, path: &str) -> Option<RouteSpec<Self>> {
        let (route, auth, is_write) = match (method, path) {
            ("GET", "/") => (Self::Root, RouteAuth::QueryToken, false),
            ("GET", "/api/state") => (Self::State, RouteAuth::HeaderToken, false),
            ("GET", "/api/events") => (Self::Events, RouteAuth::QueryToken, false),
            ("POST", "/api/action") => (Self::SubmitAction, RouteAuth::HeaderToken, true),
            ("POST", "/api/ask/answer") => (Self::AskAnswer, RouteAuth::HeaderToken, true),
            ("GET", "/api/sessions") => (Self::Sessions, RouteAuth::HeaderToken, false),
            ("GET", "/api/tasks") => (Self::Tasks, RouteAuth::HeaderToken, false),
            ("GET", path) => {
                if let Some(id) = path.strip_prefix("/s/") {
                    if valid_session_segment(id) {
                        (
                            Self::Shell {
                                session_id: id.to_string(),
                            },
                            RouteAuth::QueryToken,
                            false,
                        )
                    } else {
                        return None;
                    }
                } else if let Some(rest) = path.strip_prefix("/api/sessions/") {
                    let (id, tail) = rest.split_once('/')?;
                    if tail == "messages" && valid_session_segment(id) {
                        (
                            Self::SessionMessages {
                                session_id: id.to_string(),
                            },
                            RouteAuth::HeaderToken,
                            false,
                        )
                    } else {
                        return None;
                    }
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        Some(RouteSpec {
            route,
            auth,
            is_write,
        })
    }
}

/// A session id is a UUID our own store minted: hex and hyphens, bounded.
/// Anything else is off the table before it reaches a query — the same
/// no-path-resolver stance as every other route.
fn valid_session_segment(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests;
