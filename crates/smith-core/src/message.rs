use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
        }
    }

    /// Concatenation of all Text blocks, ignoring tool_use/tool_result blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

/// `snake_case` on the wire like every other enum in the event stream — this
/// one rides inside `assistant_turn_complete`, and PascalCase here would have
/// been the single inconsistency a `jq` user tripped over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    Cancelled,
}

/// Tokens a single provider response accounted for.
///
/// The four fields are disjoint, which is the only way the arithmetic works:
/// Anthropic reports `input_tokens` *excluding* anything served from or
/// written to the prompt cache, so context occupancy is
/// `input_tokens + cache_read + cache_write`, not `input_tokens` alone — see
/// [`Usage::prompt_tokens`]. Treating them as overlapping would under-report
/// the context by exactly the cached prefix, which on a long session is most
/// of it.
///
/// The cache fields are `#[serde(default)]` because this type rides the
/// `--output-format stream-json` wire format inside `token_usage`: an event
/// recorded by an older smith must still deserialize.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Prompt tokens served from a provider-side cache hit. Billed at a
    /// fraction of the input rate (Anthropic: 0.1x; OpenAI: 0.25–0.5x).
    #[serde(default)]
    pub cache_read: u32,
    /// Prompt tokens written into the cache on this request. Billed at a
    /// premium over the input rate (Anthropic 5-minute TTL: 1.25x); OpenAI
    /// does not charge for cache writes and always reports zero here.
    #[serde(default)]
    pub cache_write: u32,
}

impl Usage {
    /// Every token that actually occupied the context window on this request.
    pub fn prompt_tokens(&self) -> u32 {
        self.input_tokens
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    /// Prompt plus completion — what one request cost in total tokens.
    pub fn total_tokens(&self) -> u32 {
        self.prompt_tokens().saturating_add(self.output_tokens)
    }

    /// Accumulates another response's usage into this one. Saturating rather
    /// than wrapping: a long session's totals overflowing `u32` should pin at
    /// the maximum, never silently restart from zero.
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
    }
}

/// Normalized stream events every provider adapter must produce, regardless of
/// how differently each provider frames tool-call streaming on the wire.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    TextDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    /// Raw partial JSON fragment for a tool call's input; accumulate and parse
    /// once `ToolUseComplete` arrives.
    ToolUseInputDelta {
        id: String,
        partial_json: String,
    },
    ToolUseComplete {
        id: String,
    },
    MessageComplete {
        stop_reason: StopReason,
        usage: Usage,
    },
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason the cache fields exist as separate numbers: Anthropic
    /// bills and reports them apart from `input_tokens`, so anything that
    /// wants "how full is the context" has to add all three.
    #[test]
    fn prompt_tokens_counts_cached_tokens_too() {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 40,
            cache_read: 9_000,
            cache_write: 500,
        };
        assert_eq!(usage.prompt_tokens(), 9_600);
        assert_eq!(usage.total_tokens(), 9_640);
    }

    #[test]
    fn add_accumulates_every_field() {
        let mut total = Usage::default();
        total.add(&Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read: 3,
            cache_write: 4,
        });
        total.add(&Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read: 30,
            cache_write: 40,
        });
        assert_eq!(
            total,
            Usage {
                input_tokens: 11,
                output_tokens: 22,
                cache_read: 33,
                cache_write: 44,
            }
        );
    }

    #[test]
    fn add_saturates_instead_of_wrapping() {
        let mut total = Usage {
            input_tokens: u32::MAX,
            ..Usage::default()
        };
        total.add(&Usage {
            input_tokens: 10,
            ..Usage::default()
        });
        assert_eq!(total.input_tokens, u32::MAX);
    }

    /// `Usage` rides the stream-json wire format; an event written by a smith
    /// that predates the cache fields must still load.
    #[test]
    fn usage_deserializes_without_the_cache_fields() {
        let usage: Usage =
            serde_json::from_str(r#"{"input_tokens": 5, "output_tokens": 7}"#).unwrap();
        assert_eq!(usage.cache_read, 0);
        assert_eq!(usage.cache_write, 0);
        assert_eq!(usage.prompt_tokens(), 5);
    }
}
