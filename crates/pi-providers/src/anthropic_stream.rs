//! Anthropic Messages streaming events.
//!
//! Consumes SSE frames (see `pi-sse`) and produces typed events, including the
//! usage fields needed for cache-hit-rate accounting. Mirrors the event handling
//! in pi `packages/ai/src/api/anthropic-messages.ts` (`iterateAnthropicEvents`).

use crate::anthropic::ContentBlock;
use serde_json::Value;

/// Token usage as reported by Anthropic.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Subset of `cache_write` written with 1h retention.
    pub cache_write_1h: i64,
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

/// A parsed Anthropic stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum AnthropicStreamEvent {
    MessageStart {
        id: Option<String>,
        usage: Usage,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    ThinkingDelta {
        index: usize,
        thinking: String,
    },
    /// A tool call began; `id` and `name` come from `content_block_start`.
    ToolUseStart {
        index: usize,
        id: String,
        name: String,
    },
    /// A fragment of the tool call's streaming JSON arguments.
    ToolUseInputDelta {
        index: usize,
        partial_json: String,
    },
    MessageDelta {
        stop_reason: Option<String>,
        usage: Usage,
    },
    MessageStop,
    Error {
        message: String,
    },
    /// An event type this parser does not model (kept so callers can ignore it).
    Other {
        event: String,
    },
}

/// Stateful parser for one Anthropic message stream.
#[derive(Debug, Default)]
pub struct AnthropicStream {
    usage: Usage,
    message_id: Option<String>,
}

impl AnthropicStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cumulative usage observed so far.
    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// The provider message id, once `message_start` has been seen.
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    fn apply_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("input_tokens").and_then(Value::as_i64) {
            self.usage.input = v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(Value::as_i64) {
            self.usage.output = v;
        }
        if let Some(v) = usage.get("cache_read_input_tokens").and_then(Value::as_i64) {
            self.usage.cache_read = v;
        }
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_i64)
        {
            self.usage.cache_write = v;
        }
        if let Some(v) = usage
            .get("cache_creation")
            .and_then(|c| c.get("ephemeral_1h_input_tokens"))
            .and_then(Value::as_i64)
        {
            self.usage.cache_write_1h = v;
        }
    }

    /// Handle one SSE event, returning the typed stream event.
    pub fn handle(&mut self, sse_event: &str, data: &str) -> AnthropicStreamEvent {
        let json: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                return AnthropicStreamEvent::Error {
                    message: format!("invalid event JSON: {error}"),
                }
            }
        };

        match sse_event {
            "message_start" => {
                if let Some(id) = json
                    .get("message")
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                {
                    self.message_id = Some(id.to_string());
                }
                if let Some(usage) = json.get("message").and_then(|m| m.get("usage")) {
                    self.apply_usage(usage);
                }
                AnthropicStreamEvent::MessageStart {
                    id: self.message_id.clone(),
                    usage: self.usage.clone(),
                }
            }
            "content_block_start" => {
                let index = index_of(&json);
                let block = json.get("content_block");
                match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                    Some("tool_use") => AnthropicStreamEvent::ToolUseStart {
                        index,
                        id: block
                            .and_then(|b| b.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .and_then(|b| b.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                    _ => AnthropicStreamEvent::Other {
                        event: "content_block_start".to_string(),
                    },
                }
            }
            "content_block_delta" => {
                let index = index_of(&json);
                let delta = json.get("delta");
                match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                    Some("text_delta") => AnthropicStreamEvent::TextDelta {
                        index,
                        text: delta
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                    Some("thinking_delta") => AnthropicStreamEvent::ThinkingDelta {
                        index,
                        thinking: delta
                            .and_then(|d| d.get("thinking"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                    Some("input_json_delta") => AnthropicStreamEvent::ToolUseInputDelta {
                        index,
                        partial_json: delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                    _ => AnthropicStreamEvent::Other {
                        event: "content_block_delta".to_string(),
                    },
                }
            }
            "message_delta" => {
                if let Some(usage) = json.get("usage") {
                    self.apply_usage(usage);
                }
                AnthropicStreamEvent::MessageDelta {
                    stop_reason: json
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    usage: self.usage.clone(),
                }
            }
            "message_stop" => AnthropicStreamEvent::MessageStop,
            "error" => AnthropicStreamEvent::Error {
                message: json
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown stream error")
                    .to_string(),
            },
            other => AnthropicStreamEvent::Other {
                event: other.to_string(),
            },
        }
    }
}

fn index_of(json: &Value) -> usize {
    json.get("index").and_then(Value::as_u64).unwrap_or(0) as usize
}

/// Assemble the completed content blocks from a sequence of typed events.
///
/// Streaming tool-call JSON is concatenated per index; callers parse it after
/// the stream ends.
pub fn collect_content(events: &[AnthropicStreamEvent]) -> (Vec<ContentBlock>, String) {
    use std::collections::BTreeMap;

    let mut texts: BTreeMap<usize, String> = BTreeMap::new();
    let mut thinking: BTreeMap<usize, String> = BTreeMap::new();
    let mut tools: BTreeMap<usize, (String, String, String)> = BTreeMap::new();

    for event in events {
        match event {
            AnthropicStreamEvent::TextDelta { index, text } => {
                texts.entry(*index).or_default().push_str(text);
            }
            AnthropicStreamEvent::ThinkingDelta { index, thinking: t } => {
                thinking.entry(*index).or_default().push_str(t);
            }
            AnthropicStreamEvent::ToolUseStart { index, id, name } => {
                tools.insert(*index, (id.clone(), name.clone(), String::new()));
            }
            AnthropicStreamEvent::ToolUseInputDelta {
                index,
                partial_json,
            } => {
                if let Some(entry) = tools.get_mut(index) {
                    entry.2.push_str(partial_json);
                }
            }
            _ => {}
        }
    }

    let mut blocks = Vec::new();
    let mut indices: Vec<usize> = texts
        .keys()
        .chain(thinking.keys())
        .chain(tools.keys())
        .copied()
        .collect();
    indices.sort_unstable();
    indices.dedup();

    let mut text_concat = String::new();
    for index in indices {
        if let Some(t) = texts.get(&index) {
            text_concat.push_str(t);
            blocks.push(ContentBlock::Text {
                text: t.clone(),
                cache_control: None,
            });
        }
        if let Some(t) = thinking.get(&index) {
            blocks.push(ContentBlock::Thinking {
                thinking: t.clone(),
                signature: String::new(),
            });
        }
        if let Some((id, name, partial)) = tools.get(&index) {
            let input = if partial.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(partial).unwrap_or(serde_json::json!({}))
            };
            blocks.push(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input,
            });
        }
    }
    (blocks, text_concat)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":900,"cache_creation_input_tokens":200}}}"#;
    const TEXT_START: &str =
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
    const TEXT_DELTA: &str =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
    const TOOL_START: &str = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}"#;
    const TOOL_DELTA: &str = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x\"}"}}"#;
    const MESSAGE_DELTA: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":50}}"#;

    #[test]
    fn parses_usage_and_cache_hit_rate() {
        let mut stream = AnthropicStream::new();
        let event = stream.handle("message_start", MESSAGE_START);
        match event {
            AnthropicStreamEvent::MessageStart { id, usage } => {
                assert_eq!(id.as_deref(), Some("msg_1"));
                assert_eq!(usage.input, 100);
                assert_eq!(usage.cache_read, 900);
                assert_eq!(usage.cache_write, 200);
                // 900 / (100 + 900 + 200) = 0.75
                assert_eq!(usage.cache_hit_rate(), Some(0.75));
            }
            _ => panic!("expected message_start"),
        }
    }

    #[test]
    fn output_tokens_update_on_message_delta() {
        let mut stream = AnthropicStream::new();
        stream.handle("message_start", MESSAGE_START);
        let event = stream.handle("message_delta", MESSAGE_DELTA);
        match event {
            AnthropicStreamEvent::MessageDelta { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("tool_use"));
                assert_eq!(usage.output, 50);
                // prior fields retained
                assert_eq!(usage.cache_read, 900);
            }
            _ => panic!("expected message_delta"),
        }
        assert_eq!(stream.usage().output, 50);
    }

    #[test]
    fn collects_text_and_tool_calls_in_order() {
        let mut stream = AnthropicStream::new();
        let events = [
            stream.handle("message_start", MESSAGE_START),
            stream.handle("content_block_start", TEXT_START),
            stream.handle("content_block_delta", TEXT_DELTA),
            stream.handle("content_block_start", TOOL_START),
            stream.handle("content_block_delta", TOOL_DELTA),
            stream.handle("message_delta", MESSAGE_DELTA),
            stream.handle("message_stop", "{}"),
        ];
        let (blocks, text) = collect_content(&events);
        assert_eq!(text, "Hello");
        assert_eq!(blocks.len(), 2);
        match &blocks[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_1");
                assert_eq!(name, "read");
                assert_eq!(input["path"], serde_json::json!("x"));
            }
            _ => panic!("expected tool_use"),
        }
    }

    #[test]
    fn error_events_surface_message() {
        let mut stream = AnthropicStream::new();
        let event = stream.handle(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
        );
        assert_eq!(
            event,
            AnthropicStreamEvent::Error {
                message: "busy".to_string()
            }
        );
    }

    #[test]
    fn invalid_json_becomes_error() {
        let mut stream = AnthropicStream::new();
        assert!(matches!(
            stream.handle("message_start", "{not json"),
            AnthropicStreamEvent::Error { .. }
        ));
    }
}
