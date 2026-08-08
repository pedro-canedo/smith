//! The action loop: one `match` over `Action`, run until the channel closes.

//! Owns "which provider/model" and the `Action` → `Agent` dispatch loop:
//! `run_orchestrator` is the async task that receives every `Action` the TUI
//! sends and drives the shared `Agent` accordingly (spawning one task per
//! action so a long-running turn never blocks e.g. `CancelGeneration` from
//! being handled). `main.rs` only builds the initial state and hands off to
//! this loop; prompt text/parsing lives in `prompts.rs`.

use smith_core::{Action, AgentEvent, AgentPhase, PermissionAsk, QuestionAsk};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::prompts::{build_approved_plan_prompt, last_assistant_plan_text};

use super::setup::Wiring;
use super::{live_models, persist_turn, run_loop};

/// Handles actions until the frontend drops its sender.
pub(super) async fn run(
    wiring: Wiring,
    mut action_rx: mpsc::UnboundedReceiver<Action>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
    permission_tx: mpsc::UnboundedSender<PermissionAsk>,
    question_tx: mpsc::UnboundedSender<QuestionAsk>,
) {
    let Wiring {
        state,
        interjections,
        mcp,
        config,
        provider_kind,
    } = wiring;

    let mut current_cancel: Option<CancellationToken> = None;

    while let Some(action) = action_rx.recv().await {
        match action {
            Action::SubmitMessage(text) => {
                drop_pending_duplicate(&interjections, &text);
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                // Spawned so CancelGeneration keeps flowing through this loop
                // while the turn is in flight, instead of blocking behind it.
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard
                        .agent
                        .run_turn(text, event_tx, permission_tx, question_tx, cancel)
                        .await;

                    persist_turn(&mut guard);
                });
            }
            Action::CancelGeneration => {
                if let Some(cancel) = current_cancel.take() {
                    cancel.cancel();
                }
            }
            Action::Interject(text) => {
                // Pushed onto the shared queue rather than through the agent:
                // `run_turn` holds the state lock for the whole turn, so the
                // only way to reach a turn in flight is a handle taken before
                // it started — the same reason the cancel token lives here.
                if let Ok(mut queue) = interjections.lock() {
                    queue.push_back(text);
                }
            }
            Action::ListModels => {
                // Spawned: a catalogue is a network call, and the action loop
                // must keep answering Esc while it is in flight.
                let config = config.clone();
                let event_tx = event_tx.clone();
                let provider = provider_kind;
                tokio::spawn(async move {
                    let models = live_models(provider, &config).await;
                    let _ = event_tx.send(AgentEvent::ModelsAvailable {
                        provider: provider.label().to_string(),
                        models,
                    });
                });
            }
            Action::SwitchModel {
                provider,
                model,
                save,
            } => {
                // Spawned (like SubmitMessage) so it doesn't block this loop
                // — e.g. behind a turn that's still holding the agent lock.
                let state = state.clone();
                let event_tx = event_tx.clone();
                let config = config.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    match guard.switch_model(provider, model, &config).await {
                        Ok((provider_label, model_label)) => {
                            let saved = if save {
                                // The global file only, and only these
                                // two fields — see `update_global`.
                                let model = model_label.clone();
                                smith_config::Config::update_global(move |global| {
                                    global.general.provider = Some(provider_label.to_string());
                                    global.general.model = Some(model);
                                })
                                .is_ok()
                            } else {
                                false
                            };
                            let _ = event_tx.send(AgentEvent::ModelChanged {
                                provider: provider_label.to_string(),
                                model: model_label,
                                saved,
                            });
                        }
                        Err(err) => {
                            let _ = event_tx.send(AgentEvent::Error(err));
                        }
                    }
                });
            }
            Action::SetPermissionPolicy { policy, save } => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard.agent.set_permission_policy(policy);

                    let saved = if save {
                        smith_config::Config::update_global(|global| {
                            global.general.permission_policy = Some(policy.as_str().to_string());
                        })
                        .is_ok()
                    } else {
                        false
                    };
                    let _ = event_tx.send(AgentEvent::PermissionPolicyChanged { policy, saved });
                });
            }
            Action::StartPlan(description) => {
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                tokio::spawn(async move {
                    let prompt = crate::prompts::build_planning_prompt(&description);

                    let mut guard = state.lock().await;
                    guard.agent.set_plan_gated(true);
                    let _ = event_tx.send(AgentEvent::PlanGateChanged { gated: true });
                    let _ = event_tx.send(AgentEvent::PhaseChanged(AgentPhase::Planning));
                    guard
                        .agent
                        .run_turn(prompt, event_tx.clone(), permission_tx, question_tx, cancel)
                        .await;

                    persist_turn(&mut guard);
                });
            }
            Action::ApprovePlan => {
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard.agent.set_plan_gated(false);
                    let _ = event_tx.send(AgentEvent::PlanGateChanged { gated: false });
                    let _ = event_tx.send(AgentEvent::PhaseChanged(AgentPhase::Building));
                    let plan_text = last_assistant_plan_text(guard.agent.history());
                    let prompt = build_approved_plan_prompt(&plan_text);
                    guard
                        .agent
                        .run_turn(prompt, event_tx.clone(), permission_tx, question_tx, cancel)
                        .await;
                    persist_turn(&mut guard);
                });
            }
            Action::RejectPlan => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard.agent.set_plan_gated(false);
                    let _ = event_tx.send(AgentEvent::PlanGateChanged { gated: false });
                });
            }
            Action::SetGoal(goal) => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    guard.agent.set_goal(goal.clone());
                    if let Some(p) = guard.persistence.as_mut() {
                        p.save_goal(goal.as_deref());
                    }
                    let _ = event_tx.send(AgentEvent::GoalChanged(goal));
                });
            }
            Action::Remember(note) => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let guard = state.lock().await;
                    // The chain root, not the cwd: a note dropped in whatever
                    // subdirectory you happened to be in would only apply
                    // while you stayed there, which isn't what "remember this"
                    // means.
                    let path = smith_config::memory::memory_path(&guard.memory.scope().root);
                    match smith_config::memory::remember(&path, &note) {
                        Ok(()) => {
                            // The next request re-reads memory, so the note is
                            // in effect immediately — but only if the cache is
                            // told its fingerprint is stale.
                            guard.memory.invalidate();
                        }
                        Err(e) => {
                            let _ = event_tx.send(AgentEvent::Error(format!(
                                "could not write {}: {e}",
                                path.display()
                            )));
                        }
                    }
                });
            }
            Action::Compact => {
                // Shares `current_cancel` with turns: compaction spends a
                // provider request, so Esc has to be able to stop it.
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let mut guard = state.lock().await;
                    match guard.agent.compact(&event_tx, &cancel).await {
                        Ok(_) => {
                            // Compaction rewrites history rather than
                            // appending to it, so the messages already in the
                            // database stay as they are — the session store
                            // keeps the full transcript even though the model
                            // no longer sees it, which is what makes the
                            // operation non-destructive. What does need saving
                            // is the summarising request's cost.
                            //
                            // Success is reported by the `ContextUsage` event
                            // `compact` emits, not by a message: the only
                            // notice channel here is `Error`, and headless
                            // treats an `Error` mid-turn as a failed run.
                            persist_turn(&mut guard);
                        }
                        Err(err) => {
                            let _ = event_tx
                                .send(AgentEvent::Error(format!("compaction failed: {err}")));
                        }
                    }
                });
            }
            Action::Rewind { turn, apply, force } => {
                let state = state.clone();
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    // Takes the same lock a turn holds, so a rewind can never
                    // interleave with the writes it is undoing.
                    let mut guard = state.lock().await;
                    let session = guard.agent.tool_ctx().session_id.clone();
                    let checkpoints = guard.checkpoints.clone();

                    let report = if apply {
                        checkpoints.apply(&session, turn, force).await
                    } else {
                        checkpoints.plan(&session, turn).await
                    };

                    // The model believes it wrote those files. Left uncorrected
                    // it will keep building on edits that no longer exist —
                    // reading a file it "just changed" and finding the old
                    // contents is exactly the kind of confusion that ends in it
                    // rewriting them.
                    if report.status == smith_core::RewindStatus::Applied && report.touches_files()
                    {
                        let files: Vec<&str> = report
                            .restore
                            .iter()
                            .chain(report.delete.iter())
                            .map(String::as_str)
                            .collect();
                        guard.agent.note_to_model(format!(
                            "[smith] The user ran /rewind on turn {}. These files were put back to \
                             the state they were in before that turn, undoing your edits to them: \
                             {}. Do not assume any of your earlier changes to those files are \
                             still present — re-read anything you need.",
                            report.turn.unwrap_or_default(),
                            files.join(", ")
                        ));
                    }

                    let _ = event_tx.send(AgentEvent::Rewind(report));
                });
            }
            Action::StartLoop {
                prompt,
                max_iterations,
            } => {
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                tokio::spawn(async move {
                    run_loop(
                        state,
                        event_tx,
                        permission_tx,
                        question_tx,
                        cancel,
                        prompt,
                        max_iterations,
                    )
                    .await;
                });
            }
            Action::Mcp(command) => {
                let mcp = mcp.clone();
                let state = state.clone();
                let event_tx = event_tx.clone();
                let permission_tx = permission_tx.clone();
                let question_tx = question_tx.clone();
                let cancel = CancellationToken::new();
                current_cancel = Some(cancel.clone());
                // Spawned like every other arm: `/mcp prompt` runs a whole
                // turn, and a turn must never block this loop from handling
                // the Esc that would cancel it.
                tokio::spawn(async move {
                    match command {
                        smith_core::McpCommand::Status => {
                            let _ = event_tx.send(AgentEvent::McpStatus(mcp.status()));
                        }
                        smith_core::McpCommand::Prompt {
                            server,
                            name,
                            arguments,
                        } => {
                            match mcp
                                .render_prompt(server.as_deref(), &name, &arguments)
                                .await
                            {
                                Ok(text) => {
                                    let mut guard = state.lock().await;
                                    guard
                                        .agent
                                        .run_turn(
                                            text,
                                            event_tx,
                                            permission_tx,
                                            question_tx,
                                            cancel,
                                        )
                                        .await;
                                    persist_turn(&mut guard);
                                }
                                // An `Error` is what resets the frontend's
                                // "waiting on assistant" state, so a prompt
                                // that cannot be fetched must report as one.
                                Err(e) => {
                                    let _ = event_tx.send(AgentEvent::Error(e));
                                }
                            }
                        }
                    }
                });
            }
            Action::Quit => break,
            Action::PermissionResponse { .. } | Action::QuestionResponse { .. } => {
                // Resolved directly by smith-tui via the oneshot channels.
            }
        }
    }
}

/// Drops one copy of `text` from the pending interjections, if it is there.
///
/// A message typed mid-turn is *both* sent as an interjection and held by the
/// frontend, because the two can miss each other: `run_turn` folds
/// interjections in at a round boundary, and a turn already on its last round
/// never reaches another one. The frontend keeps its copy until
/// `AgentEvent::UserInterjected` says the agent took it, and resends whatever
/// is left as its own turn — which is right, and was the only half that
/// worked. The agent was still holding the original, so the next turn's first
/// round pushed the same sentence a second time: the model answered it twice
/// and the user saw two identical bubbles.
///
/// One copy, not `clear()`: a `/loop` iteration can legitimately be carrying
/// an interjection meant for the iteration still to come, and emptying the
/// queue would swallow it.
fn drop_pending_duplicate(
    interjections: &std::sync::Mutex<std::collections::VecDeque<String>>,
    text: &str,
) {
    // A poisoned lock costs a duplicate message, never the turn.
    let Ok(mut queue) = interjections.lock() else {
        return;
    };
    if let Some(at) = queue.iter().position(|pending| pending == text) {
        queue.remove(at);
    }
}

#[cfg(test)]
mod tests {
    use super::drop_pending_duplicate;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn queue(items: &[&str]) -> Mutex<VecDeque<String>> {
        Mutex::new(items.iter().map(|s| (*s).to_string()).collect())
    }

    fn contents(queue: &Mutex<VecDeque<String>>) -> Vec<String> {
        queue.lock().unwrap().iter().cloned().collect()
    }

    #[test]
    fn a_resent_message_is_no_longer_pending_as_an_interjection() {
        let pending = queue(&["also handle the errors"]);
        drop_pending_duplicate(&pending, "also handle the errors");
        assert!(contents(&pending).is_empty());
    }

    #[test]
    fn only_one_copy_goes_even_when_the_same_thing_was_said_twice() {
        // Two identical interjections are two messages the user typed, and
        // resending one of them accounts for exactly one of them.
        let pending = queue(&["again", "again"]);
        drop_pending_duplicate(&pending, "again");
        assert_eq!(contents(&pending), vec!["again"]);
    }

    #[test]
    fn an_interjection_meant_for_the_next_loop_iteration_survives() {
        let pending = queue(&["check the tests too"]);
        drop_pending_duplicate(&pending, "a different message entirely");
        assert_eq!(contents(&pending), vec!["check the tests too"]);
    }
}
