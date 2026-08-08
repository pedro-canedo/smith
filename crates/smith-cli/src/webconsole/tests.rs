//! The console's route table, and the server end to end over real sockets.

use std::sync::Arc;

use smith_core::{AgentEvent, PermissionRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::state::Tee;
use super::*;

const TOKEN: &str = "test-token-for-the-console";
const SESSION: &str = "11111111-2222-3333-4444-555555555555";

// ---- the route table -------------------------------------------------------

#[test]
fn the_table_maps_every_route_and_only_those() {
    for (method, path, expect) in [
        ("GET", "/", Some(Route::Root)),
        ("GET", "/api/state", Some(Route::State)),
        ("GET", "/api/events", Some(Route::Events)),
        ("POST", "/api/action", Some(Route::SubmitAction)),
        ("POST", "/api/ask/answer", Some(Route::AskAnswer)),
        ("GET", "/api/sessions", Some(Route::Sessions)),
        ("GET", "/api/tasks", Some(Route::Tasks)),
        (
            "GET",
            "/s/abc-123",
            Some(Route::Shell {
                session_id: "abc-123".into(),
            }),
        ),
        (
            "GET",
            "/api/sessions/abc-123/messages",
            Some(Route::SessionMessages {
                session_id: "abc-123".into(),
            }),
        ),
        // Off the table.
        ("POST", "/api/state", None),
        ("GET", "/api/action", None),
        ("GET", "/s/", None),
        ("GET", "/s/../etc", None),
        ("GET", "/api/sessions/abc/turns", None),
        ("GET", "/api/sessions//messages", None),
        ("DELETE", "/api/sessions", None),
    ] {
        let found = Route::lookup(method, path).map(|spec| spec.route);
        assert_eq!(found, expect, "{method} {path}");
    }
}

/// The two query-token exceptions are exactly the two places a header is
/// impossible: the clickable link and the EventSource connection.
#[test]
fn only_the_link_and_the_event_stream_take_the_query_token() {
    use crate::webguard::RouteAuth;
    for (method, path, auth) in [
        ("GET", "/", RouteAuth::QueryToken),
        ("GET", "/s/abc", RouteAuth::QueryToken),
        ("GET", "/api/events", RouteAuth::QueryToken),
        ("GET", "/api/state", RouteAuth::HeaderToken),
        ("GET", "/api/meta", RouteAuth::HeaderToken),
        ("POST", "/api/action", RouteAuth::HeaderToken),
        ("POST", "/api/ask/answer", RouteAuth::HeaderToken),
        ("GET", "/api/sessions", RouteAuth::HeaderToken),
    ] {
        let spec = Route::lookup(method, path).unwrap();
        assert_eq!(spec.auth, auth, "{method} {path}");
    }
}

// ---- the server, end to end ------------------------------------------------

struct Console {
    addr: std::net::SocketAddr,
    host: String,
    action_rx: mpsc::UnboundedReceiver<smith_core::Action>,
    ask_rx: mpsc::UnboundedReceiver<smith_core::SubmittedAnswer>,
    tee: Tee,
    _server: tokio::task::JoinHandle<()>,
}

async fn console() -> Console {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let host = format!("127.0.0.1:{}", addr.port());
    let (action_tx, action_rx) = mpsc::unbounded_channel();
    let (ask_tx, ask_rx) = mpsc::unbounded_channel();
    let tee = Tee::new(SESSION.into(), "ollama".into(), "qwen".into());
    let handles = Handles {
        guard: Arc::new(crate::webguard::Guard {
            host: host.clone(),
            token: TOKEN.into(),
        }),
        tee: tee.clone(),
        meta: Arc::new(super::ConsoleMeta {
            session_id: SESSION.into(),
            provider: "ollama".into(),
            model: "qwen".into(),
            version: "0.0.0-test",
            cwd: "/tmp/project".into(),
            started_at_ms: 1_700_000_000_000,
            links: super::links::links_for(
                &smith_config::Config::default(),
                crate::orchestrator::ProviderKind::Ollama,
            ),
        }),
        action_tx,
        ask_tx,
        session_id: SESSION.into(),
        store_dir: std::path::PathBuf::new(),
    };
    let server = tokio::spawn(server::serve(listener, handles, PAGE));
    Console {
        addr,
        host,
        action_rx,
        ask_rx,
        tee,
        _server: server,
    }
}

/// One request, one response — read to EOF, returned whole.
async fn roundtrip(console: &Console, request: String) -> String {
    let mut stream = TcpStream::connect(console.addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

fn get(console: &Console, target: &str, token: Option<&str>) -> String {
    let token_header = token
        .map(|t| format!("X-Smith-Token: {t}\r\n"))
        .unwrap_or_default();
    format!(
        "GET {target} HTTP/1.1\r\nHost: {}\r\n{token_header}\r\n",
        console.host
    )
}

fn post(console: &Console, target: &str, body: &str) -> String {
    format!(
        "POST {target} HTTP/1.1\r\nHost: {}\r\nX-Smith-Token: {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        console.host,
        body.len()
    )
}

#[tokio::test]
async fn every_api_route_refuses_a_request_without_the_token() {
    let c = console().await;
    for target in ["/api/state", "/api/meta", "/api/sessions", "/api/tasks"] {
        let response = roundtrip(&c, get(&c, target, None)).await;
        assert!(response.starts_with("HTTP/1.1 403"), "{target}: {response}");
    }
    // The shell too — and with the *wrong* token, same answer.
    let response = roundtrip(&c, get(&c, &format!("/s/{SESSION}?t=wrong"), None)).await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
}

#[tokio::test]
async fn the_shell_serves_the_page_for_the_live_session_and_404s_any_other() {
    let c = console().await;
    let response = roundtrip(&c, get(&c, &format!("/s/{SESSION}?t={TOKEN}"), None)).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("Content-Security-Policy"));
    assert!(response.contains("smith console"));

    // A wrong id is a 404, never a redirect that would confirm the real id.
    let response = roundtrip(&c, get(&c, &format!("/s/other-session?t={TOKEN}"), None)).await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(!response.contains(SESSION), "the real id must not leak");
}

#[tokio::test]
async fn the_root_redirects_to_the_live_session() {
    let c = console().await;
    let response = roundtrip(&c, get(&c, &format!("/?t={TOKEN}"), None)).await;
    assert!(response.starts_with("HTTP/1.1 302"), "{response}");
    assert!(response.contains(&format!("Location: /s/{SESSION}?t=")));
}

#[tokio::test]
async fn a_submitted_action_reaches_the_orchestrator_channel_and_the_projection() {
    let mut c = console().await;
    let response = roundtrip(
        &c,
        post(
            &c,
            "/api/action",
            r#"{"type":"submit_message","data":"olá"}"#,
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 202"), "{response}");
    assert!(matches!(
        c.action_rx.recv().await,
        Some(smith_core::Action::SubmitMessage(text)) if text == "olá"
    ));
    // The user's own message is in the snapshot for the next late joiner.
    let projection = c.tee.projection.read().unwrap();
    assert!(matches!(
        projection.transcript.last(),
        Some(state::TranscriptItem::User { text }) if text == "olá"
    ));
}

#[tokio::test]
async fn quit_is_a_bad_request_not_an_action() {
    let mut c = console().await;
    let response = roundtrip(&c, post(&c, "/api/action", r#"{"type":"quit"}"#)).await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert!(
        c.action_rx.try_recv().is_err(),
        "nothing may reach the orchestrator"
    );
}

#[tokio::test]
async fn an_ask_answer_for_a_pending_permission_reaches_the_broker_channel() {
    let mut c = console().await;
    c.tee.projection.write().unwrap().apply(
        1,
        &AgentEvent::PermissionPromptNeeded(PermissionRequest {
            tool_call_id: "call_9".into(),
            tool_name: "run_bash".into(),
            detail: "cargo test".into(),
        }),
    );
    let body = r#"{"kind":"permission","tool_call_id":"call_9","decision":"allow_once"}"#;
    let response = roundtrip(&c, post(&c, "/api/ask/answer", body)).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let submitted = c.ask_rx.recv().await.unwrap();
    assert!(matches!(submitted.source, smith_core::AskSource::Web));
}

/// The polite 404: the ask was already settled (or never existed), and the
/// console says so instead of accepting an answer into the void.
#[tokio::test]
async fn an_ask_answer_for_a_settled_prompt_is_not_found() {
    let mut c = console().await;
    let body = r#"{"kind":"permission","tool_call_id":"ghost","decision":"deny"}"#;
    let response = roundtrip(&c, post(&c, "/api/ask/answer", body)).await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    assert!(c.ask_rx.try_recv().is_err());
}

#[tokio::test]
async fn the_state_snapshot_reflects_what_the_pump_applied() {
    let c = console().await;
    c.tee
        .projection
        .write()
        .unwrap()
        .apply(7, &AgentEvent::AssistantTextDelta("stream".into()));
    let response = roundtrip(&c, get(&c, "/api/state", Some(TOKEN))).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body = response.split("\r\n\r\n").nth(1).unwrap();
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(json["seq"], 7);
    assert_eq!(json["streaming_text"], "stream");
    assert_eq!(json["session_id"], SESSION);
}

/// Two connections served at once — the webconfig loop could not do this,
/// and an SSE console cannot exist without it.
#[tokio::test]
async fn two_concurrent_requests_are_both_served() {
    let c = console().await;
    let first = roundtrip(&c, get(&c, "/api/state", Some(TOKEN)));
    let second = roundtrip(&c, get(&c, "/api/tasks", Some(TOKEN)));
    let (first, second) = tokio::join!(first, second);
    assert!(first.starts_with("HTTP/1.1 200"), "{first}");
    assert!(second.starts_with("HTTP/1.1 200"), "{second}");
}

/// The stream: hello names the current seq, and a pumped event arrives as a
/// frame whose id is its seq and whose data is the stream-json line.
#[tokio::test]
async fn the_event_stream_says_hello_and_then_streams_stamped_events() {
    let c = console().await;
    c.tee.projection.write().unwrap().seq = 3;

    let mut stream = TcpStream::connect(c.addr).await.unwrap();
    stream
        .write_all(get(&c, &format!("/api/events?t={TOKEN}"), None).as_bytes())
        .await
        .unwrap();

    // Read until the hello frame is in.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !String::from_utf8_lossy(&buf).contains("\n\n") {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "stream closed before hello");
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    assert!(text.contains("text/event-stream"), "{text}");
    assert!(
        text.contains(r#"{"type":"hello","data":{"seq":3}}"#),
        "{text}"
    );

    // An event published after the subscription arrives as a stamped frame.
    c.tee
        .broadcast
        .send((4, AgentEvent::AssistantTextDelta("oi".into())))
        .unwrap();
    let mut buf = Vec::new();
    while !String::from_utf8_lossy(&buf).contains("\n\n") {
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "stream closed before the event");
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    assert!(text.contains("id: 4"), "{text}");
    assert!(
        text.contains(r#"data: {"type":"assistant_text_delta","data":"oi"}"#),
        "{text}"
    );
}

/// `webguard`'s predicates hold on this server too — one spot check per
/// family; the full suite lives with the predicates.
#[tokio::test]
async fn the_guard_predicates_apply_to_the_console() {
    let c = console().await;
    let raw =
        format!("GET /api/state HTTP/1.1\r\nHost: evil.com\r\nX-Smith-Token: {TOKEN}\r\n\r\n");
    let response = roundtrip(&c, raw).await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    assert!(
        !response.contains("host"),
        "the refusal reason must not be sent: {response}"
    );
}

// ---- the committed bundle --------------------------------------------------

/// The console paints with the Ember palette — `docs/design-system.md` is
/// the source of truth and `Theme::token_hex` is its machine-readable form.
/// This is also the coarse staleness tripwire: a rebuild that dropped the
/// tokens (or a hand-edit of the bundle) fails here.
#[test]
fn the_committed_console_page_paints_with_the_ember_palette() {
    let theme = smith_tui::Theme::preset(smith_tui::ThemeName::Dark, true);
    for token in ["ember", "amber", "success", "danger", "base", "raised"] {
        let hex = theme.token_hex(token).unwrap();
        assert!(
            PAGE.contains(&hex),
            "the bundle does not carry the {token} token ({hex}) — \
             rebuild with scripts/build-web.sh or fix web/src/index.css"
        );
    }
}

/// One file is the contract: the whitelist has no static-asset routes, so a
/// document-level external script or stylesheet would simply 404.
#[test]
fn the_console_page_is_a_single_self_contained_document() {
    assert!(!PAGE.contains("<script src="), "external script tag");
    assert!(
        !PAGE.contains("<script type=\"module\" src="),
        "external module tag"
    );
    assert!(
        !PAGE.contains("stylesheet\" href="),
        "external stylesheet link"
    );
    assert!(PAGE.contains("smith console"), "the title is gone");
}

/// The console's own URL carries the session token in its query string, and
/// the navigation rail links out to provider dashboards. A referrer policy is
/// what keeps the second from spending the first: without it a click on
/// "OpenRouter" is a cross-origin navigation whose `Referer` is up to the
/// browser's default. Belt and braces — the page also marks every external
/// anchor `rel="noreferrer"` (`links::ConsoleLink::external`).
#[test]
fn the_console_page_never_offers_its_url_as_a_referrer() {
    assert!(
        PAGE.contains("no-referrer"),
        "the referrer policy is gone from the bundle — the page URL carries \
         the session token"
    );
}

#[tokio::test]
async fn the_meta_route_answers_with_the_session_constants_and_its_links() {
    let c = console().await;
    let response = roundtrip(&c, get(&c, "/api/meta", Some(TOKEN))).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body = response.split_once("\r\n\r\n").unwrap().1;
    let meta: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(meta["session_id"], SESSION);
    assert_eq!(meta["model"], "qwen");
    assert_eq!(meta["cwd"], "/tmp/project");
    // Ollama is serving this fixture, so its dashboard is offered and marked
    // as the active one.
    let links = meta["links"].as_array().unwrap();
    let ollama = links.iter().find(|l| l["id"] == "ollama").unwrap();
    assert_eq!(ollama["active"], true);
    assert_eq!(ollama["external"], false);
}
