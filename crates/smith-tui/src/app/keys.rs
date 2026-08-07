//! `on_key` — an ordered cascade of guards. The order is semantics.

use std::time::Instant;

use smith_core::{Action, AgentPhase, PermissionDecision};

use crate::complete::{self, CompletionKind};
use crate::keymap::KeyAction;
use crossterm::event::{KeyCode, KeyModifiers};

use super::chatline::{ChatLine, ChatRole};
use super::modal::Modal;
use super::App;

/// What one guard in `on_key`'s cascade decided.
///
/// `None` means "not mine, keep going" — the same thing falling past an
/// `if` block used to mean. `Some(x)` claims the key and `x` is what
/// `on_key` returns.
type Handled = Option<Option<Action>>;

impl App {
    /// Handles a raw key event, returning an Action to forward to the
    /// orchestrator if this keystroke produced one.
    ///
    /// **The order of the guards below is semantics, not style.** Each returns
    /// `Some(result)` to claim the key or `None` to let it fall through, and
    /// moving any of them changes what a keystroke does. See the doc on each.
    pub fn on_key(
        &mut self,
        code: crossterm::event::KeyCode,
        modifiers: crossterm::event::KeyModifiers,
    ) -> Option<Action> {
        if let Some(handled) = self.key_quit(code, modifiers) {
            return handled;
        }
        // Anything else means the user moved on — quitting has to be two
        // deliberate presses in a row, not any two presses. Outside `key_quit`
        // because it has to run for every key that is not Ctrl+C.
        self.quit_armed_at = None;

        if let Some(handled) = self.key_model_modal(code) {
            return handled;
        }
        if let Some(handled) = self.key_question_modal(code) {
            return handled;
        }
        if let Some(handled) = self.key_plan_modal(code) {
            return handled;
        }
        if let Some(handled) = self.key_permission_modal(code) {
            return handled;
        }

        let bound = self.keys.action_for(code, modifiers);

        if let Some(handled) = self.key_toggle_logs(bound) {
            return handled;
        }
        if let Some(handled) = self.key_overlay(code) {
            return handled;
        }
        if let Some(handled) = self.key_chrome(bound) {
            return handled;
        }
        if let Some(handled) = self.key_card_focus(code) {
            return handled;
        }

        self.key_prompt(code, modifiers, bound)
    }

    /// Ctrl+C, which pre-empts every modal below.
    fn key_quit(&mut self, code: KeyCode, modifiers: KeyModifiers) -> Handled {
        // Before the modal branches, which each consume every key they see:
        // with a modal up, `Ctrl+C` used to either type a literal "c" into
        // the question box or do nothing at all, leaving no way out.
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            if self.quit_pending() {
                self.quit_armed_at = None;
                self.should_quit = true;
                return Some(Some(Action::Quit));
            }
            self.quit_armed_at = Some(Instant::now());
            return Some(None);
        }
        None
    }

    /// The model picker owns the keyboard, plain characters included.
    fn key_model_modal(&mut self, code: KeyCode) -> Handled {
        // Before every other modal: the picker owns the keyboard while it is
        // open, including plain characters, which are its filter rather than
        // prompt text.
        if self.modal.is_model() {
            match code {
                KeyCode::Esc => {
                    self.modal = Modal::None;
                    return Some(None);
                }
                KeyCode::Enter => {
                    let picked = self.modal.model().and_then(|m| m.selected_id());
                    self.modal = Modal::None;
                    return Some(picked.map(|model| Action::SwitchModel {
                        provider: None,
                        model,
                        // A pick is for this session. Persisting silently
                        // would make a keystroke outlive the conversation it
                        // was meant for; `/model <name> --save` is the way to
                        // say otherwise, and it still works.
                        save: false,
                    }));
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Backspace | KeyCode::Char(_) => {
                    if let Some(m) = self.modal.model_mut() {
                        match code {
                            KeyCode::Up => m.selected = m.selected.saturating_sub(1),
                            KeyCode::Down => m.selected = m.selected.saturating_add(1),
                            KeyCode::Backspace => {
                                m.filter.pop();
                                // A narrower list can leave the cursor past
                                // the end; the renderer clamps, but the state
                                // must not be nonsense in the meantime.
                                m.selected = 0;
                            }
                            KeyCode::Char(c) => {
                                m.filter.push(c);
                                m.selected = 0;
                            }
                            _ => {}
                        }
                    }
                    return Some(None);
                }
                _ => return Some(None),
            }
        }
        None
    }

    /// The `ask_user` modal.
    fn key_question_modal(&mut self, code: KeyCode) -> Handled {
        if self.modal.is_question() {
            return Some(match code {
                KeyCode::Char('1') => self.submit_question_choice(0),
                KeyCode::Char('2') => self.submit_question_choice(1),
                KeyCode::Char('3') => self.submit_question_choice(2),
                KeyCode::Char('4') => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = 3;
                    }
                    None
                }
                KeyCode::Up => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = m.selected.saturating_sub(1);
                    }
                    None
                }
                KeyCode::Down => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = (m.selected + 1).min(3);
                    }
                    None
                }
                KeyCode::Enter => {
                    let selected = self.modal.question().map(|m| m.selected).unwrap_or(0);
                    if selected == 3 {
                        let custom = self
                            .modal
                            .question()
                            .map(|m| m.custom.trim().to_string())
                            .unwrap_or_default();
                        if custom.is_empty() {
                            None
                        } else {
                            self.submit_question_answer(custom)
                        }
                    } else {
                        self.submit_question_choice(selected)
                    }
                }
                KeyCode::Esc => self.submit_question_answer(
                    "User dismissed the question without answering.".to_string(),
                ),
                KeyCode::Backspace => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = 3;
                        m.custom.pop();
                    }
                    None
                }
                KeyCode::Char(c) if !c.is_control() => {
                    if let Some(m) = self.modal.question_mut() {
                        m.selected = 3;
                        m.custom.push(c);
                    }
                    None
                }
                _ => None,
            });
        }
        None
    }

    /// The plan approval modal.
    fn key_plan_modal(&mut self, code: KeyCode) -> Handled {
        if self.modal.is_plan() {
            return Some(match code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.modal = Modal::None;
                    self.plan_gated = false;
                    self.waiting_on_assistant = true;
                    self.phase = AgentPhase::Building;
                    self.in_flight_text = None;

                    self.metrics.begin_turn();
                    self.request_count += 1;
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "plan approved — building…"));
                    Some(Action::ApprovePlan)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.modal = Modal::None;
                    self.plan_gated = false;
                    self.lines
                        .push(ChatLine::new(ChatRole::System, "plan rejected"));
                    Some(Action::RejectPlan)
                }
                KeyCode::Up => {
                    if let Some(m) = self.modal.plan_mut() {
                        m.scroll = m.scroll.saturating_sub(1);
                    }
                    None
                }
                KeyCode::Down => {
                    if let Some(m) = self.modal.plan_mut() {
                        m.scroll = m.scroll.saturating_add(1);
                    }
                    None
                }
                KeyCode::PageUp => {
                    if let Some(m) = self.modal.plan_mut() {
                        m.scroll = m.scroll.saturating_sub(10);
                    }
                    None
                }
                KeyCode::PageDown => {
                    if let Some(m) = self.modal.plan_mut() {
                        m.scroll = m.scroll.saturating_add(10);
                    }
                    None
                }
                _ => None,
            });
        }
        None
    }

    /// The permission modal.
    fn key_permission_modal(&mut self, code: KeyCode) -> Handled {
        if self.modal.permission().is_some() {
            // Captured before the modal closes: the action must carry the
            // ask's id, and `Modal::None` is about to erase the only copy.
            let answer = |app: &mut Self, decision: PermissionDecision| {
                let tool_call_id = app
                    .modal
                    .permission()
                    .map(|m| m.request.tool_call_id.clone())
                    .unwrap_or_default();
                app.modal = Modal::None;
                Some(Action::PermissionResponse {
                    tool_call_id,
                    decision,
                })
            };
            return Some(match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    answer(self, PermissionDecision::AllowOnce)
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    answer(self, PermissionDecision::AllowSession)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    answer(self, PermissionDecision::Deny)
                }
                KeyCode::Up => {
                    if let Some(m) = self.modal.permission_mut() {
                        m.scroll = m.scroll.saturating_sub(1);
                    }
                    None
                }
                KeyCode::Down => {
                    if let Some(m) = self.modal.permission_mut() {
                        m.scroll = m.scroll.saturating_add(1);
                    }
                    None
                }
                KeyCode::PageUp => {
                    if let Some(m) = self.modal.permission_mut() {
                        m.scroll = m.scroll.saturating_sub(10);
                    }
                    None
                }
                KeyCode::PageDown => {
                    if let Some(m) = self.modal.permission_mut() {
                        m.scroll = m.scroll.saturating_add(10);
                    }
                    None
                }
                _ => None,
            });
        }
        None
    }

    /// Ctrl+L, which has to beat the overlay guard so it can toggle the panel off.
    fn key_toggle_logs(&mut self, bound: Option<KeyAction>) -> Handled {
        if bound == Some(KeyAction::ToggleLogs) {
            self.toggle_log_panel();
            return Some(None);
        }
        None
    }

    /// The overlay pager. Its `_` arm dismisses and *falls through* on purpose.
    fn key_overlay(&mut self, code: KeyCode) -> Handled {
        // The overlay is a pager, not a prompt: it claims the navigation keys
        // and `Esc`, and *anything else* dismisses it and is then handled
        // normally. A panel that swallowed the first keystroke of the next
        // message would be a panel you have to remember to close.
        if self.overlay.is_some() {
            match code {
                KeyCode::Esc => {
                    self.overlay = None;
                    return Some(None);
                }
                KeyCode::Up => {
                    self.scroll_overlay(-1);
                    return Some(None);
                }
                KeyCode::Down => {
                    self.scroll_overlay(1);
                    return Some(None);
                }
                KeyCode::PageUp => {
                    self.scroll_overlay(-10);
                    return Some(None);
                }
                KeyCode::PageDown => {
                    self.scroll_overlay(10);
                    return Some(None);
                }
                KeyCode::Home => {
                    if let Some(o) = self.overlay.as_mut() {
                        o.scroll = 0;
                    }
                    return Some(None);
                }
                _ => self.overlay = None,
            }
        }
        None
    }

    /// Card focus, sidebar and tab cycling — the three bound chrome keys.
    fn key_chrome(&mut self, bound: Option<KeyAction>) -> Handled {
        if bound == Some(KeyAction::ToggleCardFocus) {
            self.toggle_card_focus();
            return Some(None);
        }

        if bound == Some(KeyAction::ToggleSidebar) {
            self.sidebar_visible = !self.sidebar_visible;
            return Some(None);
        }

        // Bound to `Shift+Tab` by default, which arrives as its own key code
        // and so never competes with the `Tab` that accepts a slash
        // completion. Cycling a hidden sidebar would be a keystroke with no
        // visible effect, so it reveals it first.
        if bound == Some(KeyAction::CycleSidebarTab) {
            if self.sidebar_visible {
                self.sidebar_tab = self.sidebar_tab.next();
            } else {
                self.sidebar_visible = true;
            }
            return Some(None);
        }

        None
    }

    /// A focused card claims four keys and leaves the rest for the prompt.
    fn key_card_focus(&mut self, code: KeyCode) -> Handled {
        // Card focus claims exactly four keys and leaves typing alone, so the
        // prompt stays usable — but `Enter` belongs to the card while one is
        // selected, which is why `Ctrl+O`/`Esc` both have to release it.
        if self.selected_card().is_some() {
            match code {
                KeyCode::Up => {
                    self.move_card_focus(false);
                    return Some(None);
                }
                KeyCode::Down => {
                    self.move_card_focus(true);
                    return Some(None);
                }
                KeyCode::Enter => {
                    self.toggle_selected_card();
                    return Some(None);
                }
                // A running turn keeps `Esc` for cancelling — that is the more
                // urgent meaning, and `Ctrl+O` still releases focus.
                KeyCode::Esc if !self.waiting_on_assistant => {
                    self.select_card(None);
                    return Some(None);
                }
                _ => {}
            }
        }
        None
    }

    /// Everything the guards above did not claim: completion,
    /// history, scrolling, and submitting the prompt.
    fn key_prompt(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
        bound: Option<KeyAction>,
    ) -> Option<Action> {
        let hints = self.suggestions();
        let slash_nav = !hints.is_empty();
        let completing_file = self.completion_kind == CompletionKind::File;

        // Ahead of the cascade, because these are the unconditional half of
        // the pair: they must reach the transcript even mid-sentence.
        if modifiers.contains(KeyModifiers::CONTROL) {
            match code {
                KeyCode::Home => {
                    self.scroll_to_top();
                    return None;
                }
                KeyCode::End => {
                    self.scroll_to_bottom();
                    return None;
                }
                _ => {}
            }
        }

        match code {
            KeyCode::Esc if self.waiting_on_assistant => Some(Action::CancelGeneration),
            KeyCode::Tab if slash_nav && completing_file => {
                if let Some(pick) = hints.get(self.slash_selected) {
                    let replaced = complete::accept_file(&self.input.text(), &pick.name);
                    self.input.set(&replaced);
                    self.slash_selected = 0;
                }
                None
            }
            KeyCode::Tab if slash_nav => {
                if let Some(completed) = self
                    .commands
                    .complete(&self.input.text(), Some(self.slash_selected))
                {
                    self.input.set(&completed);
                    self.slash_selected = 0;
                }
                None
            }
            KeyCode::Up if slash_nav => {
                self.slash_selected = self.slash_selected.saturating_sub(1);
                None
            }
            KeyCode::Down if slash_nav => {
                let max = hints.len().saturating_sub(1);
                self.slash_selected = (self.slash_selected + 1).min(max);
                None
            }
            // A newline in the prompt, not a submission. `Alt+Enter` and
            // `Ctrl+J` work everywhere; `Shift+Enter` only reaches us on
            // terminals that support the kitty keyboard protocol, since
            // otherwise it arrives indistinguishable from a bare Enter.
            KeyCode::Enter
                if modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                    && !self.waiting_on_assistant =>
            {
                self.input.insert_newline();
                None
            }
            _ if bound == Some(KeyAction::InsertNewline) && !self.waiting_on_assistant => {
                self.input.insert_newline();
                None
            }
            // Arrows walk the caret through a multi-line prompt first; they
            // only reach the history once the caret is already at the top or
            // bottom of the input — which is always the case for the
            // single-row prompt this used to be.
            KeyCode::Up if self.input.move_up() => None,
            KeyCode::Down if self.input.move_down() => None,
            // Past the edge of the input, the arrows walk the prompt history,
            // the way every shell and REPL binds them. Scrolling the
            // transcript keeps PageUp/PageDown and the mouse wheel: a
            // conversation is read by the page, and a previous prompt is
            // recalled by the line.
            KeyCode::Up if self.history_back() => None,
            KeyCode::Down if self.history_forward() => None,
            // Nothing to recall — fall back to the old meaning rather than
            // making the key inert.
            KeyCode::Up => {
                self.follow_bottom = false;
                self.scroll = self.scroll.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                self.scroll = self.scroll.saturating_add(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll_page_up();
                None
            }
            KeyCode::PageDown => {
                self.scroll_page_down();
                None
            }
            // Top and bottom of the transcript. `Home`/`End` belong to the
            // caret whenever there is a prompt to move within — taking them
            // outright would cost line editing to buy scrolling — so they
            // cascade exactly as the arrows above do, and `Ctrl` takes them
            // unconditionally for anyone typing and reading at once.
            KeyCode::Home if self.input.is_empty() => {
                self.scroll_to_top();
                None
            }
            KeyCode::End if self.input.is_empty() => {
                self.scroll_to_bottom();
                None
            }
            KeyCode::Enter => {
                if self.input.text().trim().is_empty() {
                    return None;
                }
                // Mid-turn, a plain message is queued rather than refused.
                // A slash command still runs now: they are how you *steer* a
                // running turn (`/queue clear`, `/plan approve`), and queueing
                // one would be the opposite of what it is for. The ones that
                // genuinely cannot run mid-turn already say so themselves.
                if self.waiting_on_assistant && !self.input.text().starts_with('/') {
                    let text = self.input.take();
                    self.slash_selected = 0;
                    self.remember_prompt(&text);
                    // Held here *and* sent. The queue is the record of what
                    // has not landed yet: the agent folds an interjection in
                    // at its next round boundary, and a turn already on its
                    // last round has no next boundary — so anything still
                    // waiting when the turn ends is sent as its own turn
                    // instead. Without the fallback, speaking to a turn that
                    // was about to finish would drop the message silently.
                    self.queued.push_back(text.clone());
                    return Some(Action::Interject(text));
                }
                // A highlighted path is accepted rather than submitted — the
                // list is on screen precisely because the caret is mid-token,
                // so Enter means "that one", the way Tab does.
                if slash_nav && completing_file {
                    if let Some(pick) = hints.get(self.slash_selected) {
                        let replaced = complete::accept_file(&self.input.text(), &pick.name);
                        self.input.set(&replaced);
                        self.slash_selected = 0;
                        return None;
                    }
                }
                // If a slash suggestion is highlighted and input is still only
                // a partial command, Tab-complete first instead of submitting.
                if slash_nav && !self.input.text().contains(char::is_whitespace) {
                    if let Some(completed) = self
                        .commands
                        .complete(&self.input.text(), Some(self.slash_selected))
                    {
                        self.input.set(&completed);
                        self.slash_selected = 0;
                        return None;
                    }
                }
                let text = self.input.take();
                self.slash_selected = 0;
                // Slash commands go in too: `/model gpt-4.1` is exactly the
                // kind of thing you want back with one keypress.
                self.remember_prompt(&text);

                if let Some(command) = text.strip_prefix('/') {
                    return self.run_slash_command(command.trim());
                }

                self.lines.push(ChatLine::new(ChatRole::User, text.clone()));
                self.waiting_on_assistant = true;
                self.phase = AgentPhase::Thinking;
                self.in_flight_text = None;
                self.metrics.begin_turn();
                self.request_count += 1;
                Some(Action::SubmitMessage(text))
            }
            // Everything else the transcript didn't claim is text editing:
            // typing, Backspace/Delete, caret movement, word jumps,
            // Ctrl+A/E/W/U. The editor reports whether it consumed the key.
            other => {
                if self.input.handle(other, modifiers) {
                    self.slash_selected = 0;
                }
                None
            }
        }
    }

    fn submit_question_choice(&mut self, index: usize) -> Option<Action> {
        let answer = self
            .modal
            .question()
            .and_then(|m| m.question.options.get(index).cloned())?;
        self.submit_question_answer(answer)
    }

    /// Records the exchange in the transcript, closes the modal, and builds
    /// the action — with the question's id captured first, because the action
    /// must carry it and `Modal::None` erases the only copy.
    fn submit_question_answer(&mut self, answer: String) -> Option<Action> {
        let id = self.record_question_answer(&answer)?;
        self.modal = Modal::None;
        Some(Action::QuestionResponse { id, answer })
    }

    /// Leaves a permanent record of an `ask_user` exchange in the transcript
    /// before the modal (which is otherwise the only place it's visible)
    /// closes. Returns the question's id.
    fn record_question_answer(&mut self, answer: &str) -> Option<String> {
        let modal = self.modal.question()?;
        let id = modal.question.id.clone();
        let prompt = modal.question.prompt.clone();
        self.lines.push(ChatLine::new(
            ChatRole::System,
            format!("asked: {prompt} — answered: {answer}"),
        ));
        Some(id)
    }
}
