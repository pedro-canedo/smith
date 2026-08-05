//! When to re-send a provider request that failed, and how long to wait first.
//!
//! This lives above the provider adapters rather than inside them on purpose:
//! one policy then covers every adapter (and every future one), and it can be
//! driven from the agent loop, where the `CancellationToken` and the event
//! channel already are — a backoff that can't be interrupted or seen is worse
//! than no backoff at all.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::provider::ProviderError;

/// The backoff schedule for one provider request.
///
/// Every number here is tuned for an *interactive* turn — a user is watching a
/// spinner — not for a batch job that can afford to sit out a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts, including the first. `1` disables retrying.
    pub max_attempts: u32,
    pub base_delay: Duration,
    /// Growth factor per attempt. An integer because it is only ever doubling
    /// in practice, and a float here would cost the type its `Eq`.
    pub multiplier: u32,
    /// Ceiling on a single computed delay (before jitter).
    pub max_delay: Duration,
    /// Largest server-supplied `Retry-After` this layer will actually sit out.
    /// Beyond it the turn fails immediately instead — see [`RetryPolicy::delay_for`].
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    /// Schedule: 0.5s, 1s, 2s (each jittered), then give up — four attempts,
    /// ~3.5s of added latency in the worst case.
    ///
    /// - **`base_delay` 500ms**: below roughly half a second a retry is likely
    ///   to hit the very condition that just failed, and no provider's rate
    ///   limit window is shorter than that. Above ~1s the pause reads as "the
    ///   app is broken" rather than "the network hiccuped".
    /// - **`multiplier` 2**: plain doubling. It reaches a meaningful wait in
    ///   two steps, which matters when there are only three of them.
    /// - **`max_delay` 8s**: an 8-second stall is already at the edge of what
    ///   someone staring at a terminal will tolerate before reaching for
    ///   Ctrl-C. The schedule never actually reaches it at four attempts; the
    ///   cap exists so a caller that raises `max_attempts` doesn't silently
    ///   inherit minute-long sleeps.
    /// - **`max_attempts` 4**: the overwhelming majority of transient failures
    ///   clear on the second attempt. A failure that survives four is not
    ///   transient, and grinding on it converts one honest error message into
    ///   a long unexplained hang. Failing fast is cheap here: history is
    ///   intact, so the user just sends the message again.
    /// - **`max_retry_after` 30s**: covers the windows providers actually ask
    ///   for on a 429, without letting a server park an interactive session.
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(500),
            multiplier: 2,
            max_delay: Duration::from_secs(8),
            max_retry_after: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Fail on the first error. For callers that want the old behaviour back
    /// (and for tests that assert a request was *not* replayed).
    pub fn no_retries() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// How long to wait before re-sending, given the error from a 1-based
    /// `attempt`. `None` means stop and surface the error.
    ///
    /// Three ways to get `None`, in priority order:
    ///
    /// 1. The error isn't retryable — [`ProviderError::retryable`] is trusted
    ///    outright. Replaying a 400 or a bad key can never succeed; it only
    ///    burns quota and turns one clear failure into several slow ones.
    /// 2. The attempt budget is spent.
    /// 3. The server asked for longer than [`Self::max_retry_after`]. A
    ///    provider saying "come back in 300s" is not describing a blip, and
    ///    sleeping on it would be indistinguishable from a hang while holding
    ///    the agent lock. The number is surfaced in the error message instead,
    ///    so the user can decide: wait, switch model, or switch provider.
    ///
    /// When the server *did* send a `Retry-After` inside the cap it is used
    /// verbatim — no jitter, no formula. It is the only party that knows when
    /// the window actually reopens; retrying earlier just earns another 429,
    /// and retrying later wastes the user's time.
    pub fn delay_for(&self, error: &ProviderError, attempt: u32) -> Option<Duration> {
        if !error.retryable() || attempt >= self.max_attempts {
            return None;
        }
        match error.retry_after() {
            Some(server) if server > self.max_retry_after => None,
            Some(server) => Some(server),
            None => Some(equal_jitter(self.backoff(attempt))),
        }
    }

    /// The un-jittered delay after a 1-based `attempt`.
    fn backoff(&self, attempt: u32) -> Duration {
        let factor = self
            .multiplier
            .checked_pow(attempt.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// Spreads a delay over `[d/2, d]`.
///
/// Equal jitter rather than full jitter (`[0, d]`): full jitter can draw a
/// near-zero wait, which re-sends into the same rate-limit window and wastes
/// an attempt. Keeping the lower half guarantees the backoff still backs off,
/// while the random upper half is what stops many clients — or one client
/// retrying several requests — from synchronising onto the same instant.
fn equal_jitter(delay: Duration) -> Duration {
    let half = delay / 2;
    half + Duration::from_nanos(pseudo_random(half.as_nanos() as u64 + 1))
}

/// A value in `[0, bound)`, seeded from the wall clock.
///
/// Deliberately not the `rand` crate: jitter needs decorrelation, not
/// unpredictability, and a whole dependency (plus its transitive tree) in the
/// crate that is supposed to know nothing about the outside world is a poor
/// trade for one modulo. Calls are seconds apart, so re-seeding from the
/// nanosecond clock each time is plenty of entropy for the job.
fn pseudo_random(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x % bound
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, retry_after: Option<Duration>) -> ProviderError {
        ProviderError::Api {
            status,
            message: "boom".into(),
            retry_after,
        }
    }

    #[test]
    fn backoff_doubles_from_the_base_delay_and_stops_at_the_cap() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.backoff(1), Duration::from_millis(500));
        assert_eq!(policy.backoff(2), Duration::from_secs(1));
        assert_eq!(policy.backoff(3), Duration::from_secs(2));
        // Far past the attempt budget: the cap holds rather than overflowing.
        assert_eq!(policy.backoff(50), policy.max_delay);
    }

    #[test]
    fn jitter_keeps_the_delay_in_the_upper_half_of_the_window() {
        let base = Duration::from_secs(4);
        for _ in 0..100 {
            let jittered = equal_jitter(base);
            assert!(
                jittered >= base / 2 && jittered <= base,
                "{jittered:?} outside [2s, 4s]"
            );
        }
    }

    #[test]
    fn a_retryable_error_gets_a_delay_within_the_jitter_window() {
        let policy = RetryPolicy::default();
        let delay = policy.delay_for(&api(429, None), 1).unwrap();
        assert!(delay >= Duration::from_millis(250) && delay <= Duration::from_millis(500));
    }

    #[test]
    fn a_non_retryable_error_is_never_delayed_however_early_the_attempt() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.delay_for(&api(400, None), 1), None);
        assert_eq!(policy.delay_for(&ProviderError::Cancelled, 1), None);
        assert_eq!(
            policy.delay_for(&ProviderError::MissingApiKey("anthropic".into()), 1),
            None
        );
    }

    #[test]
    fn the_last_attempt_gets_no_further_delay() {
        let policy = RetryPolicy::default();
        assert!(policy.delay_for(&api(503, None), 3).is_some());
        assert_eq!(policy.delay_for(&api(503, None), 4), None);
        assert_eq!(
            RetryPolicy::no_retries().delay_for(&api(503, None), 1),
            None
        );
    }

    /// The server knows when its window reopens and the formula does not, so
    /// `Retry-After` replaces the computed delay outright — jitter included.
    #[test]
    fn retry_after_is_used_verbatim_instead_of_the_computed_backoff() {
        let policy = RetryPolicy::default();
        let server = Duration::from_secs(7);
        assert_eq!(policy.delay_for(&api(429, Some(server)), 1), Some(server));
        // Also when it is *shorter* than what the formula would have picked.
        let policy = RetryPolicy {
            base_delay: Duration::from_secs(5),
            ..RetryPolicy::default()
        };
        assert_eq!(
            policy.delay_for(&api(429, Some(Duration::from_millis(100))), 1),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn a_retry_after_beyond_the_cap_gives_up_rather_than_parking_the_session() {
        let policy = RetryPolicy::default();
        assert_eq!(
            policy.delay_for(&api(429, Some(Duration::from_secs(300))), 1),
            None
        );
        assert!(policy
            .delay_for(&api(429, Some(policy.max_retry_after)), 1)
            .is_some());
    }

    #[test]
    fn default_schedule_stays_inside_an_interactive_budget() {
        let policy = RetryPolicy::default();
        let total: Duration = (1..policy.max_attempts).map(|a| policy.backoff(a)).sum();
        assert!(
            total <= Duration::from_secs(5),
            "an interactive turn must not spend {total:?} sleeping"
        );
    }
}
