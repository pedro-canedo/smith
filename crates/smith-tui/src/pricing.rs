use smith_core::Usage;

/// Rough USD-per-million-token pricing for models this project defaults to
/// or lists in `smith setup`. Deliberately approximate — always labeled
/// "(est.)" in the UI — since published pricing can change and isn't worth
/// re-verifying on every release. Returns `None` for unlisted models rather
/// than guessing, so we never show a fabricated number for a model we don't
/// actually know the pricing for.
fn price_per_million_usd(provider: &str, model: &str) -> Option<(f64, f64)> {
    match provider {
        "anthropic" => {
            if model.contains("opus") {
                Some((15.0, 75.0))
            } else if model.contains("sonnet") {
                Some((3.0, 15.0))
            } else if model.contains("haiku") {
                Some((0.80, 4.0))
            } else {
                None
            }
        }
        "openai" => match model {
            "gpt-4.1" => Some((2.0, 8.0)),
            "gpt-4.1-mini" => Some((0.40, 1.60)),
            "gpt-4o" => Some((2.50, 10.0)),
            "o3" => Some((2.0, 8.0)),
            _ => None,
        },
        _ => None,
    }
}

/// Estimated cost in USD for the given cumulative usage, or `None` if we
/// don't have a price for this provider/model.
pub fn estimate_cost_usd(provider: &str, model: &str, usage: &Usage) -> Option<f64> {
    let (input_per_m, output_per_m) = price_per_million_usd(provider, model)?;
    let input_cost = usage.input_tokens as f64 / 1_000_000.0 * input_per_m;
    let output_cost = usage.output_tokens as f64 / 1_000_000.0 * output_per_m;
    Some(input_cost + output_cost)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_known_model_cost() {
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
        };
        let cost = estimate_cost_usd("anthropic", "claude-sonnet-5", &usage).unwrap();
        assert!((cost - 18.0).abs() < 0.001);
    }

    #[test]
    fn unknown_model_has_no_estimate() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 100,
        };
        assert!(estimate_cost_usd("anthropic", "claude-does-not-exist", &usage).is_none());
        assert!(estimate_cost_usd("ollama", "qwen3.5", &usage).is_none());
    }
}
