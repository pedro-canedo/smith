//! Token, context-window and cost accounting for a turn.

use tokio::sync::mpsc;

use crate::context::{estimate_messages_tokens, estimate_tokens, ContextUsage};
use crate::event::AgentEvent;
use crate::message::Usage;

use super::compaction::TurnAccounting;
use super::Agent;

impl Agent {
    /// Usage and cost for the most recent `run_turn` — what the caller
    /// persists as one `turns` row.
    pub fn last_turn(&self) -> Option<&TurnAccounting> {
        self.last_turn.as_ref()
    }

    pub fn session_usage(&self) -> Usage {
        self.session_usage
    }

    /// Accumulated cost for the session, in USD. Only ever the sum of costs
    /// computed at the time of each turn — never a recomputation from a price
    /// table that may have moved since.
    pub fn session_cost_usd(&self) -> f64 {
        self.session_cost_usd
    }

    /// Turns billed against a model this build has no price for.
    pub fn unpriced_turns(&self) -> u32 {
        self.unpriced_turns
    }

    /// Restores the running totals for a resumed session, from the numbers the
    /// session store recorded when those turns actually ran. Also used by a
    /// `/model` switch, which rebuilds the whole `Agent`.
    pub fn seed_session_totals(&mut self, usage: Usage, cost_usd: f64, unpriced_turns: u32) {
        self.session_usage = usage;
        self.session_cost_usd = cost_usd;
        self.unpriced_turns = unpriced_turns;
    }

    /// How full the context window is for the *next* request.
    ///
    /// Exact where it can be and estimated only where it must be: the last
    /// response's `prompt_tokens` (input + cache read + cache write — Anthropic
    /// reports those separately, and adding only `input_tokens` would miss the
    /// entire cached prefix) plus its `output_tokens`, which is the assistant
    /// message now sitting in history, plus a `chars/4` estimate of everything
    /// appended since. Before the first response there is nothing but estimate,
    /// and the system prompt and tool schemas have to be estimated too — they
    /// are a fixed several-thousand-token floor that a naive count of
    /// `messages` alone would miss entirely.
    pub fn context_usage(&self) -> ContextUsage {
        let window = self.provider.capabilities(&self.model).context_window;

        let (counted, exact) = match self.last_usage {
            Some(usage) => (
                usage.prompt_tokens().saturating_add(usage.output_tokens),
                true,
            ),
            None => (self.estimate_request_overhead(), false),
        };

        let pending = self
            .messages
            .get(self.counted_messages..)
            .map(estimate_messages_tokens)
            .unwrap_or(0);

        ContextUsage {
            used: counted.saturating_add(pending),
            window,
            // Exact only at the instant a response lands with nothing appended
            // after it; one tool result and it is an estimate again.
            estimated: !exact || pending > 0,
        }
    }

    /// The part of a request that is not conversation: system prompt and tool
    /// definitions. Only used before the first response, since after that the
    /// provider's `input_tokens` already includes it.
    fn estimate_request_overhead(&self) -> u32 {
        let system = self
            .effective_system()
            .map(|s| estimate_tokens(&s))
            .unwrap_or(0);
        let tools = self
            .tools
            .tool_defs()
            .iter()
            .map(|d| {
                estimate_tokens(&d.name)
                    .saturating_add(estimate_tokens(&d.description))
                    .saturating_add(estimate_tokens(&d.input_schema.to_string()))
            })
            .fold(0u32, u32::saturating_add);
        system.saturating_add(tools)
    }

    /// Whether history is due for compaction.
    pub fn should_compact(&self) -> bool {
        self.compaction.enabled && self.context_usage().ratio() >= self.compaction.threshold
    }

    pub(super) fn emit_context(&self, events: &mpsc::UnboundedSender<AgentEvent>) {
        let context = self.context_usage();
        let _ = events.send(AgentEvent::ContextUsage {
            used: context.used,
            window: context.window,
            estimated: context.estimated,
        });
    }

    /// Folds one provider response's usage into the turn, session, and context
    /// bookkeeping. Called once per round, after the assistant message has
    /// been pushed, so `counted_messages` lines up with what the provider was
    /// actually charging for.
    pub(super) fn note_usage(&mut self, usage: Usage) {
        self.last_usage = Some(usage);
        self.counted_messages = self.messages.len();
        self.session_usage.add(&usage);

        let provider = self.provider.id().to_string();
        // Through the provider, not `self.model` directly: a fallback chain
        // that advanced mid-session answers with the entry now serving, and
        // pricing/persisting the turn under the original model's name would
        // be a silent accounting error.
        let model = self.provider.effective_model(&self.model);
        let cost = crate::pricing::cost_usd(&provider, &model, &usage);
        match cost {
            Some(cost) => self.session_cost_usd += cost,
            None => self.unpriced_turns = self.unpriced_turns.saturating_add(1),
        }

        // One `TurnAccounting` spans every round of a turn: the model cannot
        // change mid-turn (a fallback advancement lands between requests, and
        // the round that failed never produced usage to note), so summing
        // rounds loses nothing, and it keeps the persisted `turns` table one
        // row per user-visible turn rather than one per HTTP request.
        let turn = self.last_turn.get_or_insert_with(|| TurnAccounting {
            provider: provider.clone(),
            model,
            usage: Usage::default(),
            cost_usd: None,
        });
        turn.usage.add(&usage);
        if let Some(cost) = cost {
            turn.cost_usd = Some(turn.cost_usd.unwrap_or(0.0) + cost);
        }
    }

    /// Bills a request the agent made on its own behalf (the compaction
    /// summary, a subagent's whole conversation) to the session and the
    /// current turn — but *not* to the context tracker. It was a different
    /// prompt entirely, so letting it overwrite `last_usage` would make the
    /// gauge describe a conversation that isn't the one in `self.messages`.
    ///
    /// `model` is passed rather than read from `self` because a subagent may
    /// be configured to run on a different one, and pricing that request at
    /// the parent's rate would be a silent error in the direction nobody
    /// checks.
    pub(super) fn note_side_request_usage(&mut self, usage: Usage, model: &str) {
        self.session_usage.add(&usage);
        let provider = self.provider.id().to_string();
        let cost = crate::pricing::cost_usd(&provider, model, &usage);
        if let Some(cost) = cost {
            self.session_cost_usd += cost;
        }
        let turn = self.last_turn.get_or_insert_with(|| TurnAccounting {
            provider,
            model: self.model.clone(),
            usage: Usage::default(),
            cost_usd: None,
        });
        turn.usage.add(&usage);
        if let Some(cost) = cost {
            turn.cost_usd = Some(turn.cost_usd.unwrap_or(0.0) + cost);
        }
    }
}
