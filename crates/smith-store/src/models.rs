//! Small, hand-maintained lists of popular models per provider, shared
//! between the `smith setup` wizard and the runtime `/model` command. Not
//! exhaustive — providers accept any model string, this is just what's
//! offered as quick choices.

const ANTHROPIC_MODELS: &[&str] = &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"];
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

/// Known model names for a provider id ("anthropic" | "openai" | "ollama"),
/// or an empty slice for anything else.
pub fn known_models(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => ANTHROPIC_MODELS,
        "openai" => OPENAI_MODELS,
        "ollama" => OLLAMA_MODELS,
        _ => &[],
    }
}

/// The three provider ids smith understands, for validating `/model
/// <provider>/<name>` without needing config/network access.
pub fn is_known_provider(provider: &str) -> bool {
    matches!(provider, "anthropic" | "openai" | "ollama")
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
