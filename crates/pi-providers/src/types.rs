//! Provider-agnostic wire types shared by the OpenAI request builders and the
//! stream parsers.
//!
//! Shared by the OpenAI request builders and the stream parsers.

use pi_cache::CacheControlEphemeral;
use serde::Serialize;
use serde_json::Value;

/// A tool spec as supplied by the tool registry.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// Full JSON schema (object with `type`, `properties`, `required`).
    pub input_schema: Value,
    /// Whether the tool prefers constrained (strict) JSON-schema sampling.
    pub strict: bool,
}

/// A message content block produced by the stream parsers. `cache_control` is
/// unused and always `None` now that only OpenAI formats are built.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Image {
        source: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: MessageContent,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

/// Message content: a bare string for text-only content, or a block array.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Token usage as reported by a provider.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Subset of `cache_write` written with 1h retention.
    pub cache_write_1h: i64,
    /// Reasoning tokens (a subset of `output`), when the provider reports them.
    pub reasoning: i64,
}

impl Usage {
    /// Total prompt tokens (input + cache read + cache write).
    pub fn prompt_tokens(&self) -> i64 {
        self.input + self.cache_read + self.cache_write
    }

    /// Fraction of prompt tokens served from cache, in `[0, 1]`.
    /// Returns `None` when the prompt is empty.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let prompt = self.prompt_tokens();
        if prompt <= 0 {
            return None;
        }
        Some(self.cache_read as f64 / prompt as f64)
    }
}
