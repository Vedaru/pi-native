//! Shared transcript types and size estimates.
//!
//! These are the provider-agnostic shapes the agent loop holds and the OpenAI
//! builders convert from.

use serde_json::Value;

/// A text or image part, used for user content and tool-result content.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    Text { text: String },
    Image { data: String, mime_type: String },
}

/// An assistant content block, mirroring pi's message content union.
#[derive(Debug, Clone, PartialEq)]
pub enum AssistantBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
        redacted: bool,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// A transcript message as the agent loop holds it.
#[derive(Debug, Clone)]
pub enum TranscriptMessage {
    /// `UserMessage` whose content is a bare string.
    UserText(String),
    /// `UserMessage` whose content is a text/image array.
    UserParts(Vec<ContentPart>),
    Assistant(Vec<AssistantBlock>),
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<ContentPart>,
        is_error: bool,
    },
}

fn part_bytes(part: &ContentPart) -> usize {
    match part {
        ContentPart::Text { text } => text.len() + 16,
        ContentPart::Image { data, mime_type } => data.len() + mime_type.len() + 24,
    }
}

fn block_bytes(block: &AssistantBlock) -> usize {
    match block {
        AssistantBlock::Text { text } => text.len() + 16,
        AssistantBlock::Thinking { thinking, .. } => thinking.len() + 16,
        AssistantBlock::ToolCall {
            id,
            name,
            arguments,
        } => id.len() + name.len() + arguments.to_string().len() + 24,
    }
}

impl TranscriptMessage {
    /// Approximate token count (bytes / 4), matching pi's rough estimator.
    pub fn approx_tokens(&self) -> usize {
        self.approx_bytes().div_ceil(4)
    }

    /// Approximate serialized size in bytes, for context budgeting.
    pub fn approx_bytes(&self) -> usize {
        match self {
            TranscriptMessage::UserText(text) => text.len() + 32,
            TranscriptMessage::UserParts(parts) => parts.iter().map(part_bytes).sum::<usize>() + 32,
            TranscriptMessage::Assistant(blocks) => {
                blocks.iter().map(block_bytes).sum::<usize>() + 48
            }
            TranscriptMessage::ToolResult {
                tool_call_id,
                tool_name,
                content,
                ..
            } => {
                tool_call_id.len()
                    + tool_name.len()
                    + content.iter().map(part_bytes).sum::<usize>()
                    + 48
            }
        }
    }
}
