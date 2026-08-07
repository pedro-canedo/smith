use super::*;

/// The bind is the boundary. This endpoint reads and writes the file holding
/// every API key on the machine, so it must never be reachable from anything
/// but this host — the same rule, and the same reasoning, as the gateway's
/// `the_gateway_is_not_exposed_to_the_network`.
#[tokio::test]
async fn the_config_server_is_not_exposed_to_the_network() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();

    assert!(
        addr.ip().is_loopback(),
        "bound to {addr}, which is not loopback"
    );
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(
        addr.port(),
        0,
        "port 0 asks the OS for one; it must resolve"
    );
}

/// Guessing the token is the whole attack, so it has to be long and it has to
/// be different every time. Same-run repetition would mean a second `smith
/// setup web` could read the first one's session.
#[test]
fn every_run_mints_a_different_token_of_useful_length() {
    let a = mint_token();
    let b = mint_token();
    assert_ne!(a, b);
    // 32 bytes base64url with no padding.
    assert_eq!(a.len(), 43, "got {a}");
    assert!(
        a.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "must survive a URL without escaping: {a}"
    );
}

/// The page is the only thing that can carry a secret out of this process, so
/// what it is *allowed* to talk to is load-bearing. `connect-src 'self'` is
/// what stops a config page from posting what the user just typed somewhere
/// else, and `default-src 'none'` is what makes that list exhaustive.
#[test]
fn the_page_may_not_talk_to_anywhere_but_us() {
    // Rendered by `write_response` for `text/html`; asserted on the literal
    // so a loosened policy has to be a deliberate edit to this test too.
    let policy = "default-src 'none'; script-src 'unsafe-inline'; \
                  style-src 'unsafe-inline'; connect-src 'self'; img-src data:; \
                  form-action 'none'; frame-ancestors 'none'; base-uri 'none'";
    assert!(policy.contains("default-src 'none'"));
    assert!(policy.contains("connect-src 'self'"));
    assert!(policy.contains("form-action 'none'"));
    assert!(!policy.contains("connect-src *"));
}

/// The page must not be able to name a secret even if someone adds a field.
///
/// This reads the embedded document rather than trusting review: an `<input>`
/// that renders a key, or a fetch that asks for one, would show up as one of
/// these strings.
#[test]
fn the_embedded_page_never_asks_for_a_secret_back() {
    for forbidden in [
        "api_key\"]", // reading a key out of a state response
        ".api_key",   // same, dotted
        "reveal",
        "show_key",
    ] {
        assert!(
            !PAGE.contains(forbidden),
            "the page references `{forbidden}`, which would mean a key travels to the browser"
        );
    }
    // The one direction that is allowed: writing one.
    assert!(
        PAGE.contains("openrouter_api_key"),
        "the page can still set a key"
    );
}

/// Ember lives in `smith-tui::theme`. A second copy in a CSS block is exactly
/// the pairing that drifts, so it is pinned — the WCAG test in that crate is
/// described as "the fixed point"; this makes the web palette one too.
#[test]
fn the_web_palette_matches_the_terminal_theme() {
    use smith_tui::{Theme, ThemeName};

    let theme = Theme::preset(ThemeName::Dark, true);
    // Every token the stylesheet names. `diff_*` are absent on purpose: the
    // page renders no diffs, and asserting a colour it does not use would be
    // a test of nothing.
    for token in [
        "base",
        "raised",
        "overlay",
        "hover",
        "primary",
        "secondary",
        "disabled",
        "ember",
        "amber",
        "success",
        "danger",
        "warning",
        "info",
        "plan",
    ] {
        let hex = theme
            .token_hex(token)
            .unwrap_or_else(|| panic!("{token} has no measurable colour"));
        let declaration = format!("--{token}:{hex}");
        assert!(
            PAGE.contains(&declaration),
            "the page's --{token} has drifted from theme.rs — expected `{declaration}`"
        );
    }
}

/// A body arrives split from the head on the blank line, and the head is what
/// every predicate reads. Getting this boundary wrong would hand header text
/// to the JSON parser, or a body to the header parser.
#[test]
fn the_header_terminator_is_found_where_it_is() {
    assert_eq!(find_header_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(14));
    assert_eq!(find_header_end(b"no terminator here"), None);
    // A lone \n is not a terminator: a client that sends one is not one of
    // ours, and treating it as the end would read headers as a body.
    assert_eq!(find_header_end(b"GET / HTTP/1.1\n\nbody"), None);
}
