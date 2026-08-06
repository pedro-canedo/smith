//! The two URL-based MCP transports.
//!
//! * [`HttpTransport`] — "Streamable HTTP": every client message is a POST to
//!   one endpoint, and the reply comes back either as a JSON body or as an SSE
//!   stream on that same response.
//! * [`SseTransport`] — the older HTTP+SSE pair: a long-lived GET carries every
//!   server→client message, and the server names a separate POST endpoint in
//!   its first `endpoint` event.
//!
//! Both funnel their messages into the same [`Incoming`] channel stdio uses, so
//! `McpClient` cannot tell them apart.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::Value;
use tokio::sync::{oneshot, Mutex};

use crate::client::McpError;
use crate::transport::{Incoming, IncomingSender, Transport, TransportKind};

/// How long the HTTP+SSE handshake waits for the server to name its POST
/// endpoint. A server that opens the stream and then says nothing is the exact
/// failure this bounds: without it, `connect` would wait for the GET to end,
/// which for a well-behaved SSE stream is never.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(10);

/// MCP's session header. A server that hands one out expects it echoed on
/// every later request; dropping it silently starts a new session per call.
const SESSION_HEADER: &str = "mcp-session-id";

fn base_headers(extra: &BTreeMap<String, String>) -> Result<HeaderMap, McpError> {
    let mut headers = HeaderMap::new();
    for (k, v) in extra {
        let name = HeaderName::from_bytes(k.as_bytes())
            .map_err(|_| McpError::Protocol(format!("invalid header name `{k}`")))?;
        let value = HeaderValue::from_str(v)
            .map_err(|_| McpError::Protocol(format!("invalid value for header `{k}`")))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn http_client() -> Result<reqwest::Client, McpError> {
    reqwest::Client::builder()
        // No global request timeout: an SSE stream is meant to stay open for
        // the session's lifetime, and a client-wide timeout would cut it. The
        // per-request deadline lives in `McpClient::call` instead, where it
        // can fail one request without killing the connection.
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| McpError::Protocol(format!("could not build an HTTP client: {e}")))
}

/// Forwards every JSON message in an SSE response body to `tx` until the
/// stream ends. Returns when the server closes it.
async fn pump_sse(resp: reqwest::Response, tx: IncomingSender) {
    let mut events = resp.bytes_stream().eventsource();
    while let Some(event) = events.next().await {
        match event {
            Ok(event) => {
                if event.data.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&event.data) {
                    // Same tolerance as the stdio reader: one malformed frame
                    // costs its own message, never the connection.
                    Err(e) => tracing::warn!("mcp: unparseable SSE frame: {e}"),
                    Ok(value) => {
                        if tx.send(value).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("mcp: SSE stream ended: {e}");
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streamable HTTP
// ---------------------------------------------------------------------------

pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session_id: Mutex<Option<String>>,
    tx: IncomingSender,
}

impl HttpTransport {
    /// Opens the transport. Nothing is sent yet — the handshake is
    /// `McpClient`'s `initialize`, and it is that POST which decides whether
    /// this endpoint speaks Streamable HTTP at all.
    pub fn connect(
        url: &str,
        extra_headers: &BTreeMap<String, String>,
    ) -> Result<(Self, Incoming), McpError> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Ok((
            Self {
                client: http_client()?,
                url: url.to_string(),
                headers: base_headers(extra_headers)?,
                session_id: Mutex::new(None),
                tx,
            },
            rx,
        ))
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, message: &Value) -> Result<(), McpError> {
        let mut request = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .json(message);
        if let Some(session) = self.session_id.lock().await.as_ref() {
            request = request.header(SESSION_HEADER, session);
        }

        let resp = request
            .send()
            .await
            .map_err(|e| McpError::Transport(format!("POST {}: {e}", self.url)))?;

        if let Some(session) = resp
            .headers()
            .get(SESSION_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().await = Some(session.to_string());
        }

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            return Err(McpError::Transport(format!(
                "POST {} returned HTTP {status}{}",
                self.url,
                if snippet.is_empty() {
                    String::new()
                } else {
                    format!(": {snippet}")
                }
            )));
        }

        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if content_type.starts_with("text/event-stream") {
            // Pumped in the background: the server may keep this stream open
            // past the reply (for progress notifications), and awaiting it
            // here would deadlock every request behind the first one.
            let tx = self.tx.clone();
            tokio::spawn(pump_sse(resp, tx));
            return Ok(());
        }

        let body = resp.bytes().await.unwrap_or_default();
        if body.is_empty() {
            // 202 for a notification, which has no reply by definition.
            return Ok(());
        }
        match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Array(items)) => {
                for item in items {
                    let _ = self.tx.send(item);
                }
            }
            Ok(value) => {
                let _ = self.tx.send(value);
            }
            Err(e) => {
                return Err(McpError::Protocol(format!(
                    "POST {} answered with a body that is not JSON-RPC: {e}",
                    self.url
                )))
            }
        }
        Ok(())
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Http
    }
}

// ---------------------------------------------------------------------------
// HTTP + SSE (the older pair)
// ---------------------------------------------------------------------------

pub struct SseTransport {
    client: reqwest::Client,
    post_url: String,
    headers: HeaderMap,
}

impl SseTransport {
    /// Opens the GET stream and waits for the server to name its POST
    /// endpoint, which is the whole handshake for this transport.
    pub async fn connect(
        url: &str,
        extra_headers: &BTreeMap<String, String>,
    ) -> Result<(Self, Incoming), McpError> {
        let client = http_client()?;
        let headers = base_headers(extra_headers)?;

        let resp = client
            .get(url)
            .headers(headers.clone())
            .header(ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(|e| McpError::Transport(format!("GET {url}: {e}")))?;
        if !resp.status().is_success() {
            return Err(McpError::Transport(format!(
                "GET {url} returned HTTP {}",
                resp.status()
            )));
        }

        let base = reqwest::Url::parse(url)
            .map_err(|e| McpError::Protocol(format!("`{url}` is not a URL: {e}")))?;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (endpoint_tx, endpoint_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut endpoint_tx = Some(endpoint_tx);
            let mut events = resp.bytes_stream().eventsource();
            while let Some(event) = events.next().await {
                let Ok(event) = event else { break };
                if event.event == "endpoint" {
                    if let Some(sender) = endpoint_tx.take() {
                        let resolved = base
                            .join(event.data.trim())
                            .map(|u| u.to_string())
                            .unwrap_or_else(|_| event.data.trim().to_string());
                        let _ = sender.send(resolved);
                    }
                    continue;
                }
                if event.data.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(&event.data) {
                    Err(e) => tracing::warn!("mcp: unparseable SSE frame: {e}"),
                    Ok(value) => {
                        if tx.send(value).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        let post_url = match tokio::time::timeout(ENDPOINT_TIMEOUT, endpoint_rx).await {
            Ok(Ok(url)) => url,
            Ok(Err(_)) => {
                return Err(McpError::Transport(
                    "the SSE stream closed before naming a POST endpoint".into(),
                ))
            }
            Err(_) => {
                return Err(McpError::Transport(format!(
                    "no `endpoint` event within {}s — this may be a Streamable HTTP server \
                     rather than an HTTP+SSE one",
                    ENDPOINT_TIMEOUT.as_secs()
                )))
            }
        };

        Ok((
            Self {
                client,
                post_url,
                headers,
            },
            rx,
        ))
    }
}

#[async_trait]
impl Transport for SseTransport {
    async fn send(&self, message: &Value) -> Result<(), McpError> {
        // Nothing is read from this response: every reply arrives on the GET
        // stream opened at connect time. A 2xx only means "accepted".
        let resp = self
            .client
            .post(&self.post_url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .json(message)
            .send()
            .await
            .map_err(|e| McpError::Transport(format!("POST {}: {e}", self.post_url)))?;
        if !resp.status().is_success() {
            return Err(McpError::Transport(format!(
                "POST {} returned HTTP {}",
                self.post_url,
                resp.status()
            )));
        }
        Ok(())
    }

    fn kind(&self) -> TransportKind {
        TransportKind::Sse
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::mpsc;

    /// Which shape of MCP-over-HTTP the fake server speaks.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Mode {
        /// Streamable HTTP answering each POST with a plain JSON body.
        JsonReply,
        /// Streamable HTTP answering each POST with a one-shot SSE stream —
        /// the other branch of the same transport.
        SseReply,
        /// The older HTTP+SSE pair: a long-lived GET plus a POST endpoint.
        LegacySse,
        /// Answers every request with 404, so the auto-detecting connect has
        /// something real to fail over from.
        NotFound,
    }

    /// The three or four JSON-RPC methods the transport tests need.
    fn handle(req: &Value) -> Option<Value> {
        let id = req.get("id")?.clone();
        let result = match req.get("method").and_then(|m| m.as_str())? {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-http", "version": "9.9.9"},
            }),
            "tools/list" => json!({"tools": [
                {"name": "ping", "description": "answers pong", "inputSchema": {"type": "object"}}
            ]}),
            "tools/call" => json!({
                "content": [{"type": "text", "text": "pong"}],
                "isError": false,
            }),
            other => {
                return Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("no {other}")},
                }))
            }
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    /// Reads one HTTP request. Enough of a parser for a loopback test and no
    /// more: request line, headers, and a `Content-Length` body.
    async fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let head_end = loop {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        let mut body = buf[head_end..].to_vec();
        while body.len() < len {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        Some((head, String::from_utf8_lossy(&body).to_string()))
    }

    async fn write_all(stream: &mut TcpStream, text: &str) {
        let _ = stream.write_all(text.as_bytes()).await;
        let _ = stream.flush().await;
    }

    /// Starts the fake server on an ephemeral loopback port and returns its
    /// URL. The task ends when the test does.
    pub(crate) async fn spawn(mode: Mode) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let addr = listener.local_addr().unwrap();
        // Where a POST's reply goes in LegacySse mode: into the open GET.
        let sse_out: Arc<tokio::sync::Mutex<Option<mpsc::UnboundedSender<String>>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let sse_out = sse_out.clone();
                tokio::spawn(async move {
                    let Some((head, body)) = read_request(&mut stream).await else {
                        return;
                    };
                    let is_get = head.starts_with("GET ");

                    if mode == Mode::NotFound {
                        write_all(
                            &mut stream,
                            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                        return;
                    }

                    if is_get {
                        if mode != Mode::LegacySse {
                            write_all(
                                &mut stream,
                                "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                            return;
                        }
                        let (tx, mut rx) = mpsc::unbounded_channel();
                        *sse_out.lock().await = Some(tx);
                        write_all(
                            &mut stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                             Cache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n\
                             event: endpoint\r\ndata: /messages\r\n\r\n",
                        )
                        .await;
                        while let Some(payload) = rx.recv().await {
                            write_all(&mut stream, &format!("data: {payload}\n\n")).await;
                        }
                        return;
                    }

                    // A real HTTP+SSE server refuses a POST anywhere but its
                    // message endpoint — which is exactly what lets the
                    // auto-detecting connect fail over quickly instead of
                    // waiting out a request timeout.
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    if mode == Mode::LegacySse && path != "/messages" {
                        write_all(
                            &mut stream,
                            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                        return;
                    }

                    let Ok(req) = serde_json::from_str::<Value>(&body) else {
                        return;
                    };
                    let Some(reply) = handle(&req) else {
                        // A notification: accepted, nothing to answer.
                        write_all(
                            &mut stream,
                            "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                        return;
                    };
                    let payload = reply.to_string();

                    match mode {
                        Mode::JsonReply => {
                            write_all(
                                &mut stream,
                                &format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Mcp-Session-Id: sess-1\r\nContent-Length: {}\r\n\
                                 Connection: close\r\n\r\n{payload}",
                                    payload.len()
                                ),
                            )
                            .await;
                        }
                        Mode::SseReply => {
                            let frame = format!("data: {payload}\n\n");
                            write_all(
                                &mut stream,
                                &format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{frame}",
                                    frame.len()
                                ),
                            )
                            .await;
                        }
                        Mode::LegacySse => {
                            write_all(
                                &mut stream,
                                "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                            if let Some(tx) = sse_out.lock().await.as_ref() {
                                let _ = tx.send(payload);
                            }
                        }
                        Mode::NotFound => unreachable!("handled above"),
                    }
                });
            }
        });

        format!("http://{addr}")
    }

    #[test]
    fn header_names_and_values_are_validated_rather_than_panicking() {
        let mut bad_name = BTreeMap::new();
        bad_name.insert("not a header".to_string(), "x".to_string());
        assert!(base_headers(&bad_name).is_err());

        let mut bad_value = BTreeMap::new();
        bad_value.insert("Authorization".to_string(), "line\nbreak".to_string());
        assert!(base_headers(&bad_value).is_err());

        let mut good = BTreeMap::new();
        good.insert("Authorization".to_string(), "Bearer t".to_string());
        assert_eq!(base_headers(&good).unwrap().len(), 1);
    }

    fn spec(url: &str, transport: Option<&str>) -> smith_config::McpServerConfig {
        smith_config::McpServerConfig {
            name: "http-fake".into(),
            url: Some(url.into()),
            transport: transport.map(str::to_string),
            ..Default::default()
        }
    }

    /// Streamable HTTP, both reply shapes. The transport has to accept a plain
    /// JSON body *and* an SSE stream on the POST response — servers pick per
    /// request, so a client that only handles one works with half the world.
    #[tokio::test]
    async fn streamable_http_works_with_a_json_reply_and_with_an_sse_reply() {
        for mode in [Mode::JsonReply, Mode::SseReply] {
            let url = spawn(mode).await;
            let client = crate::client::McpClient::connect("http-fake", &spec(&url, Some("http")))
                .await
                .expect("connects");

            assert_eq!(client.transport_kind(), TransportKind::Http);
            assert_eq!(client.server_version(), Some("9.9.9"));

            let tools = client.list_tools().await.unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "ping");

            let outcome = client.call_tool("ping", json!({})).await.unwrap();
            assert_eq!(outcome.text, "pong");
        }
    }

    /// The older HTTP+SSE pair: replies arrive on the GET stream opened at
    /// connect time, not on the POST that asked for them.
    #[tokio::test]
    async fn the_legacy_http_and_sse_pair_works_end_to_end() {
        let url = spawn(Mode::LegacySse).await;
        let client = crate::client::McpClient::connect("sse-fake", &spec(&url, Some("sse")))
            .await
            .expect("connects");

        assert_eq!(client.transport_kind(), TransportKind::Sse);
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools[0].name, "ping");
        assert_eq!(
            client.call_tool("ping", json!({})).await.unwrap().text,
            "pong"
        );
    }

    /// With no explicit `transport`, a URL is tried as Streamable HTTP and
    /// then as HTTP+SSE — so a legacy server needs no configuration at all.
    #[tokio::test]
    async fn an_unqualified_url_falls_back_from_streamable_http_to_sse() {
        let url = spawn(Mode::LegacySse).await;
        let client = crate::client::McpClient::connect("auto", &spec(&url, None))
            .await
            .expect("falls back");
        assert_eq!(client.transport_kind(), TransportKind::Sse);
    }

    /// When neither shape answers, the error names both attempts rather than
    /// reporting only the last one — "404" alone would send the user hunting
    /// for the wrong problem.
    #[tokio::test]
    async fn an_endpoint_that_is_neither_reports_both_failures() {
        let url = spawn(Mode::NotFound).await;
        let err = crate::client::McpClient::connect("auto", &spec(&url, None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("Streamable HTTP"), "{err}");
        assert!(err.contains("HTTP+SSE"), "{err}");
    }

    /// Configured headers reach the server on every request — without this an
    /// authenticated remote server is unusable.
    #[tokio::test]
    async fn configured_headers_are_sent_on_every_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let seen_tx = seen_tx.clone();
                tokio::spawn(async move {
                    let Some((head, body)) = read_request(&mut stream).await else {
                        return;
                    };
                    let _ = seen_tx.send(head.clone());
                    let Ok(req) = serde_json::from_str::<Value>(&body) else {
                        return;
                    };
                    let Some(reply) = handle(&req) else {
                        write_all(
                            &mut stream,
                            "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                        return;
                    };
                    let payload = reply.to_string();
                    write_all(
                        &mut stream,
                        &format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        ),
                    )
                    .await;
                });
            }
        });

        let mut spec = spec(&format!("http://{addr}"), Some("http"));
        spec.headers
            .insert("Authorization".into(), "Bearer secret".into());
        let client = crate::client::McpClient::connect("auth", &spec)
            .await
            .unwrap();
        client.list_tools().await.unwrap();

        let mut requests = 0;
        while let Ok(head) = seen_rx.try_recv() {
            assert!(
                head.to_ascii_lowercase()
                    .contains("authorization: bearer secret"),
                "{head}"
            );
            requests += 1;
        }
        assert!(
            requests >= 2,
            "expected initialize and tools/list, saw {requests}"
        );
    }
}
