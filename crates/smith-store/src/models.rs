//! Small, hand-maintained lists of popular models per provider, shared
//! between the `smith setup` wizard and the runtime `/model` command. Not
//! exhaustive — providers accept any model string, this is just what's
//! offered as quick choices.

const ANTHROPIC_MODELS: &[&str] = &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"];

/// The curated OpenRouter free chain, best first. **Every entry must be
/// `:free` and support tool calling** — an agent without tools is a chatbot.
///
/// The single source: the setup wizard imports this (and intersects it with
/// the live `GET /models` catalogue, keeping this order), and `[openrouter]
/// fallback_models` is seeded from it. Verified against the live catalogue on
/// 2026-08-06; free models rotate, which is exactly why setup re-validates
/// instead of trusting this list blindly.
pub const OPENROUTER_MODELS: &[&str] = &[
    "nvidia/nemotron-3-ultra-550b-a55b:free",
    "nvidia/nemotron-3-super-120b-a12b:free",
    "poolside/laguna-s-2.1:free",
    "google/gemma-4-31b-it:free",
    "cohere/north-mini-code:free",
];

/// 9Router routes by its own model prefixes; `auto` lets the gateway pick,
/// which is the mode its own docs lead with.
const NINEROUTER_MODELS: &[&str] = &["auto"];
const OPENAI_MODELS: &[&str] = &["gpt-4.1", "gpt-4.1-mini", "gpt-4o", "o3"];
const OLLAMA_MODELS: &[&str] = &[
    "llama3.3",
    "llama3.2",
    "qwen2.5",
    "qwen3",
    "mistral",
    "gemma2",
    "phi4",
    "deepseek-r1",
    "codellama",
];

/// Known model names for a provider id, or an empty slice for anything else.
pub fn known_models(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => ANTHROPIC_MODELS,
        "openai" => OPENAI_MODELS,
        "openrouter" => OPENROUTER_MODELS,
        "9router" => NINEROUTER_MODELS,
        "ollama" => OLLAMA_MODELS,
        _ => &[],
    }
}

/// The provider ids smith understands, for validating `/model
/// <provider>/<name>` without needing config/network access.
pub fn is_known_provider(provider: &str) -> bool {
    matches!(
        provider,
        "anthropic" | "openai" | "openrouter" | "9router" | "ollama"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_models_covers_supported_providers() {
        assert!(!known_models("anthropic").is_empty());
        assert!(!known_models("openai").is_empty());
        assert!(!known_models("ollama").is_empty());
        assert!(known_models("bogus").is_empty());
    }

    #[test]
    fn validates_provider_ids() {
        assert!(is_known_provider("ollama"));
        assert!(!is_known_provider("bogus"));
    }
}
