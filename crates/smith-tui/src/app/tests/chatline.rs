//! Grouping repeated tool calls into one card, and line stamps.

use super::*;

/// A research turn issues six searches in a row; six cards is six times
/// the chrome for one activity, and it buries what the agent is doing.
#[test]
fn consecutive_searches_collapse_into_one_card() {
    let mut app = test_app();
    search(&mut app, "s1", "rust 1.97 release");
    search(&mut app, "s2", "rust release schedule");
    search(&mut app, "s3", "rust beta 1.98");

    let cards: Vec<&ChatLine> = app
        .lines
        .iter()
        .filter(|l| l.role == ChatRole::Tool)
        .collect();
    assert_eq!(cards.len(), 1, "one card per run of searches");
    assert_eq!(cards[0].grouped().len(), 2, "the other two folded in");
    assert_eq!(cards[0].grouped()[0].label, "rust release schedule");
}

/// The card is done only once nothing under it is still running.
#[test]
fn a_grouped_card_stays_running_until_its_last_child_lands() {
    let mut app = test_app();
    search(&mut app, "s1", "one");
    search(&mut app, "s2", "two");

    finish(&mut app, "s1");
    let card = app.lines.iter().find(|l| l.role == ChatRole::Tool).unwrap();
    assert_eq!(card.tool_status(), Some(ActivityStatus::Running));

    finish(&mut app, "s2");
    let card = app.lines.iter().find(|l| l.role == ChatRole::Tool).unwrap();
    assert_eq!(card.tool_status(), Some(ActivityStatus::Done));
}

/// A real research burst alternates searching and fetching. Keying the group
/// on the tool *name* gave that one card per call — the stack the grouping
/// exists to remove.
#[test]
fn searches_and_fetches_share_one_research_card() {
    let mut app = test_app();
    search(&mut app, "s1", "brasileirão rodada 21");
    fetch(&mut app, "f1", "https://ge.globo.com/brasileirao");
    search(&mut app, "s2", "brasileirão placares");
    fetch(&mut app, "f2", "https://flashscore.com.br/serie-a");

    let cards: Vec<_> = app
        .lines
        .iter()
        .filter(|l| l.role == ChatRole::Tool)
        .collect();
    assert_eq!(cards.len(), 1, "the run split into {} cards", cards.len());
    assert_eq!(cards[0].group_summary().steps, 4);
}

/// A `+ Thought: 4.0s` row is the pause *inside* one activity — the card's own
/// timer already covers it. Letting it close the run put the transcript back
/// to one card per search, which is exactly the wall being removed here.
#[test]
fn a_thought_row_does_not_break_a_research_run() {
    let mut app = test_app();
    search(&mut app, "s1", "one");
    finish(&mut app, "s1");
    app.lines.push(ChatLine::new(ChatRole::Thought, "4.0s"));
    search(&mut app, "s2", "two");

    let cards: Vec<_> = app
        .lines
        .iter()
        .filter(|l| l.role == ChatRole::Tool)
        .collect();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].group_summary().steps, 2);
    assert!(
        !app.lines.iter().any(|l| l.role == ChatRole::Thought),
        "the row was stepped over instead of dropped, so the group's newest \
         step now renders above a separator belonging to the previous one"
    );
}

/// …but a reply between two searches does. That is the agent moving on, and
/// folding the next call into a card above it would reorder the transcript.
#[test]
fn a_reply_between_two_searches_closes_the_run() {
    let mut app = test_app();
    search(&mut app, "s1", "one");
    finish(&mut app, "s1");
    app.lines
        .push(ChatLine::new(ChatRole::Assistant, "ESPN só tem sábado"));
    search(&mut app, "s2", "two");

    assert_eq!(
        app.lines
            .iter()
            .filter(|l| l.role == ChatRole::Tool)
            .count(),
        2
    );
}

/// One blocked search among several does not make the whole run a failure —
/// the header counts it instead. A burst that loses two fetches to a 404 and
/// answers on the other eight is a research that worked.
#[test]
fn one_failed_step_is_counted_not_promoted_to_the_whole_group() {
    let mut app = test_app();
    search(&mut app, "s1", "one");
    search(&mut app, "s2", "two");
    finish(&mut app, "s1");
    fail(&mut app, "s2", "blocked");

    let card = app.lines.iter().find(|l| l.role == ChatRole::Tool).unwrap();
    assert_eq!(card.tool_status(), Some(ActivityStatus::Done));
    let summary = card.group_summary();
    assert_eq!(summary.steps, 2);
    assert_eq!(summary.failed, 1, "the failure has to be stated somewhere");
}

/// A run where nothing got through is the case that really failed.
#[test]
fn a_group_whose_every_step_failed_reports_failure() {
    let mut app = test_app();
    search(&mut app, "s1", "one");
    search(&mut app, "s2", "two");
    fail(&mut app, "s1", "blocked");
    fail(&mut app, "s2", "blocked");

    let card = app.lines.iter().find(|l| l.role == ChatRole::Tool).unwrap();
    assert_eq!(card.tool_status(), Some(ActivityStatus::Error));
    assert_eq!(card.group_summary().failed, 2);
}

/// Only *consecutive* calls fold. A search after a reply is a new
/// activity, and joining it to a card further up would reorder history.
#[test]
fn a_search_after_something_else_starts_a_new_card() {
    let mut app = test_app();
    search(&mut app, "s1", "one");
    finish(&mut app, "s1");
    app.lines
        .push(ChatLine::new(ChatRole::Assistant, "here is what I found"));
    search(&mut app, "s2", "two");

    let cards = app
        .lines
        .iter()
        .filter(|l| l.role == ChatRole::Tool)
        .count();
    assert_eq!(cards, 2);
}

#[test]
fn expanding_one_card_stamps_only_that_card() {
    // The whole reason expansion is a field of `ChatLine` and not of
    // `App`: a global flag would have to join `LayoutKey` and re-render
    // the entire transcript on every `Enter`.
    let mut app = app_with_cards(3);
    let before: Vec<LineStamp> = app.lines.iter().map(ChatLine::stamp).collect();
    app.toggle_card_focus();
    app.toggle_selected_card();
    let after: Vec<LineStamp> = app.lines.iter().map(ChatLine::stamp).collect();

    let moved = before.iter().zip(&after).filter(|(a, b)| a != b).count();
    assert_eq!(moved, 1, "selection + expansion touched {moved} lines");
}
