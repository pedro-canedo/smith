//! Wires the workspace's `tracing` calls to somewhere a user can read them.
//!
//! Until this existed, no subscriber was installed anywhere in the workspace.
//! `smith-mcp` warning that a server sent an unparseable JSON-RPC frame, and
//! `smith-provider` warning that it fell back to an HTTP client with no
//! timeouts, both compiled, ran, and vanished. The failure mode was the worst
//! kind: a diagnostic that exists in the source, so nobody adds a second one,
//! and reaches nobody, so nobody acts on the first.
//!
//! Two sinks, because they answer different questions:
//!
//! - The in-memory ring behind `Ctrl+L` answers "what just went wrong", during
//!   the session, without leaving the alternate screen.
//! - The rolling file under the state directory answers "what went wrong
//!   before it died", after the fact. A TUI that has crashed cannot show you
//!   its own log.
//!
//! Level comes from `SMITH_LOG` (or `RUST_LOG`), defaulting to `warn` for our
//! own crates and `off` for everything else. The default matters: `reqwest`
//! and `hyper` at `debug` would fill a 500-line ring with connection-pool
//! chatter and evict the one line that explains the failure.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;

use smith_tui::{LogBuffer, LogLevel, LogLine};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Applied when neither `SMITH_LOG` nor `RUST_LOG` is set.
const DEFAULT_FILTER: &str = "warn,smith_core=warn,smith_provider=warn,smith_mcp=warn,\
                             smith_tools=warn,smith_store=warn,smith_config=warn";

/// A `tracing` layer that appends into a [`LogBuffer`].
struct BufferLayer {
    buffer: LogBuffer,
}

/// Pulls the `message` field out of an event, and folds any other fields into
/// `key=value` text. Without the second half, a `tracing::warn!(error = %e,
/// "falling back")` would log "falling back" and silently drop the error —
/// which is the entire content of that line.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
            // `record_debug` on a string field quotes it; the message is the
            // one field a reader sees raw.
            if self.message.starts_with('"') && self.message.ends_with('"') {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        } else {
            self.fields.push(format!("{}={value:?}", field.name()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push(format!("{}={value}", field.name()));
        }
    }
}

impl MessageVisitor {
    fn into_text(self) -> String {
        if self.fields.is_empty() {
            return self.message;
        }
        let fields = self.fields.join(" ");
        if self.message.is_empty() {
            fields
        } else {
            format!("{} ({fields})", self.message)
        }
    }
}

fn level_of(level: &Level) -> LogLevel {
    match *level {
        Level::TRACE => LogLevel::Trace,
        Level::DEBUG => LogLevel::Debug,
        Level::INFO => LogLevel::Info,
        Level::WARN => LogLevel::Warn,
        Level::ERROR => LogLevel::Error,
    }
}

impl<S> Layer<S> for BufferLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let metadata = event.metadata();
        self.buffer.push(LogLine {
            level: level_of(metadata.level()),
            target: metadata.target().to_string(),
            message: visitor.into_text(),
        });
    }
}

/// Installs the global subscriber and returns the ring the TUI will read.
///
/// Never fails the process: a log file that cannot be opened costs the file
/// sink and nothing else, and a subscriber that is already installed (a second
/// call, or a test harness) leaves the returned buffer working as a plain
/// queue. Refusing to start `smith` because logging could not start would
/// trade a minor loss of diagnostics for a total loss of the tool.
pub fn install(log_dir: Option<PathBuf>) -> LogBuffer {
    let buffer = LogBuffer::new();

    let filter = EnvFilter::try_from_env("SMITH_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    let file = log_dir.and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        File::create(dir.join("smith.log")).ok()
    });

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(BufferLayer {
            buffer: buffer.clone(),
        });

    match file {
        Some(file) => {
            let _ = registry
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(Mutex::new(file)),
                )
                .try_init();
        }
        None => {
            let _ = registry.try_init();
        }
    }

    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(event_fields: &[(&str, &str)], message: &str) -> String {
        // `Visit` is exercised directly: constructing a real `Event` needs a
        // `Metadata` with a `'static` callsite, which a test cannot fabricate.
        MessageVisitor {
            message: message.to_string(),
            fields: event_fields
                .iter()
                .map(|(k, val)| format!("{k}={val}"))
                .collect(),
        }
        .into_text()
    }

    #[test]
    fn a_message_with_no_other_fields_is_passed_through() {
        assert_eq!(visit(&[], "mcp: SSE stream ended"), "mcp: SSE stream ended");
    }

    #[test]
    fn extra_fields_are_kept_because_they_are_usually_the_actual_content() {
        assert_eq!(
            visit(&[("error", "connection refused")], "falling back"),
            "falling back (error=connection refused)"
        );
    }

    #[test]
    fn a_field_only_event_still_says_something() {
        assert_eq!(visit(&[("error", "timeout")], ""), "error=timeout");
    }

    #[test]
    fn levels_map_onto_the_tui_enum_without_collapsing_any() {
        let mapped = [
            level_of(&Level::TRACE),
            level_of(&Level::DEBUG),
            level_of(&Level::INFO),
            level_of(&Level::WARN),
            level_of(&Level::ERROR),
        ];
        assert_eq!(
            mapped,
            [
                LogLevel::Trace,
                LogLevel::Debug,
                LogLevel::Info,
                LogLevel::Warn,
                LogLevel::Error
            ]
        );
    }

    #[test]
    fn install_returns_a_working_buffer_even_when_it_cannot_write_a_file() {
        // A path under a file rather than a directory: `create_dir_all` fails,
        // and the point is that `install` still hands back a usable ring.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let bogus = tmp.path().join("not-a-dir");
        let buffer = install(Some(bogus));
        buffer.push(LogLine {
            level: LogLevel::Warn,
            target: "test".to_string(),
            message: "still works".to_string(),
        });
        assert_eq!(buffer.len(), 1);
    }
}
