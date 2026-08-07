//! The config server's route table.
//!
//! The predicates that admit or refuse a request live in [`crate::webguard`]
//! now — shared with the web console, because a security check that exists
//! twice is one that will be fixed once. What stays here is the part that is
//! genuinely this server's: which method/path pairs exist at all, where each
//! route's token may ride, and which routes are writes.

use crate::webguard::{RouteAuth, RouteSpec};

pub use crate::webguard::{Guard, Refusal, MAX_BODY};

/// An admitted request against this server's table.
pub type Request = crate::webguard::Request<Route>;

/// Routes, exhaustively. Anything not on this list is a 404 before any header
/// is even looked at, so there is no path resolver to get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Page,
    State,
    Models,
    Config,
    Test,
    Browser,
    Close,
}

impl Route {
    /// The table, as [`crate::webguard::Guard::admit`] consumes it.
    pub fn lookup(method: &str, path: &str) -> Option<RouteSpec<Self>> {
        // Only the page takes its token from the URL — a link must be
        // clickable. Every API call carries the header.
        let (route, auth, is_write) = match (method, path) {
            ("GET", "/") => (Self::Page, RouteAuth::QueryToken, false),
            ("GET", "/api/state") => (Self::State, RouteAuth::HeaderToken, false),
            ("GET", "/api/models") => (Self::Models, RouteAuth::HeaderToken, false),
            ("POST", "/api/config") => (Self::Config, RouteAuth::HeaderToken, true),
            ("POST", "/api/test") => (Self::Test, RouteAuth::HeaderToken, true),
            ("POST", "/api/browser") => (Self::Browser, RouteAuth::HeaderToken, true),
            ("POST", "/api/close") => (Self::Close, RouteAuth::HeaderToken, true),
            _ => return None,
        };
        Some(RouteSpec {
            route,
            auth,
            is_write,
        })
    }
}

#[cfg(test)]
mod tests;
