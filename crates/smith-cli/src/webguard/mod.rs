//! The security predicates every local smith HTTP endpoint shares.
//!
//! Extracted from `webconfig::request` so the config server and the web
//! console admit requests through the *same* audited code — a predicate that
//! exists twice is a predicate that will be fixed once. The philosophy is
//! unchanged: this is a whitelist, not a router. Method and path must match a
//! fixed table the caller supplies, five headers must each hold exactly one
//! shape, and everything else is refused **before the body is read**.
//!
//! The threat model is not the internet. It is a process on this machine, or
//! a page the user already has open in a browser, trying to reach a
//! privileged loopback port. TLS and accounts are irrelevant to that; origin
//! authentication and secrecy of the token are everything.
//!
//! What is generic here, and why: the route table and the token placement.
//! The config page reads its token from `?t=` because a link must be
//! clickable; the console's SSE endpoint must too, because the EventSource
//! API cannot set a header. Everything else takes `X-Smith-Token`. That
//! per-route choice is [`RouteAuth`], declared in the caller's table rather
//! than special-cased in here.

use std::collections::HashMap;

use base64::Engine;

/// The largest body worth reading. Every write is a small JSON object; a
/// megabyte of it is not a payload, it is someone finding out what happens.
pub const MAX_BODY: usize = 64 * 1024;

/// Where a route's token may ride.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteAuth {
    /// `?t=` in the URL — for the two places a header is impossible: the
    /// link the user clicks, and an `EventSource` connection.
    QueryToken,
    /// The `X-Smith-Token` header — everything else. A browser will not
    /// attach a custom header cross-site, which is half of what makes CSRF
    /// structural rather than unlikely (the JSON content type is the other).
    HeaderToken,
}

/// One row of a caller's route table.
#[derive(Debug, Clone, Copy)]
pub struct RouteSpec<R> {
    pub route: R,
    pub auth: RouteAuth,
    /// Writes must additionally be `application/json` with a declared,
    /// capped `Content-Length`.
    pub is_write: bool,
}

/// An admitted request.
#[derive(Debug, PartialEq, Eq)]
pub struct Request<R> {
    pub route: R,
    pub query: HashMap<String, String>,
    pub body: String,
}

/// Why a request was refused. Each maps to a status, and none of them says
/// more than the sender already knows — an attacker learning *which* check
/// they failed is an attacker with a hill to climb.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No route. 404.
    NotFound,
    /// A predicate failed. 403.
    Forbidden(&'static str),
    /// Unparseable, oversized, or chunked. 400.
    BadRequest(&'static str),
}

pub struct Guard {
    /// The exact `Host` this server answers to, e.g. `127.0.0.1:41337`.
    pub host: String,
    /// The token minted for this invocation.
    pub token: String,
}

impl Guard {
    /// Every predicate, in the order that lets the cheapest refusal win.
    ///
    /// `head` is the request line plus headers, already split from the body.
    /// `lookup` is the caller's route table: `(method, path)` to a spec, or
    /// `None` for a 404 before any header is examined.
    pub fn admit<R>(
        &self,
        head: &str,
        body: &str,
        lookup: impl Fn(&str, &str) -> Option<RouteSpec<R>>,
    ) -> Result<Request<R>, Refusal> {
        let mut lines = head.split("\r\n");
        let request_line = lines.next().ok_or(Refusal::BadRequest("no request line"))?;
        let mut parts = request_line.split(' ');
        let method = parts.next().ok_or(Refusal::BadRequest("no method"))?;
        let target = parts.next().ok_or(Refusal::BadRequest("no target"))?;
        let version = parts.next().unwrap_or("");
        if !version.starts_with("HTTP/1.") {
            return Err(Refusal::BadRequest("not HTTP/1.x"));
        }

        let (path, query_string) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };
        let spec = lookup(method, path).ok_or(Refusal::NotFound)?;

        let headers = parse_headers(lines);

        // `Transfer-Encoding` before anything that reads a length: we control
        // the only client, so chunked is never legitimate here, and refusing
        // it outright is how request smuggling stops being a question.
        if headers.contains_key("transfer-encoding") {
            return Err(Refusal::BadRequest("chunked bodies are not accepted"));
        }

        // Host is pinned to a literal IP. This is what defeats DNS rebinding:
        // an attacker's page can resolve their hostname to 127.0.0.1, but the
        // browser then sends *their* hostname here, and it will never be ours.
        match headers.get("host") {
            Some(h) if h == &self.host => {}
            _ => return Err(Refusal::Forbidden("host")),
        }

        // Absent is fine — a same-origin GET may omit it. Present and
        // different is a cross-site request.
        if let Some(origin) = headers.get("origin") {
            if origin != &format!("http://{}", self.host) {
                return Err(Refusal::Forbidden("origin"));
            }
        }
        if let Some(site) = headers.get("sec-fetch-site") {
            if site != "same-origin" && site != "none" {
                return Err(Refusal::Forbidden("sec-fetch-site"));
            }
        }

        let query = parse_query(query_string);

        let presented = match spec.auth {
            RouteAuth::QueryToken => query.get("t").map(String::as_str),
            RouteAuth::HeaderToken => headers.get("x-smith-token").map(String::as_str),
        };
        match presented {
            Some(t) if constant_time_eq(t.as_bytes(), self.token.as_bytes()) => {}
            _ => return Err(Refusal::Forbidden("token")),
        }

        if spec.is_write {
            // An HTML form can POST cross-site, but it cannot set this
            // content type and it cannot set a custom header. Requiring both
            // is what makes CSRF structurally impossible rather than unlikely.
            match headers.get("content-type") {
                Some(ct) if ct.split(';').next().unwrap_or("").trim() == "application/json" => {}
                _ => return Err(Refusal::BadRequest("writes must be application/json")),
            }
            let declared: usize = headers
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .ok_or(Refusal::BadRequest("writes need a content-length"))?;
            if declared > MAX_BODY {
                return Err(Refusal::BadRequest("body too large"));
            }
        }

        Ok(Request {
            route: spec.route,
            query,
            body: body.to_string(),
        })
    }
}

/// 244 bits from the OS CSPRNG, base64url, never written to disk or logged.
///
/// Two v4 UUIDs rather than one: 122 bits is already beyond guessing, but the
/// token is the only thing standing between another process on this machine
/// and a privileged endpoint, and the second one is free.
pub fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Lowercased names, trimmed values, last one wins. Duplicated headers are
/// not an attack surface here because every value is compared for equality
/// against a single expected string.
fn parse_headers<'a>(lines: impl Iterator<Item = &'a str>) -> HashMap<String, String> {
    lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect()
}

fn parse_query(raw: &str) -> HashMap<String, String> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), percent_decode(v)))
        .collect()
}

/// Enough of percent-decoding for a base64url token: `%XX` and `+`.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 3 <= bytes.len() => {
                match u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte as char);
                        i += 3;
                    }
                    // Not a valid escape: keep the `%` and carry on from the
                    // next byte. Advancing past all three would *drop* the two
                    // characters after it, which is how a malformed token
                    // shortens into a different one instead of failing.
                    Err(_) => {
                        out.push('%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Compares without leaking where the mismatch was.
///
/// `==` on a secret returns as soon as two bytes differ, and the time that
/// takes is a measurement of how much of the token the caller already has.
/// Over loopback that measurement is very good.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests;
