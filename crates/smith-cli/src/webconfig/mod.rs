//! `smith setup web` — the wizard, in a browser, for people who do not live
//! in a terminal.
//!
//! Hand-rolled on `tokio::net::TcpListener` rather than a framework. The whole
//! surface is six routes, all loopback, all small JSON plus one static
//! document — and the security model in `request.rs` is a *whitelist*, which
//! is easier to write and audit from scratch than to configure a router into.
//! A framework's default is "route it and let the handler decide"; ours is
//! "refuse unless every predicate holds". `smith-mcp`'s test server is the
//! existing precedent that a ~30-line request reader is enough here.
//!
//! Three properties this module is responsible for, none of which are
//! negotiable:
//!
//! - **Loopback only, ephemeral port.** This endpoint reads and writes the
//!   file holding every API key on the machine. Same reasoning as
//!   `ninerouter_args`' `--host 127.0.0.1`, and pinned by the same kind of
//!   test.
//! - **One-shot.** It exits on the Done button, on idle, on an absolute
//!   deadline, or on Ctrl-C. A credential-writing endpoint that outlives the
//!   command that started it is the thing being avoided, so there is no
//!   `smith serve`, no daemon, and no starting it alongside a chat session.
//! - **Secrets go one way.** The browser is told whether a key is set, never
//!   what it is — not even a suffix. See `api::state`.

pub mod api;
pub mod request;

use std::time::Duration;

use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use request::{Guard, Refusal, Route, MAX_BODY};

/// How long a connection may take to send its request line and headers.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// No interaction for this long and the server stops. A browser tab left open
/// over lunch must not leave a config writer listening.
const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// A hard ceiling regardless of activity.
const MAX_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// The page, with its styles and script inline.
///
/// One `include_str!`, matching the only embedding precedent in the workspace
/// (`smith_tui::banner`). `runtime.rs` argues against embedding, but that
/// argument is explicitly about ~100 MB per platform breaking cargo-dist,
/// Homebrew and binstall; a text file measured in kilobytes is not that, and
/// citing it here would be a misuse of it.
///
/// Deliberately not a directory of assets: serving one needs a path resolver,
/// and a path resolver is the last thing worth hand-writing on a socket that
/// can rewrite `~/.smith/config.toml`.
const PAGE: &str = include_str!("ui.html");

/// Runs the config UI until the user is done with it.
pub async fn run(no_browser: bool, port: Option<u16>) -> Result<(), String> {
    // Port 0 asks the OS for a free one. A fixed port would be predictable,
    // and predictable is the one property a local credential endpoint should
    // not have.
    let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
        .await
        .map_err(|e| format!("cannot listen on 127.0.0.1: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("cannot read the local address: {e}"))?;

    let guard = Guard {
        host: format!("127.0.0.1:{}", addr.port()),
        token: mint_token(),
    };
    let url = format!("http://{}/?t={}", guard.host, guard.token);

    // Printed before any attempt to open a browser, always. Over SSH, in a
    // container, or on a box with no desktop, this line *is* the feature —
    // `ssh -L` and a paste do the rest.
    println!("smith setup — open this in a browser:\n");
    println!("  {url}\n");
    println!("The link carries a one-time key for this run. Close the page or");
    println!("press Ctrl-C when you are done; the server stops with it.\n");

    if !no_browser {
        open_browser(&url);
    }

    serve(listener, guard).await
}

/// 244 bits from the OS CSPRNG, base64url, never written to disk or logged.
///
/// Two v4 UUIDs rather than one: 122 bits is already beyond guessing, but the
/// token is the only thing standing between another process on this machine
/// and the user's API keys, and the second one is free.
fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

async fn serve(listener: TcpListener, guard: Guard) -> Result<(), String> {
    let started = tokio::time::Instant::now();
    let mut last_seen = tokio::time::Instant::now();

    loop {
        let idle_left = IDLE_TIMEOUT.saturating_sub(last_seen.elapsed());
        let life_left = MAX_LIFETIME.saturating_sub(started.elapsed());
        let deadline = idle_left.min(life_left);
        if deadline.is_zero() {
            println!("smith: config server timed out — re-run `smith setup web` to continue.");
            return Ok(());
        }

        let accepted = tokio::select! {
            result = listener.accept() => result,
            _ = tokio::time::sleep(deadline) => continue,
            _ = tokio::signal::ctrl_c() => {
                println!("\nsmith: config server stopped.");
                return Ok(());
            }
        };
        let Ok((stream, _)) = accepted else { continue };
        last_seen = tokio::time::Instant::now();

        // One request per connection, handled inline. There is no concurrency
        // to win here — a single human is filling in a form — and serialising
        // means the config file has exactly one writer at a time.
        if handle(stream, &guard).await {
            println!("smith: done — config saved to ~/.smith/config.toml.");
            return Ok(());
        }
    }
}

/// Returns true when the server should stop.
async fn handle(mut stream: TcpStream, guard: &Guard) -> bool {
    let Ok(Ok(raw)) = tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await else {
        return false;
    };
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));

    let (status, content_type, payload) = match guard.admit(head, body) {
        Ok(req) if req.route == Route::Close => {
            let _ = write_response(&mut stream, 200, "application/json", "{\"ok\":true}").await;
            return true;
        }
        Ok(req) if req.route == Route::Page => (200, "text/html; charset=utf-8", PAGE.to_string()),
        Ok(req) => {
            let (status, body) = api::dispatch(req).await;
            (status, "application/json", body)
        }
        // The refusal reason is never sent. An attacker learning *which*
        // predicate they failed is an attacker with a hill to climb.
        Err(Refusal::NotFound) => (404, "text/plain", "not found\n".to_string()),
        Err(Refusal::Forbidden(_)) => (403, "text/plain", "forbidden\n".to_string()),
        Err(Refusal::BadRequest(_)) => (400, "text/plain", "bad request\n".to_string()),
    };

    let _ = write_response(&mut stream, status, content_type, &payload).await;
    false
}

/// Reads until the header terminator, then the declared body. Capped so a
/// connection cannot make us allocate on its say-so.
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
        .find_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .or_else(|| {
            head.lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse().ok())
        })
}

async fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
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
        // Inline script and style are allowed because the page *is* one file;
        // everything else is denied, including any outbound connection that
        // is not back to us. A config page that can talk to the internet is a
        // config page that can exfiltrate what the user just typed.
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

/// Best effort, and never an error.
///
/// Nothing in the workspace opened a browser before this, and the provisioned
/// `chrome-headless-shell` cannot — it hardcodes `--headless … --dump-dom`.
/// So this is a small platform table, and its failure costs nothing because
/// the URL was already printed.
///
/// WSL leads on purpose: `xdg-open` exists there and does nothing useful, so
/// a Linux-first table would silently fail on the one platform this was
/// developed against.
fn open_browser(url: &str) {
    let candidates: Vec<(&str, Vec<&str>)> = if is_wsl() {
        vec![
            ("wslview", vec![url]),
            (
                "powershell.exe",
                vec!["-NoProfile", "-Command", "Start-Process", url],
            ),
        ]
    } else if cfg!(target_os = "macos") {
        vec![("open", vec![url])]
    } else if cfg!(target_os = "windows") {
        vec![("cmd", vec!["/C", "start", "", url])]
    } else {
        vec![("xdg-open", vec![url])]
    };

    for (program, args) in candidates {
        let spawned = std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return;
        }
    }
    // Silent: the URL is on screen, and a failed spawn is not the user's
    // problem to read about.
}

fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some()
}

#[cfg(test)]
mod tests;
