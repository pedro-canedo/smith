//! ratatui/crossterm terminal UI: chat pane, input box, permission modal.

pub mod app;
pub mod banner;
pub mod complete;
pub mod components;
pub mod highlight;
pub mod keymap;
pub mod logbuf;
pub mod markdown;
pub mod slash;
pub mod terminal;
pub mod theme;
pub mod transcript;
pub mod ui;

/// The `App` this crate's tests build from. Not a feature, unlike
/// `smith_core::testkit` — nothing outside this crate consumes it.
#[cfg(test)]
mod testkit;

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use smith_core::{Action, AgentEvent, AskAnswer, AskSource, SubmittedAnswer};
use tokio::sync::mpsc;

pub use app::{App, ChatLine, ChatRole, IdleHint, TuiConfig};
pub use complete::CompletionKind;
pub use keymap::{KeyAction, KeyMap, KeyMapError};
pub use logbuf::{LogBuffer, LogLevel, LogLine};
pub use theme::{Theme, ThemeError, ThemeName};

const SPINNER_INTERVAL: Duration = Duration::from_millis(app::SPINNER_INTERVAL_MS as u64);

/// Drives the terminal UI until the user quits. `action_tx` carries user
/// intent out to the orchestrator; `agent_events` carries orchestrator state
/// back in; `ask_tx` carries answers to blocking permission/question prompts
/// to the ask broker, which owns the oneshots the agent is waiting on.
///
/// The TUI does not receive `PermissionAsk`/`QuestionAsk` itself any more:
/// those carry a `oneshot::Sender` consumable exactly once, and with the web
/// console a second frontend can answer first. The broker is the one owner;
/// every frontend — this one included — learns of pending asks from the
/// `AgentEvent`s and answers over the same channel.
pub async fn run(
    config: TuiConfig,
    action_tx: mpsc::UnboundedSender<Action>,
    mut agent_events: mpsc::UnboundedReceiver<AgentEvent>,
    ask_tx: mpsc::UnboundedSender<SubmittedAnswer>,
) -> color_eyre::Result<()> {
    let mut term = terminal::init()?;
    let _restore = terminal::RestoreGuard::armed();
    let mut app = App::new(config);
    let mut crossterm_events = EventStream::new();
    let mut spinner = tokio::time::interval(SPINNER_INTERVAL);

    term.draw(|f| ui::draw(f, &mut app))?;

    loop {
        tokio::select! {
            _ = spinner.tick() => {
                // An expiring `Ctrl+C` hint has to be repainted away even
                // when nothing else is moving — otherwise it sits on an idle
                // screen claiming a second press will still quit.
                let quit_hint_expired = app.expire_pending_quit();
                if !app.is_animating() && !quit_hint_expired {
                    continue;
                }
                app.tick();
            }
            maybe_event = crossterm_events.next() => {
                let Some(Ok(event)) = maybe_event else { continue };
                if let Event::Paste(text) = &event {
                    app.on_paste(text);
                }
                if let Event::Mouse(mouse) = event {
                    if let Some(action) = app.on_mouse(mouse) {
                        let _ = action_tx.send(action);
                    }
                }
                if let Event::Key(key) = event {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if let Some(action) = app.on_key(key.code, key.modifiers) {
                        match action {
                            Action::PermissionResponse {
                                tool_call_id,
                                decision,
                            } => {
                                let _ = ask_tx.send(SubmittedAnswer {
                                    source: AskSource::Tui,
                                    answer: AskAnswer::Permission {
                                        tool_call_id,
                                        decision,
                                    },
                                });
                            }
                            Action::QuestionResponse { id, answer } => {
                                let _ = ask_tx.send(SubmittedAnswer {
                                    source: AskSource::Tui,
                                    answer: AskAnswer::Question { id, answer },
                                });
                            }
                            other => {
                                let _ = action_tx.send(other);
                            }
                        }
                    }
                }
            }
            Some(event) = agent_events.recv() => {
                app.on_agent_event(event);
            }
        }

        // Polled here rather than pushed from `on_agent_event`: four
        // different events end a turn, and one of them forgetting to check
        // would strand the queue with no sign of why.
        if let Some(action) = app.take_queued_prompt() {
            let _ = action_tx.send(action);
        }

        term.draw(|f| ui::draw(f, &mut app))?;

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Debug-only crash path used by the PTY regression test. It has to live on
/// the TUI side so the process has really entered raw mode and the alternate
/// screen before the panic happens.
#[cfg(debug_assertions)]
pub fn panic_after_terminal_init_for_test() -> color_eyre::Result<()> {
    let _term = terminal::init()?;
    let _restore = terminal::RestoreGuard::armed();
    panic!("smith debug panic requested by --panic-now");
}
