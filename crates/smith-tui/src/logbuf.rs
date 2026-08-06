//! An in-memory ring of recent diagnostic lines, shown by `Ctrl+L`.
//!
//! # Why this lives here and knows nothing about `tracing`
//!
//! The crates below this one already emit diagnostics — `smith-mcp` warns
//! about unparseable JSON-RPC frames, `smith-provider` about falling back to
//! an HTTP client with no timeouts. Until now every one of those went nowhere:
//! no subscriber was installed anywhere in the workspace, so the calls
//! compiled, ran, and discarded their output. A user whose MCP server was
//! answering malformed frames had no way to find out.
//!
//! A TUI cannot simply print them — stdout is the alternate screen. So they
//! go here instead, and `Ctrl+L` shows them.
//!
//! This type deliberately has **no `tracing` dependency**. It is a queue with
//! a lock. `smith-cli` owns the `tracing_subscriber::Layer` that calls
//! [`LogBuffer::push`], because `smith-cli` is where the process's global
//! logging state belongs and because the dependency only points one way:
//! `smith-cli` already depends on `smith-tui`, and the reverse would be a
//! cycle.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Lines kept. Sized for "what just went wrong", not for an audit trail —
/// the file appender is what you keep. At ~120 bytes a line this is well
/// under a megabyte, which is the point: a long-running session must not
/// grow without bound because a chatty MCP server is reconnecting.
pub const LOG_CAPACITY: usize = 500;

/// Severity, kept as our own enum so this module doesn't name `tracing`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Trace => "TRACE",
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: LogLevel,
    /// Emitting module path, e.g. `smith_mcp::transport`.
    pub target: String,
    pub message: String,
}

/// A bounded, shareable ring of log lines.
///
/// Cloning shares the underlying buffer — that is how the subscriber in
/// `smith-cli` and the `App` in this crate end up looking at the same rows.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a line, evicting the oldest once the ring is full.
    ///
    /// A poisoned lock is ignored rather than propagated: a panic in some
    /// other thread must not turn every subsequent log call into a second
    /// panic, and losing a diagnostic line is strictly better than losing the
    /// session that was trying to report it.
    pub fn push(&self, line: LogLine) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if guard.len() == LOG_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(line);
    }

    /// Every line currently held, oldest first.
    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner
            .lock()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(message: &str) -> LogLine {
        LogLine {
            level: LogLevel::Warn,
            target: "smith_mcp::transport".to_string(),
            message: message.to_string(),
        }
    }

    #[test]
    fn snapshot_returns_lines_oldest_first() {
        let buf = LogBuffer::new();
        buf.push(line("first"));
        buf.push(line("second"));
        let seen: Vec<String> = buf.snapshot().into_iter().map(|l| l.message).collect();
        assert_eq!(seen, ["first", "second"]);
    }

    #[test]
    fn the_ring_evicts_the_oldest_and_never_grows_past_capacity() {
        let buf = LogBuffer::new();
        for i in 0..LOG_CAPACITY + 10 {
            buf.push(line(&format!("line {i}")));
        }
        assert_eq!(buf.len(), LOG_CAPACITY);
        let snapshot = buf.snapshot();
        // The first ten are gone, the newest is last.
        assert_eq!(snapshot.first().unwrap().message, "line 10");
        assert_eq!(
            snapshot.last().unwrap().message,
            format!("line {}", LOG_CAPACITY + 9)
        );
    }

    #[test]
    fn clones_share_one_buffer() {
        let buf = LogBuffer::new();
        let other = buf.clone();
        other.push(line("written through the clone"));
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn a_poisoned_lock_drops_the_line_instead_of_panicking() {
        let buf = LogBuffer::new();
        let inner = buf.clone();
        // Poison the mutex from another thread.
        let _ = std::thread::spawn(move || {
            let _guard = inner.inner.lock().unwrap();
            panic!("poison it");
        })
        .join();

        buf.push(line("after poisoning"));
        assert!(buf.is_empty(), "the line is dropped, not stored");
        assert!(
            buf.snapshot().is_empty(),
            "and reading still does not panic"
        );
    }
}
