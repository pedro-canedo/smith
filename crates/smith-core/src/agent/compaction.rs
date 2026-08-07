//! Compacting history when it outgrows the context window.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::context::{carry_over, compaction_split, render_transcript, COMPACT_THRESHOLD};
use crate::event::AgentEvent;
use crate::message::{CompletionRequest, ContentBlock, Message, StopReason, Usage};

use super::stream::consume_stream;
use super::Agent;

/// When and how aggressively history gets compacted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionConfig {
    /// Off entirely. Only tests and `--headless` single-shot runs, where the
    /// turn is short by construction, have a reason to disable it.
    pub enabled: bool,
    /// Fraction of the context window at which auto-compaction fires.
    pub threshold: f32,
    /// How many trailing messages survive untouched. Counted in *messages*,
    /// not exchanges: a tool-heavy round is one assistant message plus one
    /// results message, so eight is roughly the last three or four rounds.
    ///
    /// The real cut point is snapped to a clean user boundary (see
    /// `context::compaction_split`), so this is a target, not a guarantee.
    pub keep_recent: usize,
    /// Cap on the summary the model is asked to write. It has to be small —
    /// a summary that fills the space it just freed is not a compaction.
    pub summary_max_tokens: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: COMPACT_THRESHOLD,
            keep_recent: 8,
            summary_max_tokens: 1024,
        }
    }
}

/// What one call to `run_turn` consumed, and what it cost.
///
/// `cost_usd` is computed **here, when the turn runs**, from the price table
/// in force at that moment — and it is what gets persisted. Storing only the
/// tokens and recomputing later gives a different answer the day a model is
/// repriced or retired, which is exactly the drift `--resume` must not have.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnAccounting {
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    /// `None` when this build has no price for the provider/model — an honest
    /// gap, never a zero pretending to be free.
    pub cost_usd: Option<f64>,
}

/// The result of a successful compaction, for whoever wants to report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionOutcome {
    pub messages_before: usize,
    pub messages_after: usize,
    /// Estimated context tokens before and after, so a caller can say how much
    /// room it actually bought.
    pub tokens_before: u32,
    pub tokens_after: u32,
    /// Whether the model wrote a prose summary, or the compaction is carrying
    /// structure only.
    pub summarised: bool,
}

/// What the assistant "says" in the synthetic acknowledgement that follows the
/// compaction message.
///
/// It exists purely to keep roles alternating: the compaction message is a
/// user message, and the kept tail also begins with a user message (that is
/// what a clean split boundary *is*). Two user messages in a row is a shape
/// some providers reject outright and others silently merge, and there is
/// nothing to gain by finding out which.
const COMPACTION_ACK: &str =
    "Understood. I have the summary and carried-over state above, and I will continue from the \
     messages that follow.";

const SUMMARY_SYSTEM_PROMPT: &str =
    "You are compacting the transcript of a coding session so that work can continue in a smaller \
     context window. Write a dense factual summary in under 400 words. Cover, in this order: what \
     the user asked for; decisions that were made and the reasoning behind them; what was actually \
     changed and where; anything that was tried and failed, and why; and what remains unresolved. \
     Prefer concrete names — files, functions, commands, error messages — over description. Do not \
     speculate, do not offer next steps, and do not address the user. Output only the summary.";

impl Agent {
    /// Replaces the older part of history with a summary plus a structural
    /// carry-over, freeing context without losing what the session established.
    ///
    /// **Atomic.** The new history is assembled in a local vector and only
    /// assigned to `self.messages` on the last line. Every failure path —
    /// nothing safe to cut, provider error, cancellation — returns before that
    /// point with history byte-for-byte unchanged. The alternative, falling
    /// back to a mechanical drop when the summariser fails, would quietly
    /// destroy the reasoning behind everything already done at exactly the
    /// moment the provider is flaky; the turn continuing at full context and
    /// the trigger firing again next round is strictly better, because the
    /// retry layer will very likely have succeeded by then.
    ///
    /// **It spends one provider request, on the session's own model.** The
    /// structural half (todos, goal, files) is mechanical and free, but
    /// "decisions taken and why" exists only as prose in the transcript and no
    /// amount of scanning recovers it. Using a cheaper model was considered
    /// and rejected: `capabilities()` reports windows and features, not price,
    /// so it cannot actually identify the cheap one — that would take a second
    /// hardcoded model table, drifting exactly the way the pricing table
    /// drifts. And a cheaper model is not necessarily *available*: an Ollama
    /// user has one model pulled, and an API key does not imply access to
    /// every model behind it. The session's own model is the one we know
    /// works. The cost is bounded instead: the transcript is excerpted before
    /// it is sent (see `context::render_transcript`) and the reply is capped
    /// at `summary_max_tokens`.
    pub async fn compact(
        &mut self,
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<CompactionOutcome, String> {
        let split = compaction_split(&self.messages, self.compaction.keep_recent)
            .ok_or("nothing safe to compact — history has no clean split point")?;

        let messages_before = self.messages.len();
        let tokens_before = self.context_usage().used;
        let dropped = &self.messages[..split];

        // Pure, and computed before anything can fail: whatever the provider
        // does next, these facts are already in hand.
        let carried = carry_over(dropped, self.goal.as_deref(), &self.tasks);
        // Todos recovered from history become the live list, if there wasn't
        // one. Without this, a *second* compaction would look for a
        // `write_tasks` call that is no longer anywhere in history — the first
        // compaction replaced it with prose — and the todos this one just
        // rescued would quietly not survive the next round.
        let recovered_tasks = (self.tasks.is_empty() && !carried.pending_tasks.is_empty())
            .then(|| carried.pending_tasks.clone());

        let (summary, summary_usage) = self.summarise(dropped, events, cancel).await?;
        // The user paid for that request whether or not the compaction is a
        // success, so it lands in the session totals either way.
        self.note_side_request_usage(summary_usage, &self.model.clone());

        let mut compacted = Vec::with_capacity(self.messages.len() - split + 2);
        compacted.push(Message::user_text(carried.render(Some(&summary))));
        compacted.push(Message::assistant(vec![ContentBlock::Text {
            text: COMPACTION_ACK.to_string(),
        }]));
        compacted.extend_from_slice(&self.messages[split..]);

        // The only mutations in this function, and they are unreachable from
        // every failure path above.
        self.messages = compacted;
        if let Some(tasks) = recovered_tasks {
            self.tasks = tasks.clone();
            let _ = events.send(AgentEvent::TasksUpdated(tasks));
        }
        // The provider's last token count described a prompt that no longer
        // exists, so the gauge falls back to a full estimate until the next
        // response corrects it.
        self.last_usage = None;
        self.counted_messages = 0;

        let outcome = CompactionOutcome {
            messages_before,
            messages_after: self.messages.len(),
            tokens_before,
            tokens_after: self.context_usage().used,
            summarised: true,
        };
        self.emit_context(events);
        Ok(outcome)
    }

    /// Asks the model to summarise `dropped`, as a single plain-text request.
    ///
    /// The transcript goes in as the *content of one user message* rather than
    /// as replayed conversation history. That makes the `tool_use` /
    /// `tool_result` pairing rules irrelevant — text cannot be malformed the
    /// way a message array can — and lets each tool result be excerpted, which
    /// is where nearly all the savings are.
    async fn summarise(
        &self,
        dropped: &[Message],
        events: &mpsc::UnboundedSender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Result<(String, Usage), String> {
        let window = self.provider.capabilities(&self.model).context_window;
        // Half the window, in characters, at the un-margined 4:1 ratio. The
        // summarisation request must comfortably fit alongside its own reply.
        let budget_chars = (window as usize / 2).saturating_mul(4);
        let transcript = render_transcript(dropped, budget_chars);

        let request = CompletionRequest {
            model: self.model.clone(),
            system: Some(SUMMARY_SYSTEM_PROMPT.to_string()),
            messages: vec![Message::user_text(format!(
                "Summarise this session transcript.\n\n<transcript>\n{transcript}\n</transcript>"
            ))],
            // No tools: this request has one job and must not start doing work
            // of its own halfway through a compaction.
            tools: Vec::new(),
            max_tokens: self.compaction.summary_max_tokens,
            temperature: None,
        };

        let stream = self
            .stream_with_retry(request, events, cancel)
            .await
            .map_err(|e| e.to_string())?;

        // A private channel, because `consume_stream` streams text deltas to
        // whoever it is handed — and the summary must never appear in the
        // chat pane as something the assistant said to the user. Token usage
        // is the one thing forwarded on: the user is paying for this request,
        // so it belongs in their totals.
        let (quiet_tx, mut quiet_rx) = mpsc::unbounded_channel();
        let result = consume_stream(stream, &quiet_tx, cancel.clone()).await;
        drop(quiet_tx);
        while let Ok(event) = quiet_rx.try_recv() {
            if let AgentEvent::TokenUsage(usage) = event {
                let _ = events.send(AgentEvent::TokenUsage(usage));
            }
        }

        let outcome = result?;
        if outcome.stop_reason == StopReason::Cancelled {
            return Err("compaction cancelled".to_string());
        }
        // Reasoning stripped out of a *summary* is deliberately not counted:
        // the counter reports on what the user's own turns produced, and this
        // request is internal plumbing they never see.
        let (text, usage) = (outcome.message.text(), outcome.usage);
        if text.trim().is_empty() {
            return Err("the summarising request returned no text".to_string());
        }
        Ok((text, usage))
    }
}
