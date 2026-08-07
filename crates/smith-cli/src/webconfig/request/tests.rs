//! This server's table against the shared guard. The predicate suite itself
//! lives with the predicates, in `webguard::tests`.

use super::*;

const HOST: &str = "127.0.0.1:41337";
const TOKEN: &str = "s3cret-token-value";

fn guard() -> Guard {
    Guard {
        host: HOST.to_string(),
        token: TOKEN.to_string(),
    }
}

fn head(method: &str, target: &str, extra: &[(&str, &str)]) -> String {
    let mut out = format!("{method} {target} HTTP/1.1\r\nHost: {HOST}\r\n");
    for (name, value) in extra {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out
}

#[test]
fn the_page_is_served_when_the_url_carries_the_token() {
    let req = guard()
        .admit(
            &head("GET", &format!("/?t={TOKEN}"), &[]),
            "",
            Route::lookup,
        )
        .expect("our own page");
    assert_eq!(req.route, Route::Page);
}

#[test]
fn every_api_route_takes_the_header_token_and_writes_stay_writes() {
    for (method, path, route, is_write) in [
        ("GET", "/api/state", Route::State, false),
        ("GET", "/api/models", Route::Models, false),
        ("POST", "/api/config", Route::Config, true),
        ("POST", "/api/test", Route::Test, true),
        ("POST", "/api/browser", Route::Browser, true),
        ("POST", "/api/close", Route::Close, true),
    ] {
        let mut headers = vec![("X-Smith-Token", TOKEN)];
        if is_write {
            headers.push(("Content-Type", "application/json"));
            headers.push(("Content-Length", "2"));
        }
        let body = if is_write { "{}" } else { "" };
        let req = guard()
            .admit(&head(method, path, &headers), body, Route::lookup)
            .unwrap_or_else(|e| panic!("{method} {path} refused: {e:?}"));
        assert_eq!(req.route, route);

        // ...and never from the query, which would leak through referrers.
        let refused = guard().admit(
            &head(method, &format!("{path}?t={TOKEN}"), &[]),
            "",
            Route::lookup,
        );
        assert_eq!(refused, Err(Refusal::Forbidden("token")), "{method} {path}");
    }
}

#[test]
fn nothing_off_this_table_routes() {
    for (method, path) in [("GET", "/api/close"), ("POST", "/api/state"), ("GET", "/x")] {
        let refused = guard().admit(
            &head(method, path, &[("X-Smith-Token", TOKEN)]),
            "",
            Route::lookup,
        );
        assert_eq!(refused, Err(Refusal::NotFound), "{method} {path}");
    }
}
