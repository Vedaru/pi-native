//! Google Generative AI (Gemini) streaming chunks.
//!
//! With `alt=sse`, `streamGenerateContent` emits `data:` frames whose payload is
//! a `GenerateContentResponse` chunk. Mirrors the streaming handling in pi's
//! `google-generative-ai.ts` / `google-shared.ts`.

use crate::anthropic::ContentBlock;
use crate::anthropic_stream::Usage;
use serde_json::{json, Value};

/// A parsed Gemini stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum GoogleStreamEvent {
    TextDelta(String),
    FunctionCall { name: String, args: Value },
    Usage(Usage),
    Done { finish_reason: Option<String> },
    Other,
}

/// Stateful parser for one Gemini stream.
#[derive(Debug, Default)]
pub struct GoogleStream {
    usage: Usage,
}

impl GoogleStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn usage(&self) -> &Usage {
        &self.usage
    }

    /// Consume one SSE data frame, returning any events it contained.
    pub fn handle(&mut self, data: &str) -> Vec<GoogleStreamEvent> {
        let Ok(json) = serde_json::from_str::<Value>(data) else {
            return vec![GoogleStreamEvent::Other];
        };
        let mut events = Vec::new();

        if let Some(candidates) = json.get("candidates").and_then(Value::as_array) {
            for candidate in candidates {
                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|content| content.get("parts"))
                    .and_then(Value::as_array)
                {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                events.push(GoogleStreamEvent::TextDelta(text.to_string()));
                            }
                        }
                        if let Some(call) = part.get("functionCall") {
                            events.push(GoogleStreamEvent::FunctionCall {
                                name: call
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                args: call.get("args").cloned().unwrap_or_else(|| json!({})),
                            });
                        }
                    }
                }
                if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                    events.push(GoogleStreamEvent::Done {
                        finish_reason: Some(reason.to_string()),
                    });
                }
            }
        }

        if let Some(meta) = json.get("usageMetadata") {
            let prompt = meta
                .get("promptTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let cached = meta
                .get("cachedContentTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            self.usage.cache_read = cached;
            self.usage.input = (prompt - cached).max(0);
            self.usage.output = meta
                .get("candidatesTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            self.usage.reasoning = meta
                .get("thoughtsTokenCount")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            events.push(GoogleStreamEvent::Usage(self.usage.clone()));
        }

        events
    }
}

/// Assemble text and function calls from a sequence of events.
pub fn collect_google(events: &[GoogleStreamEvent]) -> (String, Vec<ContentBlock>) {
    let mut text = String::new();
    let mut blocks = Vec::new();
    for event in events {
        match event {
            GoogleStreamEvent::TextDelta(delta) => text.push_str(delta),
            GoogleStreamEvent::FunctionCall { name, args } => blocks.push(ContentBlock::ToolUse {
                id: String::new(),
                name: name.clone(),
                input: args.clone(),
            }),
            _ => {}
        }
    }
    (text, blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_chunks_and_usage() {
        let mut stream = GoogleStream::new();
        let first = stream
            .handle(r#"{"candidates":[{"content":{"parts":[{"text":"Hel"}],"role":"model"}}]}"#);
        assert_eq!(first, vec![GoogleStreamEvent::TextDelta("Hel".into())]);
        let second = stream.handle(
            r#"{"candidates":[{"content":{"parts":[{"text":"lo"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":5,"cachedContentTokenCount":80}}"#,
        );
        assert!(second.contains(&GoogleStreamEvent::TextDelta("lo".into())));
        assert!(second.contains(&GoogleStreamEvent::Done {
            finish_reason: Some("STOP".into())
        }));
        assert_eq!(stream.usage().input, 20);
        assert_eq!(stream.usage().cache_read, 80);
        assert_eq!(stream.usage().output, 5);
        assert_eq!(stream.usage().cache_hit_rate(), Some(0.8));

        let (text, blocks) = collect_google(&first);
        assert_eq!(text, "Hel");
        assert!(blocks.is_empty());
    }

    #[test]
    fn parses_function_calls() {
        let mut stream = GoogleStream::new();
        let events = stream.handle(
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"x"}}}],"role":"model"}}]}"#,
        );
        let (_, blocks) = collect_google(&events);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolUse { name, input, .. } => {
                assert_eq!(name, "read");
                assert_eq!(input["path"], json!("x"));
            }
            _ => panic!("expected tool use"),
        }
    }

    #[test]
    fn invalid_json_is_other() {
        let mut stream = GoogleStream::new();
        assert_eq!(stream.handle("{nope"), vec![GoogleStreamEvent::Other]);
    }
}
