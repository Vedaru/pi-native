//! OpenAI-compatible Chat Completions streaming events.
//!
//! Consumes SSE frames from `POST /chat/completions` and produces typed events,
//! including usage. Mirrors the delta handling in pi
//! `packages/ai/src/api/openai-completions.ts`.

use crate::types::ContentBlock;
use crate::types::Usage;
use serde_json::Value;

/// A parsed Chat Completions stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenAiCompletionsStreamEvent {
    TextDelta {
        delta: String,
    },
    ReasoningDelta {
        delta: String,
    },
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    Finished {
        finish_reason: Option<String>,
    },
    Other,
}

/// Stateful parser for one Chat Completions stream.
#[derive(Debug, Default)]
pub struct OpenAiCompletionsStream {
    usage: Usage,
    message_id: Option<String>,
}

impl OpenAiCompletionsStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// Consume one SSE `data:` payload.
    pub fn handle(&mut self, data: &str) -> OpenAiCompletionsStreamEvent {
        let trimmed = data.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return OpenAiCompletionsStreamEvent::Other;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            return OpenAiCompletionsStreamEvent::Other;
        };

        if let Some(id) = value.get("id").and_then(Value::as_str) {
            self.message_id = Some(id.to_string());
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            let prompt_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let details = usage.get("prompt_tokens_details");
            // pi's order: prompt_tokens_details.cached_tokens, then
            // prompt_cache_hit_tokens (DeepSeek), then top-level cached_tokens.
            let cache_read = details
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_i64)
                .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_i64))
                .or_else(|| usage.get("cached_tokens").and_then(Value::as_i64))
                .unwrap_or(0);
            let cache_write = details
                .and_then(|details| details.get("cache_write_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            // `prompt_tokens` includes cached tokens, so `input` is the uncached
            // remainder (matches pi's `parseChunkUsage`).
            self.usage.input = (prompt_tokens - cache_read - cache_write).max(0);
            self.usage.output = usage
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            self.usage.cache_read = cache_read;
            self.usage.cache_write = cache_write;
            self.usage.reasoning = usage
                .get("completion_tokens_details")
                .and_then(|details| details.get("reasoning_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
        }

        let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) else {
            return OpenAiCompletionsStreamEvent::Other;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            return OpenAiCompletionsStreamEvent::Finished {
                finish_reason: Some(reason.to_string()),
            };
        }
        let Some(delta) = choice.get("delta") else {
            return OpenAiCompletionsStreamEvent::Other;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                return OpenAiCompletionsStreamEvent::TextDelta {
                    delta: text.to_string(),
                };
            }
        }
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
            if !reasoning.is_empty() {
                return OpenAiCompletionsStreamEvent::ReasoningDelta {
                    delta: reasoning.to_string(),
                };
            }
        }
        if let Some(call) = delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .and_then(|calls| calls.first())
        {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let id = call.get("id").and_then(Value::as_str).map(str::to_string);
            let function = call.get("function");
            let name = function
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let arguments = function
                .and_then(|function| function.get("arguments"))
                .and_then(Value::as_str)
                .map(str::to_string);
            return OpenAiCompletionsStreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            };
        }
        OpenAiCompletionsStreamEvent::Other
    }
}

/// Accumulate text, reasoning, and tool calls into content blocks.
pub fn collect_completions(events: &[OpenAiCompletionsStreamEvent]) -> (String, Vec<ContentBlock>) {
    use OpenAiCompletionsStreamEvent::*;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: Vec<(String, String, String)> = Vec::new();
    for event in events {
        match event {
            TextDelta { delta } => text.push_str(delta),
            ReasoningDelta { delta } => reasoning.push_str(delta),
            ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => {
                while calls.len() <= *index {
                    calls.push((String::new(), String::new(), String::new()));
                }
                let call = &mut calls[*index];
                if let Some(id) = id {
                    call.0 = id.clone();
                }
                if let Some(name) = name {
                    call.1 = name.clone();
                }
                if let Some(arguments) = arguments {
                    call.2.push_str(arguments);
                }
            }
            Finished { .. } | Other => {}
        }
    }

    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(ContentBlock::Text {
            text: text.clone(),
            cache_control: None,
        });
    }
    if !reasoning.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: reasoning,
            signature: String::new(),
        });
    }
    for (id, name, arguments) in calls {
        let input = serde_json::from_str(&arguments).unwrap_or_else(|_| serde_json::json!({}));
        content.push(ContentBlock::ToolUse { id, name, input });
    }
    (text, content)
}
