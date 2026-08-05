//! ratatui/crossterm terminal UI: chat pane, input box, permission modal.

pub mod app;
pub mod banner;
pub mod components;
pub mod markdown;
pub mod pricing;
pub mod slash;
pub mod terminal;
pub mod theme;
pub mod transcript;
pub mod ui;

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use smith_core::{Action, AgentEvent, PermissionAsk, QuestionAsk};
use tokio::sync::{mpsc, oneshot};

pub use app::{App, ChatLine, ChatRole, IdleHint, TuiConfig};
pub use theme::Theme;

const SPINNER_INTERVAL: Duration = Duration::from_millis(120);

/// Drives the terminal UI until the user quits. `action_tx` carries user
/// intent out to the orchestrator; `agent_events`, `permission_asks`, and
/// `question_asks` carry orchestrator state back in.
pub async fn run(
    config: TuiConfig,
    action_tx: mpsc::UnboundedSender<Action>,
    mut agent_events: mpsc::UnboundedReceiver<AgentEvent>,
    mut permission_asks: mpsc::UnboundedReceiver<PermissionAsk>,
    mut question_asks: mpsc::UnboundedReceiver<QuestionAsk>,
) -> color_eyre::Result<()> {
    let mut term = terminal::init()?;
    let mut app = App::new(config);
    let mut crossterm_events = EventStream::new();
    let mut pending_permission: Option<oneshot::Sender<smith_core::PermissionDecision>> = None;
    // `Result` because a frontend may be unable to ask at all; the TUI
    // always can, so it only ever sends `Ok`.
    let mut pending_question: Option<oneshot::Sender<Result<String, String>>> = None;
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
                if let Event::Key(key) = event {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if let Some(action) = app.on_key(key.code, key.modifiers) {
                        match action {
                            Action::PermissionResponse(decision) => {
                                if let Some(tx) = pending_permission.take() {
                                    let _ = tx.send(decision);
                                }
                            }
                            Action::QuestionResponse(answer) => {
                                if let Some(tx) = pending_question.take() {
                                    let _ = tx.send(Ok(answer));
                                }
                            }
                            Action::Quit => {
                                let _ = action_tx.send(action);
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
            Some(ask) = permission_asks.recv() => {
                // ask.request is already surfaced via AgentEvent::PermissionPromptNeeded
                pending_permission = Some(ask.respond_to);
            }
            Some(ask) = question_asks.recv() => {
                // ask.question is already surfaced via AgentEvent::UserQuestionNeeded
                pending_question = Some(ask.respond_to);
            }
        }

        term.draw(|f| ui::draw(f, &mut app))?;

        if app.should_quit {
            break;
        }
    }

    terminal::restore()?;
    Ok(())
}
