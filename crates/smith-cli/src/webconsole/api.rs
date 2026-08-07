//! The console's JSON handlers — everything that is not the shell or the
//! event stream.
//!
//! Handlers return `(status, body)`; the server owns header writing. Errors
//! after admission carry `{"error": "..."}` — the Guard's refusals stay bare
//! by design, but an *admitted* caller is our own page and deserves to know
//! what went wrong.

use smith_core::{AskAnswer, AskSource, SubmittedAnswer};
use smith_store::SessionStore;

use super::dto::{ActionDto, AskAnswerDto};
use super::{Handles, Route};

pub fn error_body(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// Dispatches one admitted request. `Route::Root`, `Route::Shell` and
/// `Route::Events` never reach here — the server handles them (redirect,
/// page, stream) because their responses are not JSON.
pub async fn dispatch(handles: &Handles, route: Route, body: &str) -> (u16, String) {
    match route {
        Route::State => state(handles),
        Route::SubmitAction => submit_action(handles, body),
        Route::AskAnswer => ask_answer(handles, body),
        Route::Sessions => sessions(handles),
        Route::SessionMessages { session_id } => session_messages(handles, &session_id),
        Route::Tasks => tasks(handles),
        Route::Root | Route::Shell { .. } | Route::Events => {
            (500, error_body("route handled by the server, not the api"))
        }
    }
}

fn state(handles: &Handles) -> (u16, String) {
    match handles.tee.projection.read() {
        Ok(projection) => match serde_json::to_string(&*projection) {
            Ok(json) => (200, json),
            Err(e) => (500, error_body(&format!("serialize: {e}"))),
        },
        Err(_) => (500, error_body("projection lock poisoned")),
    }
}

fn tasks(handles: &Handles) -> (u16, String) {
    match handles.tee.projection.read() {
        Ok(projection) => match serde_json::to_string(&projection.tasks) {
            Ok(json) => (200, json),
            Err(e) => (500, error_body(&format!("serialize: {e}"))),
        },
        Err(_) => (500, error_body("projection lock poisoned")),
    }
}

fn submit_action(handles: &Handles, body: &str) -> (u16, String) {
    let dto: ActionDto = match serde_json::from_str(body) {
        Ok(dto) => dto,
        Err(e) => return (400, error_body(&format!("bad action: {e}"))),
    };
    // The user's own message is not an AgentEvent — the orchestrator never
    // echoes prompts back — so it is recorded here, at the same moment it is
    // forwarded, or a late-joining browser would never see its own question.
    if let Some(text) = dto.user_text() {
        if let Ok(mut projection) = handles.tee.projection.write() {
            projection.note_user_message(text);
        }
    }
    match handles.action_tx.send(dto.into_action()) {
        Ok(()) => (202, r#"{"ok":true}"#.to_string()),
        Err(_) => (500, error_body("the session has ended")),
    }
}

fn ask_answer(handles: &Handles, body: &str) -> (u16, String) {
    let dto: AskAnswerDto = match serde_json::from_str(body) {
        Ok(dto) => dto,
        Err(e) => return (400, error_body(&format!("bad answer: {e}"))),
    };

    // The 404 gate reads the projection, not the broker: the projection
    // clears its pending ask on the same resolution events the broker emits,
    // so "is there such an ask" has one answer everywhere. The broker still
    // drops unknown ids on its own — this is the polite error, not the
    // safety.
    let known = match (&dto, handles.tee.projection.read()) {
        (AskAnswerDto::Permission { tool_call_id, .. }, Ok(p)) => p
            .pending_permission
            .as_ref()
            .is_some_and(|r| &r.tool_call_id == tool_call_id),
        (AskAnswerDto::Question { id, .. }, Ok(p)) => {
            p.pending_question.as_ref().is_some_and(|q| &q.id == id)
        }
        (_, Err(_)) => false,
    };
    if !known {
        return (404, error_body("no such pending ask"));
    }

    let answer = match dto {
        AskAnswerDto::Permission {
            tool_call_id,
            decision,
        } => AskAnswer::Permission {
            tool_call_id,
            decision,
        },
        AskAnswerDto::Question { id, answer } => AskAnswer::Question { id, answer },
    };
    match handles.ask_tx.send(SubmittedAnswer {
        source: AskSource::Web,
        answer,
    }) {
        Ok(()) => (200, r#"{"ok":true}"#.to_string()),
        Err(_) => (500, error_body("the session has ended")),
    }
}

fn sessions(handles: &Handles) -> (u16, String) {
    match SessionStore::open(&handles.store_dir).and_then(|store| store.list_sessions(Some(50))) {
        Ok(list) => match serde_json::to_string(&list) {
            Ok(json) => (200, json),
            Err(e) => (500, error_body(&format!("serialize: {e}"))),
        },
        Err(e) => (500, error_body(&format!("store: {e}"))),
    }
}

fn session_messages(handles: &Handles, session_id: &str) -> (u16, String) {
    let store = match SessionStore::open(&handles.store_dir) {
        Ok(store) => store,
        Err(e) => return (500, error_body(&format!("store: {e}"))),
    };
    match store.session_exists(session_id) {
        Ok(false) => return (404, error_body("no such session")),
        Err(e) => return (500, error_body(&format!("store: {e}"))),
        Ok(true) => {}
    }
    match store.load_messages(session_id) {
        Ok(messages) => match serde_json::to_string(&messages) {
            Ok(json) => (200, json),
            Err(e) => (500, error_body(&format!("serialize: {e}"))),
        },
        Err(e) => (500, error_body(&format!("store: {e}"))),
    }
}
