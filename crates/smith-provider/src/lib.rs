//! LLM provider adapters (Anthropic, OpenAI, ...) implementing smith_core::LlmProvider.

pub mod anthropic;
pub mod openai;

pub use anthropic::AnthropicProvider;
pub use openai::OpenAiProvider;
