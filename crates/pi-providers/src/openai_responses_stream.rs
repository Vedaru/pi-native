//! OpenAI Responses streaming events.
//!
//! Consumes SSE frames and produces typed events, including usage. Mirrors
//! `packages/ai/src/api/openai-responses-shared.ts`.

use crate::types::ContentBlock;
use crate::types::Usage;
use serde_json::{json, Value};

/// A parsed OpenAI Responses stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum OpenAiResponsesStreamEvent {
    Created {
        id: Option<String>,
    },
    TextDelta {
        delta: String,
    },
    ToolCallStart {
        item_id: Option<String>,
        call_id: String,
        name: String,
    },
    ToolCallArgsDelta {
        call_id: String,
        delta: String,
    },
    Completed {
        usage: Usage,
        stop_reason: Option<String>,
    },
    Incomplete {
        usage: Usage,
        stop_reason: Option<String>,
    },
    Failed {
        message: String,
    },
    Other {
        event: String,
    },
}

/// Stateful parser for one Responses stream.
#[derive(Debug, Default)]
pub struct OpenAiResponsesStream {
    usage: Usage,
    message_id: Option<String>,
}

impl OpenAiResponsesStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    fn apply_usage(&mut self, response: &Value) {
        let Some(usage) = response.get("usage") else {
            return;
        };
        let input_tokens = usage
            .get("input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let details = usage.get("input_tokens_details");
        let cached = details
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let cache_write = details
            .and_then(|d| d.get("cache_write_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        self.usage.input = (input_tokens - cached - cache_write).max(0);
        self.usage.cache_read = cached;
        self.usage.cache_write = cache_write;
        self.usage.output = usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        self.usage.reasoning = usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
    }

    /// Handle one SSE event, returning the typed event.
    pub fn handle(&mut self, event_type: &str, data: &str) -> OpenAiResponsesStreamEvent {
        let json: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(error) => {
                return OpenAiResponsesStreamEvent::Failed {
                    message: format!("invalid event JSON: {error}"),
                }
            }
        };

        match event_type {
            "response.created" => {
                if let Some(id) = json
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(Value::as_str)
                {
                    self.message_id = Some(id.to_string());
                }
                OpenAiResponsesStreamEvent::Created {
                    id: self.message_id.clone(),
                }
            }
            "response.output_text.delta" => OpenAiResponsesStreamEvent::TextDelta {
                delta: json
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            },
            "response.output_item.added" => {
                let item = json.get("item");
                match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                    Some("function_call") => OpenAiResponsesStreamEvent::ToolCallStart {
                        item_id: item
                            .and_then(|i| i.get("id"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        call_id: item
                            .and_then(|i| i.get("call_id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: item
                            .and_then(|i| i.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                    _ => OpenAiResponsesStreamEvent::Other {
                        event: event_type.to_string(),
                    },
                }
            }
            "response.function_call_arguments.delta" => {
                OpenAiResponsesStreamEvent::ToolCallArgsDelta {
                    call_id: json
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    delta: json
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = json.get("response");
                if let Some(response) = response {
                    self.apply_usage(response);
                    if self.message_id.is_none() {
                        self.message_id = response
                            .get("id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                }
                let usage = self.usage.clone();
                let stop_reason = response
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if event_type == "response.incomplete" {
                    OpenAiResponsesStreamEvent::Incomplete { usage, stop_reason }
                } else {
                    OpenAiResponsesStreamEvent::Completed { usage, stop_reason }
                }
            }
            "response.failed" | "error" => OpenAiResponsesStreamEvent::Failed {
                message: json
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .or_else(|| json.get("error").and_then(|e| e.get("message")))
                    .and_then(Value::as_str)
                    .unwrap_or("response failed")
                    .to_string(),
            },
            other => OpenAiResponsesStreamEvent::Other {
                event: other.to_string(),
            },
        }
    }
}

/// Assemble text and tool calls from a sequence of typed events.
pub fn collect_response(events: &[OpenAiResponsesStreamEvent]) -> (String, Vec<ContentBlock>) {
    use std::collections::HashMap;
    let mut text = String::new();
    let mut order: Vec<(Option<String>, String, String, String)> = Vec::new();
    // Index each call's position in `order` by its `call_id` so argument deltas
    // are O(1) instead of a linear scan per delta.
    let mut by_call_id: HashMap<String, usize> = HashMap::new();
    for event in events {
        match event {
            OpenAiResponsesStreamEvent::TextDelta { delta } => text.push_str(delta),
            OpenAiResponsesStreamEvent::ToolCallStart {
                item_id,
                call_id,
                name,
            } => {
                by_call_id.insert(call_id.clone(), order.len());
                order.push((
                    item_id.clone(),
                    call_id.clone(),
                    name.clone(),
                    String::new(),
                ));
            }
            OpenAiResponsesStreamEvent::ToolCallArgsDelta { call_id, delta } => {
                let index = *by_call_id.entry(call_id.clone()).or_insert_with(|| {
                    // No prior `ToolCallStart`: keep the delta instead of dropping it.
                    order.push((None, call_id.clone(), String::new(), String::new()));
                    order.len() - 1
                });
                order[index].3.push_str(delta);
            }
            _ => {}
        }
    }
    let blocks: Vec<ContentBlock> = order
        .into_iter()
        .map(|(_, call_id, name, arguments)| {
            let input = serde_json::from_str(&arguments).unwrap_or_else(|_| json!({}));
            ContentBlock::ToolUse {
                id: call_id,
                name,
                input,
            }
        })
        .collect();
    (text, blocks)
}

#[cfg(test)]
#[path = "../tests/unit/openai_responses_stream.rs"]
mod tests;
