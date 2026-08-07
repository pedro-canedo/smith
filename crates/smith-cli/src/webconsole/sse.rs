//! The event stream, framed for `EventSource`.
//!
//! One frame per event: `id: <seq>` then `data: <one stream-json line>`.
//! The `data:` payload is `AgentEvent`'s own serialization — the same bytes
//! `--output-format stream-json` writes — so the console's wire format and
//! the scripting wire format are one contract, not two.
//!
//! Two synthetic frame types exist only here, never in stream-json:
//! `hello` (on connect, carrying the current seq so the client knows where
//! the stream starts) and `gap` (the subscriber lagged past the broadcast
//! buffer and must refetch `/api/state`). A comment frame `: ping` goes out
//! on a timer so nothing between us and the browser reaps an idle socket.

use std::time::Duration;

use tokio::io::{AsyncWriteExt, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

use super::state::StampedEvent;

/// How often the comment frame goes out on a quiet stream.
pub const PING_INTERVAL: Duration = Duration::from_secs(15);

/// The response head that turns a socket into an event stream. No
/// `Content-Length`: the stream ends when the connection does.
pub const SSE_HEAD: &str = "HTTP/1.1 200 OK\r\n\
    Content-Type: text/event-stream\r\n\
    Cache-Control: no-store\r\n\
    Connection: close\r\n\
    X-Content-Type-Options: nosniff\r\n\
    Referrer-Policy: no-referrer\r\n\
    \r\n";

/// One event as a wire frame.
pub fn frame(seq: u64, json: &str) -> String {
    format!("id: {seq}\ndata: {json}\n\n")
}

pub fn hello_frame(seq: u64) -> String {
    frame(
        seq,
        &format!(r#"{{"type":"hello","data":{{"seq":{seq}}}}}"#),
    )
}

/// `missed` is broadcast's own count of dropped events — sent to the client
/// verbatim so the gap is a measured fact, not a vibe.
pub fn gap_frame(missed: u64) -> String {
    // No seq id: the gap is not a position in the stream, it is a hole in it.
    format!("data: {{\"type\":\"gap\",\"data\":{{\"missed\":{missed}}}}}\n\n")
}

/// Streams events to one subscriber until the client hangs up or the pump
/// stops. The caller has already written [`SSE_HEAD`] and the hello frame.
pub async fn stream(
    mut writer: WriteHalf<TcpStream>,
    mut events: broadcast::Receiver<StampedEvent>,
) {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick completes immediately; consume it so a fresh stream
    // does not open with a pointless ping.
    ping.tick().await;

    loop {
        let payload = tokio::select! {
            received = events.recv() => match received {
                Ok((seq, event)) => match serde_json::to_string(&event) {
                    Ok(json) => frame(seq, &json),
                    // An unserializable event is a bug pinned by tests in
                    // smith-core; skipping one frame beats killing the stream.
                    Err(_) => continue,
                },
                Err(broadcast::error::RecvError::Lagged(missed)) => gap_frame(missed),
                // The pump stopped: the session is over, and so is the stream.
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = ping.tick() => ": ping\n\n".to_string(),
        };
        if writer.write_all(payload.as_bytes()).await.is_err() {
            break; // client hung up — normal, not reportable
        }
        if writer.flush().await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_carries_the_seq_as_its_id_and_the_json_as_its_data() {
        let f = frame(42, r#"{"type":"assistant_text_delta","data":"Ol"}"#);
        assert_eq!(
            f,
            "id: 42\ndata: {\"type\":\"assistant_text_delta\",\"data\":\"Ol\"}\n\n"
        );
    }

    #[test]
    fn the_synthetic_frames_are_valid_json_and_the_gap_has_no_id() {
        let hello = hello_frame(7);
        assert!(hello.starts_with("id: 7\n"));
        let json: serde_json::Value = serde_json::from_str(
            hello
                .lines()
                .nth(1)
                .unwrap()
                .strip_prefix("data: ")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(json["type"], "hello");
        assert_eq!(json["data"]["seq"], 7);

        let gap = gap_frame(12);
        assert!(!gap.contains("id:"), "a hole is not a position: {gap}");
        let json: serde_json::Value =
            serde_json::from_str(gap.lines().next().unwrap().strip_prefix("data: ").unwrap())
                .unwrap();
        assert_eq!(json["type"], "gap");
        assert_eq!(json["data"]["missed"], 12);
    }
}
