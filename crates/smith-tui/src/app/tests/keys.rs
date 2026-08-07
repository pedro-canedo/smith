//! `on_key`: the modal guards, the prompt, history, and quit arming.

use super::*;

#[test]
fn ctrl_l_opens_the_log_panel_and_closes_it_again() {
    let mut app = test_app();
    app.logs.push(crate::logbuf::LogLine {
        level: crate::logbuf::LogLevel::Warn,
        target: "smith_mcp::transport".to_string(),
        message: "unparseable frame".to_string(),
    });

    app.on_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
    let overlay = app.overlay.as_ref().expect("panel should be open");
    assert_eq!(overlay.title, LOG_PANEL_TITLE);
    let OverlayBody::Lines(lines) = &overlay.body else {
        panic!("expected lines");
    };
    assert!(lines[0].contains("WARN"), "{lines:?}");
    assert!(lines[0].contains("unparseable frame"), "{lines:?}");

    app.on_key(KeyCode::Char('l'), KeyModifiers::CONTROL);
    assert!(app.overlay.is_none(), "the same key closes it");
}

#[test]
fn the_arrow_keys_scroll_an_overlay_rather_than_the_transcript() {
    let mut app = test_app();
    app.run_slash_command("usage");
    app.on_key(KeyCode::Down, KeyModifiers::NONE);
    app.on_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.overlay.as_ref().unwrap().scroll, 2);
    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.overlay.as_ref().unwrap().scroll, 1);
    // Never off the top.
    app.on_key(KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(app.overlay.as_ref().unwrap().scroll, 0);
}

#[test]
fn up_walks_back_through_submitted_prompts_and_down_walks_forward() {
    let mut app = test_app();
    submit(&mut app, "first");
    app.waiting_on_assistant = false;
    submit(&mut app, "second");
    app.waiting_on_assistant = false;

    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.text(), "second", "most recent comes back first");
    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.text(), "first");
    app.on_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.input.text(), "second");
}

/// Walking back must not eat what was already typed.
#[test]
fn walking_forward_past_the_newest_entry_restores_the_draft() {
    let mut app = test_app();
    submit(&mut app, "old prompt");
    app.waiting_on_assistant = false;
    app.input.set("half-typed");

    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.text(), "old prompt");
    app.on_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.input.text(), "half-typed");
}

#[test]
fn history_stops_at_the_oldest_entry_instead_of_wrapping() {
    let mut app = test_app();
    submit(&mut app, "only one");
    app.waiting_on_assistant = false;
    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.input.text(), "only one");
}

/// With nothing to recall, the arrows keep their old meaning rather than
/// becoming inert keys.
#[test]
fn an_empty_history_leaves_the_arrows_scrolling_the_transcript() {
    let mut app = test_app();
    app.scroll = 5;
    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.scroll, 4);
    assert!(app.input.is_empty());
}

#[test]
fn resubmitting_the_same_prompt_does_not_double_it_in_the_history() {
    let mut app = test_app();
    submit(&mut app, "same");
    app.waiting_on_assistant = false;
    submit(&mut app, "same");
    app.waiting_on_assistant = false;
    assert_eq!(app.history.entries, vec!["same".to_string()]);
}

#[test]
fn typing_an_at_token_switches_the_suggestion_list_to_files() {
    let mut app = test_app();
    app.input.set("look at @Cargo");
    // Seeded directly: the real index walks the filesystem, which is not
    // what this test is about.
    app.file_index = Some(vec!["Cargo.toml".to_string(), "src/app.rs".to_string()]);
    let hints = app.suggestions();
    assert_eq!(app.completion_kind, CompletionKind::File);
    assert_eq!(hints.first().map(|h| h.name.as_str()), Some("Cargo.toml"));
}

#[test]
fn tab_accepts_a_file_suggestion_into_the_prompt() {
    let mut app = test_app();
    app.file_index = Some(vec!["crates/smith-tui/src/app.rs".to_string()]);
    app.input.set("explain @app.rs");
    app.on_key(KeyCode::Tab, KeyModifiers::NONE);
    assert_eq!(app.input.text(), "explain @crates/smith-tui/src/app.rs ");
}

/// A slash command must still complete as a slash command.
#[test]
fn the_file_list_does_not_displace_slash_completion() {
    let mut app = test_app();
    app.input.set("/he");
    let hints = app.suggestions();
    assert_eq!(app.completion_kind, CompletionKind::Slash);
    assert!(hints.iter().any(|h| h.name == "help"), "{hints:?}");
}

/// Slash commands are how you steer a running turn, so queueing one would
/// be the opposite of what it is for.
#[test]
fn a_slash_command_typed_mid_turn_still_runs_now() {
    let mut app = test_app();
    submit(&mut app, "first");
    submit(&mut app, "second");
    assert_eq!(app.queued.len(), 1);

    app.input.set("/queue clear");
    app.on_key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        app.queued.is_empty(),
        "the command was queued instead of run"
    );
    assert!(app
        .lines
        .last()
        .is_some_and(|l| l.text.contains("dropped the queued message")));
}

/// The motivating case for remappable keys: tmux owns Ctrl+B by default,
/// so a tmux user cannot press it at all.
#[test]
fn a_remapped_key_takes_effect_and_the_old_one_goes_inert() {
    let mut app = test_app();
    app.keys = crate::keymap::KeyMap::from_overrides([("toggle_sidebar", "ctrl+t")]).unwrap();

    app.on_key(KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert!(app.sidebar_visible, "the old binding did nothing");

    app.on_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(!app.sidebar_visible, "the new binding works");
}

#[test]
fn ctrl_b_hides_and_restores_the_sidebar() {
    let mut app = test_app();
    assert!(app.sidebar_visible);
    app.on_key(KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert!(!app.sidebar_visible);
    app.on_key(KeyCode::Char('b'), KeyModifiers::CONTROL);
    assert!(app.sidebar_visible);
}

#[test]
fn shift_tab_cycles_the_sidebar_tabs_and_wraps() {
    let mut app = test_app();
    assert_eq!(app.sidebar_tab, SidebarTab::Session);
    app.on_key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(app.sidebar_tab, SidebarTab::Tasks);
    app.on_key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(app.sidebar_tab, SidebarTab::Vitals);
    app.on_key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert_eq!(app.sidebar_tab, SidebarTab::Session);
}

/// Cycling a hidden sidebar would be a keystroke with no visible effect.
#[test]
fn shift_tab_reveals_a_hidden_sidebar_before_it_cycles_anything() {
    let mut app = test_app();
    app.sidebar_visible = false;
    app.on_key(KeyCode::BackTab, KeyModifiers::SHIFT);
    assert!(app.sidebar_visible);
    assert_eq!(app.sidebar_tab, SidebarTab::Session, "nothing cycled yet");
}

#[test]
fn submitting_a_message_increments_request_count() {
    let mut app = test_app();
    for c in "hi".chars() {
        app.on_key(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        );
    }
    app.on_key(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );
    assert_eq!(app.request_count, 1);
}

/// A transcript with `count` tool cards separated by assistant replies,
/// so navigation has to actually skip non-card lines.
#[test]
fn ctrl_o_focuses_the_newest_card_and_releases_it() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut app = app_with_cards(3);
    assert_eq!(app.selected_card_id(), None);

    assert!(app
        .on_key(KeyCode::Char('o'), KeyModifiers::CONTROL)
        .is_none());
    assert_eq!(app.selected_card_id(), Some("call_2"), "newest card first");

    app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(app.selected_card_id(), None);
}

#[test]
fn ctrl_o_is_inert_with_nothing_to_select() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut app = test_app();
    app.lines
        .push(ChatLine::new(ChatRole::Assistant, "sem tool nenhuma"));
    app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert_eq!(app.selected_card_id(), None);
}

#[test]
fn arrows_walk_between_cards_and_clamp_at_the_ends() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut app = app_with_cards(3);
    app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);

    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.selected_card_id(), Some("call_1"));
    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.selected_card_id(), Some("call_0"));
    // Clamped, not wrapped: wrapping to the newest card from the oldest is
    // never what the keystroke meant in a long session.
    app.on_key(KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(app.selected_card_id(), Some("call_0"));

    app.on_key(KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(app.selected_card_id(), Some("call_1"));
}

#[test]
fn enter_expands_only_the_selected_card() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut app = app_with_cards(3);
    app.on_key(KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(app.on_key(KeyCode::Enter, KeyModifiers::NONE).is_none());

    let expanded: Vec<&str> = app
        .lines
        .iter()
        .filter(|l| l.expanded())
        .filter_map(ChatLine::tool_id)
        .collect();
    assert_eq!(expanded, vec!["call_2"]);
    assert!(
        !app.verbose_tools,
        "per-card expansion must not flip the global default"
    );

    // Enter toggles, it doesn't latch.
    app.on_key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(app.lines.iter().all(|l| !l.expanded()));
}

#[test]
fn esc_releases_card_focus_but_never_steals_a_cancel() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut app = app_with_cards(2);
    app.toggle_card_focus();
    app.waiting_on_assistant = true;
    // A running turn keeps Esc: cancelling is the more urgent meaning.
    assert!(matches!(
        app.on_key(KeyCode::Esc, KeyModifiers::NONE),
        Some(Action::CancelGeneration)
    ));
    assert!(app.selected_card_id().is_some());

    app.waiting_on_assistant = false;
    assert!(app.on_key(KeyCode::Esc, KeyModifiers::NONE).is_none());
    assert_eq!(app.selected_card_id(), None);
}

#[test]
fn card_focus_leaves_typing_alone() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut app = app_with_cards(1);
    app.toggle_card_focus();
    for c in "oi".chars() {
        app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    assert_eq!(app.input.text(), "oi");
    assert!(app.selected_card_id().is_some());
}

#[test]
fn plan_modal_y_approves_and_starts_build() {
    let mut app = test_app();
    app.modal = Modal::Plan(crate::app::PlanModal {
        text: "step 1".into(),
        scroll: 0,
    });
    app.plan_gated = true;
    let action = app.on_key(
        crossterm::event::KeyCode::Char('y'),
        crossterm::event::KeyModifiers::NONE,
    );
    assert!(matches!(action, Some(Action::ApprovePlan)));
    assert!(app.modal.is_none());
    assert!(app.waiting_on_assistant);
}

#[test]
fn slash_tab_completes_partial_command() {
    let mut app = test_app();
    app.input.set("/pl");
    let action = app.on_key(
        crossterm::event::KeyCode::Tab,
        crossterm::event::KeyModifiers::NONE,
    );
    assert!(action.is_none());
    assert_eq!(app.input.text(), "/plan ");
}

#[test]
fn typing_past_the_box_width_keeps_every_character() {
    // The old `Paragraph` had no wrap and no scroll, so anything past the
    // box width was silently clipped and looked lost.
    let mut app = test_app();
    for c in "a".repeat(300).chars() {
        app.on_key(
            crossterm::event::KeyCode::Char(c),
            crossterm::event::KeyModifiers::NONE,
        );
    }
    assert_eq!(app.input.text().chars().count(), 300);
}

#[test]
fn caret_keys_edit_the_prompt_instead_of_scrolling_the_transcript() {
    let mut app = test_app();
    app.input.set("helo");
    app.scroll = 5;
    app.on_key(
        crossterm::event::KeyCode::Left,
        crossterm::event::KeyModifiers::NONE,
    );
    app.on_key(
        crossterm::event::KeyCode::Char('l'),
        crossterm::event::KeyModifiers::NONE,
    );
    assert_eq!(app.input.text(), "hello");
    assert_eq!(app.scroll, 5, "Left must not touch the message pane");
}

#[test]
fn alt_enter_inserts_a_newline_instead_of_submitting() {
    let mut app = test_app();
    app.input.set("first");
    let action = app.on_key(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::ALT,
    );
    assert!(action.is_none(), "Alt+Enter must not submit");
    assert!(!app.waiting_on_assistant);
    assert_eq!(app.input.text(), "first\n");
}

#[test]
fn ctrl_j_inserts_a_newline_for_terminals_without_shift_enter() {
    let mut app = test_app();
    app.input.set("first");
    let action = app.on_key(
        crossterm::event::KeyCode::Char('j'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    assert!(action.is_none());
    assert_eq!(app.input.text(), "first\n");
}

#[test]
fn bare_enter_still_submits_a_multi_line_prompt() {
    let mut app = test_app();
    app.input.set("first");
    app.on_key(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::ALT,
    );
    app.input.insert_str("second");
    let action = app.on_key(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );
    assert!(matches!(action, Some(Action::SubmitMessage(t)) if t == "first\nsecond"));
}

#[test]
fn arrows_still_scroll_the_transcript_when_the_prompt_is_one_row() {
    // Regression guard: Up/Down are shared between the caret and the
    // message pane, and the pane must keep them for the common case.
    let mut app = test_app();
    app.scroll = 5;
    app.on_key(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::NONE,
    );
    assert_eq!(app.scroll, 4);
    assert!(!app.follow_bottom);
    app.on_key(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::NONE,
    );
    assert_eq!(app.scroll, 5);
}

#[test]
fn arrows_walk_a_multi_line_prompt_before_reaching_the_transcript() {
    let mut app = test_app();
    app.input.set("one");
    app.input.insert_newline();
    app.input.insert_str("two");
    app.scroll = 5;

    app.on_key(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::NONE,
    );
    assert_eq!(app.scroll, 5, "caret moved, pane untouched");

    // Caret is on the first row now, so the next Up belongs to the pane.
    app.on_key(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::NONE,
    );
    assert_eq!(app.scroll, 4);
}

#[test]
fn paste_keeps_newlines_instead_of_submitting_at_the_first_one() {
    let mut app = test_app();
    app.on_paste("line one\nline two");
    assert_eq!(app.input.text(), "line one\nline two");
    assert!(!app.waiting_on_assistant, "paste must never submit");
}

#[test]
fn question_modal_digit_one_submits_option_a() {
    let mut app = test_app();
    app.modal = Modal::Question(QuestionModal {
        question: UserQuestion {
            id: "q1".into(),
            prompt: "Which approach?".into(),
            options: ["Alpha".into(), "Beta".into(), "Gamma".into()],
        },
        selected: 0,
        custom: String::new(),
    });
    let action = app.on_key(
        crossterm::event::KeyCode::Char('1'),
        crossterm::event::KeyModifiers::NONE,
    );
    // The answer carries the question's id: the modal is already closed by
    // the time the action is handled, so the id is the only pairing.
    assert!(matches!(
        action,
        Some(Action::QuestionResponse { ref id, ref answer }) if id == "q1" && answer == "Alpha"
    ));
    assert!(app.modal.is_none());
}

#[test]
fn ctrl_c_with_a_question_modal_open_arms_the_quit_instead_of_typing_c() {
    // The modal branch used to swallow it via `Char(c) if !c.is_control()`.
    let mut app = test_app();
    app.modal = question_modal();
    assert!(ctrl_c(&mut app).is_none());
    assert_eq!(app.modal.question().unwrap().custom, "");
    assert_eq!(app.modal.question().unwrap().selected, 0);
    assert!(app.quit_pending());
    assert!(!app.should_quit);
}

#[test]
fn ctrl_c_with_plan_or_permission_modal_open_arms_the_quit() {
    // Both branches used to fall through to `_ => None`: no way out at all.
    for modal in [
        Modal::Plan(PlanModal {
            text: "step 1".into(),
            scroll: 0,
        }),
        permission_modal(),
    ] {
        let mut app = test_app();
        app.modal = modal;
        assert!(ctrl_c(&mut app).is_none());
        assert!(app.quit_pending());
        assert!(app.modal.is_some(), "the modal must stay up until we quit");
        assert!(matches!(ctrl_c(&mut app), Some(Action::Quit)));
        assert!(app.should_quit);
    }
}

#[test]
fn quitting_takes_two_ctrl_c_presses() {
    let mut app = test_app();
    assert!(ctrl_c(&mut app).is_none());
    assert!(!app.should_quit, "one press must never discard the session");
    assert!(matches!(ctrl_c(&mut app), Some(Action::Quit)));
    assert!(app.should_quit);
}

#[test]
fn any_other_key_disarms_a_pending_quit() {
    let mut app = test_app();
    ctrl_c(&mut app);
    app.on_key(
        crossterm::event::KeyCode::Char('h'),
        crossterm::event::KeyModifiers::NONE,
    );
    assert!(!app.quit_pending());
    assert!(ctrl_c(&mut app).is_none(), "this is a fresh first press");
    assert!(!app.should_quit);
}

#[test]
fn a_stale_arm_expires_instead_of_pairing_with_a_later_press() {
    let mut app = test_app();
    app.quit_armed_at = Some(Instant::now() - QUIT_CONFIRM_WINDOW - Duration::from_secs(1));
    assert!(!app.quit_pending());
    assert!(app.expire_pending_quit(), "the lapsed hint needs a repaint");
    assert!(!app.expire_pending_quit(), "only once");
    assert!(ctrl_c(&mut app).is_none());
    assert!(!app.should_quit);
}

/// Typing filters. Substring, not sub-sequence — `gpt` matching
/// `google/gemma-4-31b-it` is a picker that surprises you.
#[test]
fn typing_narrows_the_list_by_substring() {
    let mut app = picker_app(&[
        ("gpt-oss:20b-cloud", true),
        ("gemma4:31b-cloud", true),
        ("nemotron-3-super:cloud", true),
    ]);
    for c in "gpt".chars() {
        app.on_key(KeyCode::Char(c), KeyModifiers::NONE);
    }
    let picker = app.modal.model().unwrap();
    let ids: Vec<&str> = picker.matches().iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gpt-oss:20b-cloud"]);

    app.on_key(KeyCode::Backspace, KeyModifiers::NONE);
    app.on_key(KeyCode::Backspace, KeyModifiers::NONE);
    app.on_key(KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(app.modal.model().unwrap().matches().len(), 3);
}

/// Enter switches for this session only. Persisting on a keystroke would make
/// it outlive the conversation it was meant for.
#[test]
fn enter_switches_without_saving_and_esc_changes_nothing() {
    let mut app = picker_app(&[("a:cloud", true), ("b:cloud", true)]);
    app.on_key(KeyCode::Down, KeyModifiers::NONE);
    let action = app.on_key(KeyCode::Enter, KeyModifiers::NONE);
    match action {
        Some(Action::SwitchModel { model, save, .. }) => {
            assert_eq!(model, "b:cloud");
            assert!(!save, "a keystroke must not persist");
        }
        other => panic!("expected a switch, got {other:?}"),
    }
    assert!(app.modal.is_none());

    let mut app = picker_app(&[("a:cloud", true)]);
    assert!(app.on_key(KeyCode::Esc, KeyModifiers::NONE).is_none());
    assert!(app.modal.is_none(), "Esc closes without switching");
}

/// The cursor cannot leave the filtered list, and the window follows it.
#[test]
fn the_cursor_and_the_window_stay_inside_the_list() {
    let models: Vec<(&str, bool)> = vec![
        ("m0", true),
        ("m1", true),
        ("m2", true),
        ("m3", true),
        ("m4", true),
        ("m5", true),
        ("m6", true),
        ("m7", true),
    ];
    let mut app = picker_app(&models);
    for _ in 0..20 {
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
    }
    let picker = app.modal.model_mut().unwrap();
    picker.clamp(3);
    assert_eq!(picker.selected, 7, "clamped to the last row");
    assert_eq!(picker.scroll, 5, "the window followed it");

    for _ in 0..20 {
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
    }
    let picker = app.modal.model_mut().unwrap();
    picker.clamp(3);
    assert_eq!(picker.selected, 0);
    assert_eq!(picker.scroll, 0);
}

/// A page is the pane, not a constant. Ten rows was most of a short
/// terminal and a third of a tall one, so paging felt like a different key
/// at every size.
#[test]
fn a_page_key_moves_by_the_panes_height_less_an_overlap() {
    let mut app = test_app();
    app.message_area = ratatui::layout::Rect::new(0, 0, 80, 30);
    app.follow_bottom = false;
    app.scroll = 100;

    app.on_key(KeyCode::PageUp, KeyModifiers::NONE);
    assert_eq!(app.scroll, 100 - 28, "a page is height - 2 rows of overlap");

    app.on_key(KeyCode::PageDown, KeyModifiers::NONE);
    assert_eq!(app.scroll, 100);
}

/// Home and End belong to the caret while there is a prompt to move within
/// — taking them outright would cost line editing to buy scrolling.
#[test]
fn home_and_end_reach_the_transcript_only_when_the_prompt_is_empty() {
    let mut app = test_app();
    app.follow_bottom = false;
    app.scroll = 50;

    app.on_key(KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(app.scroll, 0, "an empty prompt lets Home reach the top");

    app.scroll = 50;
    app.input.insert_str("a draft");
    app.on_key(KeyCode::Home, KeyModifiers::NONE);
    assert_eq!(app.scroll, 50, "Home moved the caret, not the transcript");

    // ...and Ctrl takes them unconditionally, for typing and reading at once.
    app.on_key(KeyCode::Home, KeyModifiers::CONTROL);
    assert_eq!(app.scroll, 0);
    assert!(!app.follow_bottom);

    app.on_key(KeyCode::End, KeyModifiers::CONTROL);
    assert!(
        app.follow_bottom,
        "End is re-arming the tail, not a big offset"
    );
}
