//! Transcript-to-Anthropic conversion.
//!
//! Mirrors pi `packages/ai/src/api/anthropic-messages.ts`: `convertMessages`,
//! `convertContentBlocks`, and `convertToolResult`.

use crate::anthropic::{AnthropicMessage, ContentBlock, MessageContent};
use serde_json::{json, Value};

/// A text or image part, used for user content and tool-result content.
#[derive(Debug, Clone)]
pub enum ContentPart {
    Text { text: String },
    Image { data: String, mime_type: String },
}

/// An assistant content block, mirroring pi's message content union.
#[derive(Debug, Clone)]
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
        content: Vec<ContentPart>,
        is_error: bool,
    },
}

fn image_block(part: &ContentPart) -> Option<ContentBlock> {
    match part {
        ContentPart::Image { data, mime_type } => Some(ContentBlock::Image {
            source: json!({
                "type": "base64",
                "media_type": mime_type,
                "data": data,
            }),
            cache_control: None,
        }),
        ContentPart::Text { .. } => None,
    }
}

/// Pi returns a concatenated string when there are no images, else a block array.
fn convert_content_blocks(parts: &[ContentPart]) -> MessageContent {
    let has_images = parts.iter().any(|p| matches!(p, ContentPart::Image { .. }));
    if !has_images {
        let joined = parts
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => text.as_str(),
                ContentPart::Image { .. } => unreachable!("no images in this branch"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        return MessageContent::Text(joined);
    }
    let blocks = parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => ContentBlock::Text {
                text: text.clone(),
                cache_control: None,
            },
            ContentPart::Image { .. } => image_block(part).expect("image"),
        })
        .collect();
    MessageContent::Blocks(blocks)
}

fn message(role: &'static str, content: MessageContent) -> AnthropicMessage {
    AnthropicMessage { role, content }
}

fn convert_assistant(blocks: &[AssistantBlock]) -> Vec<ContentBlock> {
    let mut out = Vec::new();
    for block in blocks {
        match block {
            AssistantBlock::Text { text } => {
                if text.trim().is_empty() {
                    continue;
                }
                out.push(ContentBlock::Text {
                    text: text.clone(),
                    cache_control: None,
                });
            }
            AssistantBlock::Thinking {
                thinking,
                signature,
                redacted,
            } => {
                if *redacted {
                    out.push(ContentBlock::RedactedThinking {
                        data: signature.clone().unwrap_or_default(),
                    });
                    continue;
                }
                let has_signature = signature.as_deref().is_some_and(|s| !s.trim().is_empty());
                if thinking.trim().is_empty() && !has_signature {
                    continue;
                }
                if has_signature {
                    out.push(ContentBlock::Thinking {
                        thinking: thinking.clone(),
                        signature: signature.clone().unwrap_or_default(),
                    });
                } else {
                    // Missing/empty signature: fall back to plain text.
                    out.push(ContentBlock::Text {
                        text: thinking.clone(),
                        cache_control: None,
                    });
                }
            }
            AssistantBlock::ToolCall {
                id,
                name,
                arguments,
            } => out.push(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: arguments.clone(),
            }),
        }
    }
    out
}

/// Convert a transcript to Anthropic messages, grouping consecutive tool
/// results into a single user message like pi does.
pub fn convert_messages(messages: &[TranscriptMessage]) -> Vec<AnthropicMessage> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            TranscriptMessage::UserText(text) => {
                if !text.trim().is_empty() {
                    out.push(message("user", MessageContent::Text(text.clone())));
                }
            }
            TranscriptMessage::UserParts(parts) => {
                let content = convert_content_blocks(parts);
                let empty = match &content {
                    MessageContent::Text(t) => t.trim().is_empty(),
                    MessageContent::Blocks(b) => b.is_empty(),
                };
                if !empty {
                    out.push(message("user", content));
                }
            }
            TranscriptMessage::Assistant(blocks) => {
                let content = convert_assistant(blocks);
                if !content.is_empty() {
                    out.push(message("assistant", MessageContent::Blocks(content)));
                }
            }
            TranscriptMessage::ToolResult { .. } => {
                let mut content = Vec::new();
                let mut j = index;
                while let Some(TranscriptMessage::ToolResult {
                    tool_call_id,
                    content: parts,
                    is_error,
                }) = messages.get(j)
                {
                    content.push(ContentBlock::ToolResult {
                        tool_use_id: tool_call_id.clone(),
                        content: convert_content_blocks(parts),
                        is_error: *is_error,
                        cache_control: None,
                    });
                    j += 1;
                }
                index = j - 1;
                out.push(message("user", MessageContent::Blocks(content)));
            }
        }
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_only_tool_result_becomes_string() {
        let parts = vec![
            ContentPart::Text { text: "a".into() },
            ContentPart::Text { text: "b".into() },
        ];
        match convert_content_blocks(&parts) {
            MessageContent::Text(t) => assert_eq!(t, "a\nb"),
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn image_forces_block_array() {
        let parts = vec![
            ContentPart::Text {
                text: "look".into(),
            },
            ContentPart::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            },
        ];
        match convert_content_blocks(&parts) {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(blocks[1], ContentBlock::Image { .. }));
            }
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn consecutive_tool_results_group_into_one_user_message() {
        let messages = vec![
            TranscriptMessage::ToolResult {
                tool_call_id: "a".into(),
                content: vec![ContentPart::Text { text: "1".into() }],
                is_error: false,
            },
            TranscriptMessage::ToolResult {
                tool_call_id: "b".into(),
                content: vec![ContentPart::Text { text: "2".into() }],
                is_error: true,
            },
        ];
        let converted = convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        match &converted[0].content {
            MessageContent::Blocks(blocks) => assert_eq!(blocks.len(), 2),
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn empty_assistant_is_dropped() {
        let messages = vec![TranscriptMessage::Assistant(vec![AssistantBlock::Text {
            text: "   ".into(),
        }])];
        assert!(convert_messages(&messages).is_empty());
    }
}
