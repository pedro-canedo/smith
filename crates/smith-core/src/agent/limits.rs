//! The caps that bound one call to `run_turn`.

use std::time::Duration;

use crate::event::TurnLimitKind;

/// How long one call to `run_turn` may keep going on its own.
///
/// Without these the loop is unbounded in every direction: a model that
/// oscillates between two tool calls, or keeps "just checking one more file",
/// spends the user's money until they notice and kill the process. Each cap
/// covers a different runaway shape, so they are not redundant:
/// `max_turns` bounds *requests*, `max_tool_calls_per_turn` bounds *side
/// effects* (a single round can carry a dozen calls), and `max_wall_clock`
/// bounds the one thing neither of those sees — tools that are individually
/// slow rather than numerous.
///
/// All three are checked only at a round boundary, never mid-round. That is
/// what keeps the `tool_use`/`tool_result` invariant intact and means a cap
/// can never kill a command that is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnLimits {
    /// Tool-call rounds — provider request plus its tool executions — in one
    /// turn.
    pub max_turns: u32,
    /// Individual tool calls in one turn, summed across rounds.
    pub max_tool_calls_per_turn: u32,
    /// Elapsed time since the turn started.
    pub max_wall_clock: Duration,
}

impl Default for TurnLimits {
    /// - **`max_turns` 50**: real coding work routinely takes twenty or thirty
    ///   rounds, so anything much lower would cut off legitimate turns; fifty
    ///   still caps a two-call oscillation at fifty wasted requests instead of
    ///   an unbounded number. This is the cap that actually catches a loop,
    ///   because a loop is fast — it will never reach the wall clock.
    /// - **`max_tool_calls_per_turn` 100**: rounds and calls diverge as soon
    ///   as a model emits calls in parallel, so bounding rounds alone doesn't
    ///   bound side effects. A hundred tool calls is already far more than any
    ///   single user instruction plausibly needs, and being slightly too low
    ///   costs one "continue" — the turn stops cleanly with everything intact.
    /// - **`max_wall_clock` 10 minutes**: the legitimate consumer of wall
    ///   clock is a long `run_bash` (a full workspace build and test suite is
    ///   minutes), and since the check happens between rounds it never
    ///   interrupts one. Ten minutes is roughly where a user is still watching
    ///   an interactive turn; past it, the agent has quietly become a batch
    ///   job nobody asked for.
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_tool_calls_per_turn: 100,
            max_wall_clock: Duration::from_secs(600),
        }
    }
}

impl TurnLimitKind {
    /// One line naming the cap and the value it hit — shown to the user and
    /// folded into what the model is told, so the two never disagree.
    pub(super) fn describe(self, limits: &TurnLimits) -> String {
        match self {
            TurnLimitKind::Rounds => format!(
                "reached the limit of {} tool-call rounds in one turn",
                limits.max_turns
            ),
            TurnLimitKind::ToolCalls => format!(
                "reached the limit of {} tool calls in one turn",
                limits.max_tool_calls_per_turn
            ),
            TurnLimitKind::WallClock => format!(
                "reached the {}s time limit for one turn",
                limits.max_wall_clock.as_secs()
            ),
        }
    }
}

/// What the model is told about a capped turn.
///
/// It goes into history as a text block on the *same* user message that
/// carries the round's tool results, rather than a message of its own: two
/// consecutive user messages is a shape some providers reject and others
/// silently merge, and there is nothing to gain by risking it. Not a system
/// prompt addition either — the system prompt is a cached prefix and a
/// standing instruction, while this is a one-off fact about one turn.
///
/// The model does not read it now (the turn ends without another request —
/// spending a request to narrate the moment we decided it was overspending
/// would be self-defeating). It reads it on the *next* turn, which is exactly
/// when it matters: the user types "continue" and the model needs to know why
/// it stopped rather than assuming the task was finished.
pub(super) fn limit_note(kind: TurnLimitKind, limits: &TurnLimits) -> String {
    format!(
        "[smith] This turn was stopped automatically: it {}. \
         Everything already done is intact and nothing else was executed — \
         the task is not necessarily finished. If the user asks you to \
         continue, resume from here.",
        kind.describe(limits)
    )
}
