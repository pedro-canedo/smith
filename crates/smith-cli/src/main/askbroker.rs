//! The one owner of the agent's blocking-ask oneshots.
//!
//! `PermissionAsk` and `QuestionAsk` each carry a `oneshot::Sender` that can
//! be consumed exactly once. With a single frontend that sender could live in
//! the frontend's own loop — and did. With two (the TUI and the web console),
//! exactly one task may own it, and this is that task: frontends learn of a
//! pending ask from the `AgentEvent` the agent already emits, submit answers
//! over one shared channel, and the first answer for an id wins. The winner's
//! resolution is announced as `PermissionResolved`/`QuestionResolved`, which
//! is how the losing frontend finds out its modal went stale.
//!
//! Runs whenever the TUI does — console enabled or not — so interactive mode
//! has exactly one ask-resolution mechanism rather than one per frontend.
//! Headless keeps its own (`--allowed-tools`) and never starts this.

use std::collections::HashMap;

use smith_core::{
    AgentEvent, AskAnswer, PermissionAsk, PermissionDecision, QuestionAsk, SubmittedAnswer,
};
use tokio::sync::{mpsc, oneshot};

pub(crate) async fn run(
    mut permission_rx: mpsc::UnboundedReceiver<PermissionAsk>,
    mut question_rx: mpsc::UnboundedReceiver<QuestionAsk>,
    mut answers: mpsc::UnboundedReceiver<SubmittedAnswer>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
) {
    let mut permissions: HashMap<String, oneshot::Sender<PermissionDecision>> = HashMap::new();
    let mut questions: HashMap<String, oneshot::Sender<Result<String, String>>> = HashMap::new();

    loop {
        tokio::select! {
            // Biased, asks first: when an ask and its answer are both queued
            // — which tests do on purpose and a fast web client could do by
            // accident — the ask must be registered before the answer looks
            // it up. Random polling order would drop such an answer as
            // "unknown" and leave the agent waiting on a oneshot nobody
            // holds the other end of any more.
            biased;

            ask = permission_rx.recv() => {
                let Some(ask) = ask else { break };
                // A cancelled turn drops its receiver without a word; pruning
                // on arrival keeps the maps from accreting dead entries over
                // a long session.
                permissions.retain(|_, tx| !tx.is_closed());
                permissions.insert(ask.request.tool_call_id.clone(), ask.respond_to);
            }
            ask = question_rx.recv() => {
                let Some(ask) = ask else { break };
                questions.retain(|_, tx| !tx.is_closed());
                questions.insert(ask.question.id.clone(), ask.respond_to);
            }
            submitted = answers.recv() => {
                let Some(SubmittedAnswer { source, answer }) = submitted else { break };
                match answer {
                    AskAnswer::Permission { tool_call_id, decision } => {
                        // An unknown id is not an error: the other frontend
                        // answered first, or the turn was cancelled. The
                        // answer is dropped and no event claims otherwise.
                        let Some(tx) = permissions.remove(&tool_call_id) else { continue };
                        // A send failure means the agent stopped waiting
                        // (cancelled); announcing a resolution for it would
                        // put a decision in the transcript that decided
                        // nothing.
                        if tx.send(decision).is_ok() {
                            let _ = event_tx.send(AgentEvent::PermissionResolved {
                                tool_call_id,
                                decision,
                                source,
                            });
                        }
                    }
                    AskAnswer::Question { id, answer } => {
                        let Some(tx) = questions.remove(&id) else { continue };
                        if tx.send(Ok(answer)).is_ok() {
                            let _ = event_tx.send(AgentEvent::QuestionResolved { id, source });
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use smith_core::{
        AgentEvent, AskAnswer, AskSource, PermissionAsk, PermissionDecision, PermissionRequest,
        QuestionAsk, SubmittedAnswer, UserQuestion,
    };
    use tokio::sync::{mpsc, oneshot};

    struct Harness {
        permission_tx: mpsc::UnboundedSender<PermissionAsk>,
        question_tx: mpsc::UnboundedSender<QuestionAsk>,
        answer_tx: mpsc::UnboundedSender<SubmittedAnswer>,
        event_rx: mpsc::UnboundedReceiver<AgentEvent>,
        broker: tokio::task::JoinHandle<()>,
    }

    fn harness() -> Harness {
        let (permission_tx, permission_rx) = mpsc::unbounded_channel();
        let (question_tx, question_rx) = mpsc::unbounded_channel();
        let (answer_tx, answer_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let broker = tokio::spawn(super::run(permission_rx, question_rx, answer_rx, event_tx));
        Harness {
            permission_tx,
            question_tx,
            answer_tx,
            event_rx,
            broker,
        }
    }

    fn permission_ask(id: &str) -> (PermissionAsk, oneshot::Receiver<PermissionDecision>) {
        let (respond_to, rx) = oneshot::channel();
        (
            PermissionAsk {
                request: PermissionRequest {
                    tool_call_id: id.to_string(),
                    tool_name: "run_bash".to_string(),
                    detail: "cargo test".to_string(),
                },
                respond_to,
            },
            rx,
        )
    }

    fn answer(id: &str, decision: PermissionDecision, source: AskSource) -> SubmittedAnswer {
        SubmittedAnswer {
            source,
            answer: AskAnswer::Permission {
                tool_call_id: id.to_string(),
                decision,
            },
        }
    }

    #[tokio::test]
    async fn the_first_answer_wins_and_the_second_is_dropped() {
        let mut h = harness();
        let (ask, decision_rx) = permission_ask("call_1");
        h.permission_tx.send(ask).unwrap();

        h.answer_tx
            .send(answer(
                "call_1",
                PermissionDecision::AllowOnce,
                AskSource::Web,
            ))
            .unwrap();
        h.answer_tx
            .send(answer("call_1", PermissionDecision::Deny, AskSource::Tui))
            .unwrap();

        // The agent got the web's decision, not the TUI's late veto.
        assert_eq!(decision_rx.await.unwrap(), PermissionDecision::AllowOnce);

        // Exactly one resolution is announced, naming the winner.
        match h.event_rx.recv().await.unwrap() {
            AgentEvent::PermissionResolved {
                tool_call_id,
                decision,
                source,
            } => {
                assert_eq!(tool_call_id, "call_1");
                assert_eq!(decision, PermissionDecision::AllowOnce);
                assert_eq!(source, AskSource::Web);
            }
            other => panic!("expected PermissionResolved, got {other:?}"),
        }
        h.broker.abort();
    }

    #[tokio::test]
    async fn an_answer_for_an_unknown_ask_resolves_nothing_and_announces_nothing() {
        let mut h = harness();
        h.answer_tx
            .send(answer(
                "ghost",
                PermissionDecision::AllowOnce,
                AskSource::Web,
            ))
            .unwrap();

        // Force a round trip through the broker so the answer was processed:
        // a real ask answered afterwards still works, and the only event is its.
        let (ask, decision_rx) = permission_ask("real");
        h.permission_tx.send(ask).unwrap();
        h.answer_tx
            .send(answer("real", PermissionDecision::Deny, AskSource::Tui))
            .unwrap();
        assert_eq!(decision_rx.await.unwrap(), PermissionDecision::Deny);

        match h.event_rx.recv().await.unwrap() {
            AgentEvent::PermissionResolved { tool_call_id, .. } => {
                assert_eq!(tool_call_id, "real", "the ghost must not have resolved");
            }
            other => panic!("expected PermissionResolved, got {other:?}"),
        }
        h.broker.abort();
    }

    /// A cancelled turn drops the oneshot receiver. Answering it then must not
    /// announce a resolution — the decision decided nothing.
    #[tokio::test]
    async fn an_answer_whose_agent_stopped_waiting_is_not_announced() {
        let mut h = harness();
        let (ask, decision_rx) = permission_ask("cancelled");
        h.permission_tx.send(ask).unwrap();
        drop(decision_rx);

        h.answer_tx
            .send(answer(
                "cancelled",
                PermissionDecision::AllowOnce,
                AskSource::Tui,
            ))
            .unwrap();

        // Settle the broker with a second, live exchange; its event must be the
        // first and only one.
        let (ask, live_rx) = permission_ask("live");
        h.permission_tx.send(ask).unwrap();
        h.answer_tx
            .send(answer(
                "live",
                PermissionDecision::AllowOnce,
                AskSource::Tui,
            ))
            .unwrap();
        assert_eq!(live_rx.await.unwrap(), PermissionDecision::AllowOnce);

        match h.event_rx.recv().await.unwrap() {
            AgentEvent::PermissionResolved { tool_call_id, .. } => {
                assert_eq!(tool_call_id, "live");
            }
            other => panic!("expected PermissionResolved, got {other:?}"),
        }
        h.broker.abort();
    }

    #[tokio::test]
    async fn a_question_resolution_carries_the_answer_to_the_agent_and_the_id_to_the_event() {
        let mut h = harness();
        let (respond_to, answer_rx) = oneshot::channel();
        h.question_tx
            .send(QuestionAsk {
                question: UserQuestion {
                    id: "q1".to_string(),
                    prompt: "which one?".to_string(),
                    options: ["a".to_string(), "b".to_string(), "c".to_string()],
                },
                respond_to,
            })
            .unwrap();

        h.answer_tx
            .send(SubmittedAnswer {
                source: AskSource::Web,
                answer: AskAnswer::Question {
                    id: "q1".to_string(),
                    answer: "b".to_string(),
                },
            })
            .unwrap();

        assert_eq!(answer_rx.await.unwrap(), Ok("b".to_string()));
        match h.event_rx.recv().await.unwrap() {
            AgentEvent::QuestionResolved { id, source } => {
                assert_eq!(id, "q1");
                assert_eq!(source, AskSource::Web);
            }
            other => panic!("expected QuestionResolved, got {other:?}"),
        }
        h.broker.abort();
    }
}
