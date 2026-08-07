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

/// Deliberately empty: a 9Router gateway's models are whatever its owner
/// configured in its dashboard, so there is no list to curate here.
///
/// This used to be `["auto"]`. No gateway is obliged to have a model by that
/// name — one measured in the wild had thirty-three and none of them was
/// `auto` — and asking for it made the gateway resolve it to whatever its
/// owner had set up, which failed as `No active credentials` and, on another
/// machine, as a user-defined combo that smith had no business naming. The
/// catalogue is read from `GET /v1/models` instead.
const NINEROUTER_MODELS: &[&str] = &[];
const OPENAI_MODELS: &[&str] = &["gpt-4.1", "gpt-4.1-mini", "gpt-4o", "o3"];
/// Ollama cloud models that answer on a **free** account, best first.
///
/// Cloud models are proxied by the local daemon, so they need no key from
/// smith and no VRAM — but the account behind the daemon must be entitled to
/// them, and most are not. There is no tier field anywhere: not in
/// `ollama.com/api/tags`, not in `/v1/models`, not on the model page. The only
/// way to know is to ask, and the refusal arrives as an error body.
///
/// So this list is measured, not read. Verified on 2026-08-06 by sending one
/// token to each of the eighteen models the public catalogue listed: seven
/// answered, eleven refused with "requires a subscription" or "requires both a
/// Pro, Max, or Team plan". Entitlements move, which is why the wizard offers
/// these alongside the live local catalogue and lets the refusal speak for
/// itself rather than trusting this list blindly — the same arrangement
/// `OPENROUTER_MODELS` has.
pub const OLLAMA_FREE_CLOUD_MODELS: &[&str] = &[
    "nemotron-3-ultra:cloud",
    "nemotron-3-super:cloud",
    "nemotron-3-nano:30b-cloud",
    "gpt-oss:120b-cloud",
    "gpt-oss:20b-cloud",
    "gemma4:31b-cloud",
    "minimax-m3:cloud",
];

/// Only the fallback for a daemon that did not answer — the wizard reads
/// `/api/tags` first, because what a machine has pulled is a fact and this is
/// a guess. Cloud models lead: they need no VRAM and no download, so they are
/// what a fresh install can actually run.
const OLLAMA_MODELS: &[&str] = &[
    "nemotron-3-super:cloud",
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
