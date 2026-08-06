//! Published USD prices per model, and the one function that turns a
//! [`Usage`] into a dollar figure.
//!
//! This table is *volatile by nature*. Providers reprice, rename, and retire
//! models; a price looked up today is not the price that was in force when a
//! turn actually ran. That is precisely why nothing persists tokens and
//! recomputes cost later: [`cost_usd`] is called **once, at the moment of the
//! turn**, and the resulting number is stored alongside the tokens (see
//! `smith_store::TurnRecord`). A resumed session reads the stored dollars back
//! and never consults this table again, so `--resume` reports what the session
//! actually cost rather than what it would cost at today's prices.
//!
//! `None` for an unlisted model, never a guess: showing a fabricated cost is
//! worse than showing none.

use crate::message::Usage;

/// USD per one million tokens, by kind of token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    /// Prompt tokens served from cache. Always cheaper than `input`.
    pub cache_read: f64,
    /// Prompt tokens written into the cache. Dearer than `input` where it is
    /// billed at all, and 0.0 where the provider does not charge for it.
    pub cache_write: f64,
}

impl ModelPrice {
    /// Anthropic prices cache traffic as fixed multiples of the base input
    /// rate (0.1x to read, 1.25x to write with the default 5-minute TTL), so
    /// one input number determines all three.
    const fn anthropic(input: f64, output: f64) -> Self {
        Self {
            input,
            output,
            cache_read: input * 0.1,
            cache_write: input * 1.25,
        }
    }

    /// OpenAI discounts cached prompt tokens and bills nothing for writing
    /// the cache, so `cache_write` is genuinely zero rather than unknown.
    const fn openai(input: f64, output: f64, cache_read: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write: 0.0,
        }
    }
}

/// Matched by **prefix**, longest entry first, so dated snapshots
/// (`claude-sonnet-5-20260115`, `gpt-4.1-2025-04-14`) resolve to their base
/// model instead of falling through to "unknown". Order within each provider
/// is load-bearing: `gpt-4.1-mini` has to be tested before `gpt-4.1`.
const PRICES: &[(&str, &str, ModelPrice)] = &[
    (
        "anthropic",
        "claude-opus-5",
        ModelPrice::anthropic(15.0, 75.0),
    ),
    (
        "anthropic",
        "claude-sonnet-5",
        ModelPrice::anthropic(3.0, 15.0),
    ),
    (
        "anthropic",
        "claude-haiku-4-5",
        ModelPrice::anthropic(0.80, 4.0),
    ),
    (
        "openai",
        "gpt-4.1-mini",
        ModelPrice::openai(0.40, 1.60, 0.10),
    ),
    ("openai", "gpt-4.1", ModelPrice::openai(2.0, 8.0, 0.50)),
    ("openai", "gpt-4o", ModelPrice::openai(2.50, 10.0, 1.25)),
    ("openai", "o3", ModelPrice::openai(2.0, 8.0, 0.50)),
];

/// The price in force for a provider/model pair, or `None` if this build has
/// no entry for it — local models (Ollama) included, which genuinely cost
/// nothing per token but whose electricity we are not going to invent a
/// number for.
pub fn price_for(provider: &str, model: &str) -> Option<ModelPrice> {
    // OpenRouter's `:free` variants are genuinely $0 — a fact, not a gap.
    // Without this rule every free turn would land in `unpriced_turns` and
    // the UI would report "N turns of unknown cost" for a cost that is
    // exactly known. Scoped to the provider id: a `:free` suffix elsewhere
    // is somebody's naming scheme, not OpenRouter's price contract.
    if provider == "openrouter" && model.ends_with(":free") {
        return Some(ModelPrice {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        });
    }
    PRICES
        .iter()
        .find(|(p, m, _)| *p == provider && model.starts_with(m))
        .map(|(_, _, price)| *price)
}

/// Cost in USD of one response's usage **at today's prices**.
///
/// Call this once, when the turn happens, and persist the result. Calling it
/// again later against the same tokens is exactly the bug this module's doc
/// comment describes.
pub fn cost_usd(provider: &str, model: &str, usage: &Usage) -> Option<f64> {
    let price = price_for(provider, model)?;
    Some(
        (usage.input_tokens as f64 * price.input
            + usage.output_tokens as f64 * price.output
            + usage.cache_read as f64 * price.cache_read
            + usage.cache_write as f64 * price.cache_write)
            / 1_000_000.0,
    )
}

#[cfg(test)]
mod tests {
    /// `:free` on OpenRouter is a price contract, not a naming scheme.
    #[test]
    fn openrouter_free_models_price_to_exactly_zero() {
        let usage = one_million_each();
        assert_eq!(
            cost_usd(
                "openrouter",
                "nvidia/nemotron-3-ultra-550b-a55b:free",
                &usage
            ),
            Some(0.0)
        );
        // A paid OpenRouter model has no row: unknown, not zero.
        assert_eq!(
            cost_usd("openrouter", "anthropic/claude-sonnet-5", &usage),
            None
        );
        // The suffix elsewhere is somebody's naming scheme, not a price.
        assert_eq!(cost_usd("ollama", "something:free", &usage), None);
    }

    use super::*;

    fn one_million_each() -> Usage {
        Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read: 0,
            cache_write: 0,
        }
    }

    #[test]
    fn prices_a_known_model() {
        let cost = cost_usd("anthropic", "claude-sonnet-5", &one_million_each()).unwrap();
        assert!((cost - 18.0).abs() < 1e-9, "{cost}");
    }

    /// Dated snapshots are what a real config holds; falling through to
    /// "unknown" for them would leave most real sessions unpriced.
    #[test]
    fn dated_snapshots_resolve_to_their_base_model() {
        assert_eq!(
            price_for("anthropic", "claude-sonnet-5-20260115"),
            price_for("anthropic", "claude-sonnet-5")
        );
        assert_eq!(
            price_for("openai", "gpt-4.1-2025-04-14"),
            price_for("openai", "gpt-4.1")
        );
    }

    /// The ordering hazard in `PRICES`: `gpt-4.1-mini` starts with `gpt-4.1`,
    /// so a table in the wrong order silently bills mini at five times its
    /// real rate.
    #[test]
    fn a_longer_prefix_wins_over_a_shorter_one_that_also_matches() {
        let mini = price_for("openai", "gpt-4.1-mini").unwrap();
        assert!((mini.input - 0.40).abs() < 1e-9, "{mini:?}");
        assert_ne!(mini, price_for("openai", "gpt-4.1").unwrap());
    }

    #[test]
    fn cache_traffic_is_billed_at_its_own_rate() {
        let usage = Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
        };
        // sonnet: 0.1 * 3.00 + 1.25 * 3.00 = 4.05
        let cost = cost_usd("anthropic", "claude-sonnet-5", &usage).unwrap();
        assert!((cost - 4.05).abs() < 1e-9, "{cost}");

        // OpenAI bills nothing to write the cache, so only the read shows up.
        let cost = cost_usd("openai", "gpt-4o", &usage).unwrap();
        assert!((cost - 1.25).abs() < 1e-9, "{cost}");
    }

    #[test]
    fn an_unlisted_model_has_no_price_rather_than_a_guess() {
        let usage = one_million_each();
        assert!(cost_usd("anthropic", "claude-does-not-exist", &usage).is_none());
        assert!(cost_usd("ollama", "qwen3.5", &usage).is_none());
        // A model name from the right provider that is merely a *substring*
        // of a listed one must not match either.
        assert!(cost_usd("anthropic", "claude", &usage).is_none());
    }
}
