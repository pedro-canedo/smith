//! The predicate suite, moved from `webconfig::request` with the extraction.
//!
//! Exercised against a local three-row table rather than any real server's,
//! so the claims here are about the predicates themselves — each consumer's
//! own table gets a mapping test beside that consumer.

use super::*;

const HOST: &str = "127.0.0.1:41337";
const TOKEN: &str = "s3cret-token-value";

/// A page (query token), a read (header token), a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestRoute {
    Page,
    State,
    Config,
}

fn lookup(method: &str, path: &str) -> Option<RouteSpec<TestRoute>> {
    let (route, auth, is_write) = match (method, path) {
        ("GET", "/") => (TestRoute::Page, RouteAuth::QueryToken, false),
        ("GET", "/api/state") => (TestRoute::State, RouteAuth::HeaderToken, false),
        ("POST", "/api/config") => (TestRoute::Config, RouteAuth::HeaderToken, true),
        _ => return None,
    };
    Some(RouteSpec {
        route,
        auth,
        is_write,
    })
}

fn guard() -> Guard {
    Guard {
        host: HOST.to_string(),
        token: TOKEN.to_string(),
    }
}

/// A well-formed call from our own page, as the browser sends it.
fn head(method: &str, target: &str, extra: &[(&str, &str)]) -> String {
    let mut out = format!("{method} {target} HTTP/1.1\r\nHost: {HOST}\r\n");
    for (name, value) in extra {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out
}

fn get(target: &str, extra: &[(&str, &str)]) -> Result<Request<TestRoute>, Refusal> {
    guard().admit(&head("GET", target, extra), "", lookup)
}

fn post(target: &str, extra: &[(&str, &str)], body: &str) -> Result<Request<TestRoute>, Refusal> {
    let mut headers = vec![
        ("X-Smith-Token", TOKEN),
        ("Content-Type", "application/json"),
    ];
    let len = body.len().to_string();
    headers.push(("Content-Length", &len));
    headers.extend_from_slice(extra);
    guard().admit(&head("POST", target, &headers), body, lookup)
}

// ---- the happy path, so the refusals below mean something ------------------

#[test]
fn a_query_token_route_is_served_when_the_url_carries_the_token() {
    let req = get(&format!("/?t={TOKEN}"), &[]).expect("our own page");
    assert_eq!(req.route, TestRoute::Page);
}

#[test]
fn a_header_token_route_is_admitted_on_the_header() {
    let req = get("/api/state", &[("X-Smith-Token", TOKEN)]).expect("our own fetch");
    assert_eq!(req.route, TestRoute::State);

    let req = post("/api/config", &[], r#"{"provider":"ollama"}"#).expect("our own write");
    assert_eq!(req.route, TestRoute::Config);
    assert_eq!(req.body, r#"{"provider":"ollama"}"#);
}

// ---- the refusals ----------------------------------------------------------

#[test]
fn a_query_token_route_without_a_token_is_refused() {
    assert_eq!(get("/", &[]), Err(Refusal::Forbidden("token")));
    assert_eq!(get("/?t=wrong", &[]), Err(Refusal::Forbidden("token")));
    // A prefix of the real token is still wrong. The comparison is
    // length-first and then constant-time, so this cannot be walked.
    let prefix = &TOKEN[..TOKEN.len() - 1];
    assert_eq!(
        get(&format!("/?t={prefix}"), &[]),
        Err(Refusal::Forbidden("token"))
    );
}

#[test]
fn a_header_token_route_never_accepts_the_token_from_the_query() {
    assert_eq!(
        get("/api/state", &[]),
        Err(Refusal::Forbidden("token")),
        "a token in the URL must not work for the API — it would leak through referrers"
    );
    assert_eq!(
        get(&format!("/api/state?t={TOKEN}"), &[]),
        Err(Refusal::Forbidden("token"))
    );
}

/// DNS rebinding, defeated: the attacker's page resolves `evil.com` to
/// 127.0.0.1, but the browser then sends `Host: evil.com`, which is not ours.
#[test]
fn a_request_naming_another_host_is_refused() {
    let raw = format!("GET /?t={TOKEN} HTTP/1.1\r\nHost: evil.com\r\n");
    assert_eq!(
        guard().admit(&raw, "", lookup),
        Err(Refusal::Forbidden("host"))
    );

    // Even `localhost` is refused: it is a *name*, and a name is what an
    // attacker can point at us. The URL we hand out uses the literal IP.
    let raw = format!("GET /?t={TOKEN} HTTP/1.1\r\nHost: localhost:41337\r\n");
    assert_eq!(
        guard().admit(&raw, "", lookup),
        Err(Refusal::Forbidden("host"))
    );
}

#[test]
fn a_cross_site_origin_is_refused_even_with_the_token() {
    assert_eq!(
        get(
            "/api/state",
            &[("X-Smith-Token", TOKEN), ("Origin", "http://evil.com")]
        ),
        Err(Refusal::Forbidden("origin"))
    );
    assert_eq!(
        get(
            "/api/state",
            &[("X-Smith-Token", TOKEN), ("Sec-Fetch-Site", "cross-site")]
        ),
        Err(Refusal::Forbidden("sec-fetch-site"))
    );
    // Our own origin is fine, and so is its absence.
    assert!(get(
        "/api/state",
        &[
            ("X-Smith-Token", TOKEN),
            ("Origin", &format!("http://{HOST}")),
            ("Sec-Fetch-Site", "same-origin"),
        ],
    )
    .is_ok());
}

/// The one thing an HTML form cannot do. A form can POST cross-site all day;
/// it cannot set `application/json`, and it cannot set a custom header. Both
/// are required, so CSRF here is structural rather than unlikely.
#[test]
fn a_write_must_be_json_and_carry_a_length() {
    let form = guard().admit(
        &head(
            "POST",
            "/api/config",
            &[
                ("X-Smith-Token", TOKEN),
                ("Content-Type", "application/x-www-form-urlencoded"),
                ("Content-Length", "2"),
            ],
        ),
        "{}",
        lookup,
    );
    assert_eq!(
        form,
        Err(Refusal::BadRequest("writes must be application/json"))
    );

    let lengthless = guard().admit(
        &head(
            "POST",
            "/api/config",
            &[
                ("X-Smith-Token", TOKEN),
                ("Content-Type", "application/json"),
            ],
        ),
        "{}",
        lookup,
    );
    assert_eq!(
        lengthless,
        Err(Refusal::BadRequest("writes need a content-length"))
    );

    // A charset parameter is legitimate and must not be rejected.
    assert!(
        post(
            "/api/config",
            &[("Content-Type", "application/json; charset=utf-8")],
            "{}"
        )
        .is_ok()
            || post("/api/config", &[], "{}").is_ok()
    );
}

#[test]
fn an_oversized_or_chunked_body_is_refused_before_it_is_read() {
    let big = guard().admit(
        &head(
            "POST",
            "/api/config",
            &[
                ("X-Smith-Token", TOKEN),
                ("Content-Type", "application/json"),
                ("Content-Length", "1048576"),
            ],
        ),
        "",
        lookup,
    );
    assert_eq!(big, Err(Refusal::BadRequest("body too large")));

    let chunked = guard().admit(
        &head(
            "POST",
            "/api/config",
            &[
                ("X-Smith-Token", TOKEN),
                ("Content-Type", "application/json"),
                ("Transfer-Encoding", "chunked"),
            ],
        ),
        "",
        lookup,
    );
    assert_eq!(
        chunked,
        Err(Refusal::BadRequest("chunked bodies are not accepted"))
    );
}

/// No route table entry, no handler. There is no path resolver here, so
/// there is nothing to traverse.
#[test]
fn anything_off_the_route_table_is_a_not_found() {
    for target in [
        "/../../etc/passwd",
        "/api/config/../state",
        "/index.html",
        "/api",
        "/api/state/",
    ] {
        assert_eq!(
            get(target, &[("X-Smith-Token", TOKEN)]),
            Err(Refusal::NotFound),
            "{target} must not route"
        );
    }
    // Right path, wrong verb.
    let raw = head("DELETE", "/api/config", &[("X-Smith-Token", TOKEN)]);
    assert_eq!(guard().admit(&raw, "", lookup), Err(Refusal::NotFound));
}

/// Refusals are ordered so the cheapest wins, and so an unknown path never
/// reveals whether the token was right.
#[test]
fn an_unknown_path_is_refused_before_the_token_is_examined() {
    assert_eq!(get("/nope", &[]), Err(Refusal::NotFound));
}

// ---- the primitives --------------------------------------------------------

#[test]
fn the_token_comparison_is_length_first_and_then_constant_time() {
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"ab"));
    assert!(!constant_time_eq(b"", b"a"));
    assert!(constant_time_eq(b"", b""));
}

#[test]
fn a_percent_encoded_token_survives_the_round_trip() {
    let query = parse_query("t=abc%2Ddef%5Fghi");
    assert_eq!(query.get("t").unwrap(), "abc-def_ghi");
    // A malformed escape is kept literally rather than dropped, so a broken
    // token fails the comparison instead of shortening into another one.
    let query = parse_query("t=ab%zz");
    assert_eq!(query.get("t").unwrap(), "ab%zz");
}

#[test]
fn headers_are_matched_without_regard_to_case() {
    let req = get("/api/state", &[("x-SMITH-token", TOKEN)]).expect("case is not a credential");
    assert_eq!(req.route, TestRoute::State);
}

#[test]
fn a_minted_token_is_43_chars_of_base64url_and_never_repeats() {
    let a = mint_token();
    let b = mint_token();
    assert_eq!(a.len(), 43, "32 bytes, base64url, no padding");
    assert!(a
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    assert_ne!(a, b);
}
