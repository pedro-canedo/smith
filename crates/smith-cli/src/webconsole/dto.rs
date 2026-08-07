//! What the console may ask the orchestrator to do.
//!
//! A deliberate whitelist, not `serde(Deserialize)` on `Action`. A blanket
//! derive would put `Quit` (kills the terminal session), `SetPermissionPolicy`
//! (changes standing authority) and `Mcp` one JSON body away from any token
//! holder — the Guard philosophy applied to the payload: refuse unless the
//! variant is affirmatively listed. Growing this enum is a deliberate act
//! with its own review, exactly like adding a row to a route table.
//!
//! Tagged `type`/`data` to match `AgentEvent`'s wire shape, so the two sides
//! of the protocol read the same.

use serde::Deserialize;
use smith_core::Action;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ActionDto {
    /// A new prompt. The client sends this when the session is idle.
    SubmitMessage(String),
    /// A mid-turn message, folded into the running turn at its next round
    /// boundary. The client sends this while a turn is in flight.
    Interject(String),
    /// Esc, as a button.
    CancelGeneration,
}

impl ActionDto {
    pub fn into_action(self) -> Action {
        match self {
            ActionDto::SubmitMessage(text) => Action::SubmitMessage(text),
            ActionDto::Interject(text) => Action::Interject(text),
            ActionDto::CancelGeneration => Action::CancelGeneration,
        }
    }

    /// The prompt text, when this action starts or redirects a turn — what
    /// the projection records as the user's own transcript item.
    pub fn user_text(&self) -> Option<&str> {
        match self {
            ActionDto::SubmitMessage(text) | ActionDto::Interject(text) => Some(text),
            ActionDto::CancelGeneration => None,
        }
    }
}

/// An answer to a pending ask, as POSTed by the console.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AskAnswerDto {
    Permission {
        tool_call_id: String,
        decision: smith_core::PermissionDecision,
    },
    Question {
        id: String,
        answer: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_listed_actions_deserialize_and_convert() {
        let dto: ActionDto =
            serde_json::from_str(r#"{"type":"submit_message","data":"hello"}"#).unwrap();
        assert!(matches!(dto.into_action(), Action::SubmitMessage(t) if t == "hello"));

        let dto: ActionDto =
            serde_json::from_str(r#"{"type":"interject","data":"also this"}"#).unwrap();
        assert!(matches!(dto.into_action(), Action::Interject(t) if t == "also this"));

        let dto: ActionDto = serde_json::from_str(r#"{"type":"cancel_generation"}"#).unwrap();
        assert!(matches!(dto.into_action(), Action::CancelGeneration));
    }

    /// The whole point of the DTO: what is not listed does not parse. `Quit`
    /// exists on `Action`; a body naming it must be a 400, not a dispatch.
    #[test]
    fn quit_and_policy_changes_are_not_deserializable() {
        for body in [
            r#"{"type":"quit"}"#,
            r#"{"type":"set_permission_policy","data":{"policy":"skip","save":true}}"#,
            r#"{"type":"mcp","data":{"command":"status"}}"#,
            r#"{"type":"rewind","data":{"turn":null,"apply":true,"force":true}}"#,
        ] {
            assert!(
                serde_json::from_str::<ActionDto>(body).is_err(),
                "{body} must not parse"
            );
        }
    }

    #[test]
    fn an_ask_answer_parses_both_kinds() {
        let dto: AskAnswerDto = serde_json::from_str(
            r#"{"kind":"permission","tool_call_id":"c1","decision":"allow_once"}"#,
        )
        .unwrap();
        assert!(matches!(dto, AskAnswerDto::Permission { .. }));

        let dto: AskAnswerDto =
            serde_json::from_str(r#"{"kind":"question","id":"q1","answer":"b"}"#).unwrap();
        assert!(matches!(dto, AskAnswerDto::Question { .. }));
    }
}
