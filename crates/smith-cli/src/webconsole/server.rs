//! The console's accept loop.
//!
//! Deliberately not `webconfig`'s: that loop is one-shot and serialized
//! because a single human fills in a form there. A console serves one
//! long-lived SSE stream per tab *plus* transient POSTs, concurrently — so
//! each connection gets its own task, bounded by a semaphore, and there are
//! no idle/lifetime timers: the lifetime bound is the session itself, whose
//! end aborts this task wholesale.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::webguard::{Refusal, MAX_BODY};

use super::{api, sse, Handles, Route};

/// Concurrent connections served; beyond this, accepts wait. Far above any
/// legitimate use (a tab is one stream plus transient fetches) and far below
/// anything that could matter to the machine.
const MAX_CONNECTIONS: usize = 32;

/// How long a connection may take to deliver its request head and body.
/// Applies to the read only — an admitted SSE stream then lives unbounded.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Serves until aborted. The caller bound the listener (it needed the port
/// for the URL) and owns this future's lifetime alongside the orchestrator's.
pub async fn serve(listener: TcpListener, handles: Handles, page: &'static str) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let Ok(permit) = permits.clone().acquire_owned().await else {
            break; // semaphore closed — only happens at teardown
        };
        let handles = handles.clone();
        tokio::spawn(async move {
            let _permit = permit;
            handle(stream, handles, page).await;
        });
    }
}

async fn handle(mut stream: TcpStream, handles: Handles, page: &'static str) {
    let Ok(Ok(raw)) = tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await else {
        return;
    };
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));

    let admitted = handles.guard.admit(head, body, Route::lookup);
    let (status, content_type, payload) = match admitted {
        Ok(request) => match request.route {
            Route::Root => {
                // The redirect target carries the token: it is the same URL
                // class the user already holds (the link itself).
                let location = format!("/s/{}?t={}", handles.session_id, handles.guard.token);
                let _ = write_redirect(&mut stream, &location).await;
                return;
            }
            Route::Shell { session_id } => {
                if session_id == handles.session_id {
                    (200, "text/html; charset=utf-8", page.to_string())
                } else {
                    // A wrong id is a plain 404 — never a redirect, which
                    // would confirm the real id to a guesser.
                    (404, "text/plain", "not found\n".to_string())
                }
            }
            Route::Events => {
                serve_events(stream, &handles).await;
                return;
            }
            route => {
                let (status, body) = api::dispatch(&handles, route, &request.body).await;
                (status, "application/json", body)
            }
        },
        // Refusal reasons are never sent — `webguard`'s rule, kept.
        Err(Refusal::NotFound) => (404, "text/plain", "not found\n".to_string()),
        Err(Refusal::Forbidden(_)) => (403, "text/plain", "forbidden\n".to_string()),
        Err(Refusal::BadRequest(_)) => (400, "text/plain", "bad request\n".to_string()),
    };
    let _ = write_response(&mut stream, status, content_type, &payload).await;
}

/// Subscribes, says hello, and hands the socket to the streamer.
///
/// Subscribe *before* reading the projection's seq: events between the two
/// are then duplicated (client discards by id), never lost. The other order
/// loses whatever lands in the gap.
async fn serve_events(stream: TcpStream, handles: &Handles) {
    let events = handles.tee.broadcast.subscribe();
    let seq = handles
        .tee
        .projection
        .read()
        .map(|p| p.seq)
        .unwrap_or_default();

    let (_reader, mut writer) = tokio::io::split(stream);
    if writer.write_all(sse::SSE_HEAD.as_bytes()).await.is_err() {
        return;
    }
    if writer
        .write_all(sse::hello_frame(seq).as_bytes())
        .await
        .is_err()
    {
        return;
    }
    let _ = writer.flush().await;
    sse::stream(writer, events).await;
}

/// Reads until the header terminator, then the declared body — the same
/// bounded reader `webconfig` uses, duplicated because that one is private
/// and this one may be handed a socket that never sends a body at all.
async fn read_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];

    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_BODY * 2 {
            break;
        }
        let Some(end) = find_header_end(&buf) else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        let declared = content_length(&head).unwrap_or(0).min(MAX_BODY);
        if buf.len() >= end + 4 + declared {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

async fn write_redirect(stream: &mut TcpStream, location: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 302 Found\r\n\
         Location: {location}\r\n\
         Content-Length: 0\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n",
        body.len()
    );
    if content_type.starts_with("text/html") {
        // Same CSP as the config page, same reasoning: the app is one file,
        // inline script/style are what "one file" means, and the only place
        // it may connect to is us.
        head.push_str(
            "Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; \
             style-src 'unsafe-inline'; connect-src 'self'; img-src data:; \
             form-action 'none'; frame-ancestors 'none'; base-uri 'none'\r\n",
        );
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}
