//! Adapter from the native RPC event stream to pi's JSON event stream.
//!
//! pi's canonical event shapes are documented in
//! `@earendil-works/pi-coding-agent/docs/json.md` and are what `pi-web` and
//! pi's own `RpcClient` consume. [`PiEventAdapter`] folds the native events
//! into those shapes, synthesizing the message lifecycle (`message_start`,
//! `message_update`, `message_end`) that the native stream does not carry.
//!
//! The adapter is transport-free: the HTTP/SSE gateway (VED-341) and a pi-web
//! adapter (VED-342) both drive it.

use crate::Event;
use serde_json::{json, Value};

/// Stateful translator; one per attached session stream.
pub struct PiEventAdapter {
    message_counter: u64,
    /// A `message_start` was emitted and no `message_end` yet.
    streaming: bool,
    text_started: bool,
    thinking_started: bool,
    /// True once a streaming delta supplied the current text.
    saw_delta: bool,
    text: String,
    thinking: String,
    /// Content index of the current text/thinking block (stable across deltas).
    text_index: usize,
    thinking_index: usize,
    tool_calls: Vec<(String, String, Value)>,
    /// Tool results seen since the current turn began, for `turn_end`.
    tool_results: Vec<Value>,
    next_content_index: usize,
    last_usage: Option<Value>,
}

impl Default for PiEventAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl PiEventAdapter {
    pub fn new() -> Self {
        Self {
            message_counter: 0,
            streaming: false,
            text_started: false,
            thinking_started: false,
            saw_delta: false,
            text: String::new(),
            thinking: String::new(),
            text_index: 0,
            thinking_index: 0,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            next_content_index: 0,
            last_usage: None,
        }
    }

    fn begin_message(&mut self, out: &mut Vec<Value>) {
        if self.streaming {
            return;
        }
        let id = format!("msg-{}", self.message_counter);
        self.message_counter += 1;
        self.streaming = true;
        self.text_started = false;
        self.thinking_started = false;
        self.saw_delta = false;
        self.text.clear();
        self.thinking.clear();
        self.text_index = 0;
        self.thinking_index = 0;
        self.tool_calls.clear();
        self.next_content_index = 0;
        out.push(json!({
            "type": "message_start",
            "message": { "id": id, "role": "assistant", "content": [] }
        }));
    }

    fn assistant_message(&self) -> Value {
        let mut content: Vec<Value> = Vec::new();
        if !self.thinking.is_empty() {
            content.push(json!({ "type": "thinking", "thinking": self.thinking }));
        }
        if !self.text.is_empty() {
            content.push(json!({ "type": "text", "text": self.text }));
        }
        for (id, name, arguments) in &self.tool_calls {
            content.push(json!({
                "type": "toolCall",
                "id": id,
                "name": name,
                "arguments": arguments,
            }));
        }
        let mut message = json!({ "role": "assistant", "content": content });
        if let Some(usage) = &self.last_usage {
            message["usage"] = usage.clone();
        }
        message
    }

    /// Translate one native event into pi-shaped events (possibly none).
    pub fn translate(&mut self, event: &Event) -> Vec<Value> {
        let mut out = Vec::new();
        match event {
            Event::AgentStart => out.push(json!({ "type": "agent_start" })),
            Event::TurnStart => {
                // Usage belongs to the turn that is starting.
                self.last_usage = None;
                self.tool_results.clear();
                out.push(json!({ "type": "turn_start" }));
            }
            Event::AssistantDelta { text } => {
                self.begin_message(&mut out);
                if !self.text_started {
                    self.text_started = true;
                    self.text_index = self.next_content_index;
                    self.next_content_index += 1;
                    out.push(json!({
                        "type": "message_update",
                        "assistantMessageEvent": { "type": "text_start", "contentIndex": self.text_index }
                    }));
                }
                self.text.push_str(text);
                self.saw_delta = true;
                out.push(json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "text_delta",
                        "contentIndex": self.text_index,
                        "delta": text,
                    }
                }));
            }
            Event::ThinkingDelta { text } => {
                self.begin_message(&mut out);
                if !self.thinking_started {
                    self.thinking_started = true;
                    self.thinking_index = self.next_content_index;
                    self.next_content_index += 1;
                    out.push(json!({
                        "type": "message_update",
                        "assistantMessageEvent": { "type": "thinking_start", "contentIndex": self.thinking_index }
                    }));
                }
                self.thinking.push_str(text);
                out.push(json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "thinking_delta",
                        "contentIndex": self.thinking_index,
                        "delta": text,
                    }
                }));
            }
            Event::AssistantText { text } => {
                if self.saw_delta && self.streaming {
                    // The deltas already delivered the text; this is the
                    // authoritative value for the final message.
                    self.text = text.clone();
                } else {
                    self.begin_message(&mut out);
                    if !self.text_started {
                        self.text_started = true;
                        self.text_index = self.next_content_index;
                        self.next_content_index += 1;
                        out.push(json!({
                            "type": "message_update",
                            "assistantMessageEvent": { "type": "text_start", "contentIndex": self.text_index }
                        }));
                    }
                    self.text = text.clone();
                    out.push(json!({
                        "type": "message_update",
                        "assistantMessageEvent": {
                            "type": "text_delta",
                            "contentIndex": self.text_index,
                            "delta": text,
                        }
                    }));
                }
            }
            Event::ToolStart {
                tool_call_id,
                name,
                input,
            } => {
                self.begin_message(&mut out);
                let index = self.next_content_index;
                self.next_content_index += 1;
                self.tool_calls
                    .push((tool_call_id.clone(), name.clone(), input.clone()));
                out.push(json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "toolcall_start",
                        "contentIndex": index,
                        "id": tool_call_id,
                        "toolName": name,
                    }
                }));
                out.push(json!({
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "toolcall_end",
                        "contentIndex": index,
                        "toolCall": { "id": tool_call_id, "name": name, "arguments": input }
                    }
                }));
                out.push(json!({
                    "type": "tool_execution_start",
                    "toolCallId": tool_call_id,
                    "toolName": name,
                    "args": input,
                }));
            }
            Event::ToolEnd {
                tool_call_id,
                name,
                is_error,
                content,
            } => {
                // Buffer the result for this turn's `turn_end`, but keep the
                // `tool_execution_end` envelope unchanged.
                self.tool_results.push(json!({
                    "toolCallId": tool_call_id,
                    "toolName": name,
                    "isError": is_error,
                    "content": [{ "type": "text", "text": content }],
                    "details": {},
                }));
                out.push(json!({
                    "type": "tool_execution_end",
                    "toolCallId": tool_call_id,
                    "toolName": name,
                    "isError": is_error,
                    // pi's result carries `details` even when empty.
                    "result": { "content": [{ "type": "text", "text": content }], "details": {} },
                }));
            }
            Event::TurnEnd => {
                let tool_results = std::mem::take(&mut self.tool_results);
                if self.streaming {
                    let message = self.assistant_message();
                    out.push(json!({ "type": "message_end", "message": message }));
                    out.push(json!({
                        "type": "turn_end",
                        "message": message,
                        "toolResults": tool_results,
                    }));
                    self.streaming = false;
                    self.text_started = false;
                } else {
                    out.push(json!({
                        "type": "turn_end",
                        "message": Value::Null,
                        "toolResults": tool_results,
                    }));
                }
            }
            Event::Done { stop_reason } => out.push(json!({
                "type": "agent_end",
                "messages": [],
                "willRetry": false,
                "stopReason": stop_reason,
            })),
            Event::AgentSettled => out.push(json!({ "type": "agent_settled" })),
            Event::Usage {
                input,
                output,
                cache_read,
                cache_write,
            } => {
                self.last_usage = Some(json!({
                    "input": input,
                    "output": output,
                    "cacheRead": cache_read,
                    "cacheWrite": cache_write,
                }));
            }
            Event::Compacted {
                summary,
                reason,
                tokens_before,
                estimated_tokens_after,
                first_kept_entry_id,
                ..
            } => {
                out.push(json!({
                    "type": "compaction_end",
                    "reason": reason,
                    "result": {
                        "summary": summary,
                        "firstKeptEntryId": first_kept_entry_id,
                        "tokensBefore": tokens_before,
                        "estimatedTokensAfter": estimated_tokens_after,
                        "details": {},
                    },
                    "aborted": false,
                    "willRetry": false,
                }));
            }
            // Pass extension UI through; a UI service answers it directly.
            Event::UiRequest {
                id,
                kind,
                prompt,
                options,
            } => {
                // pi's dialog shapes differ by method: `confirm` carries a
                // `message`, `select` carries `options`, and so on.
                let mut request = json!({
                    "type": "extension_ui_request",
                    "id": id,
                    "method": kind,
                });
                match kind.as_str() {
                    "confirm" => {
                        request["title"] = json!(prompt);
                        request["message"] = json!(prompt);
                    }
                    "select" => {
                        request["title"] = json!(prompt);
                        request["options"] = json!(options);
                    }
                    "notify" => {
                        request["message"] = json!(prompt);
                    }
                    _ => {
                        request["title"] = json!(prompt);
                    }
                }
                out.push(request);
            }
            // Responses, state, ready, and errors already use a stable envelope.
            Event::Ready { .. }
            | Event::Response { .. }
            | Event::State { .. }
            | Event::Error { .. } => {}
        }
        out
    }
}

/// Translate a whole native event sequence (convenience for tests/gateways).
pub fn translate_all(events: &[Event]) -> Vec<Value> {
    let mut adapter = PiEventAdapter::new();
    let mut out = Vec::new();
    for event in events {
        out.extend(adapter.translate(event));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn types(events: &[Value]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| event.get("type").and_then(Value::as_str))
            .collect()
    }

    #[test]
    fn streamed_deltas_are_not_duplicated_by_the_final_text() {
        let native = vec![
            Event::TurnStart,
            Event::AssistantDelta { text: "Hel".into() },
            Event::AssistantDelta { text: "lo".into() },
            // The authoritative full text arrives after the deltas.
            Event::AssistantText {
                text: "Hello".into(),
            },
            Event::TurnEnd,
        ];
        let events = translate_all(&native);
        let deltas: Vec<&Value> = events
            .iter()
            .filter(|event| event["assistantMessageEvent"]["type"] == json!("text_delta"))
            .collect();
        assert_eq!(deltas.len(), 2, "{events:?}");
        assert_eq!(deltas[0]["assistantMessageEvent"]["delta"], json!("Hel"));
        assert_eq!(deltas[1]["assistantMessageEvent"]["delta"], json!("lo"));
        // Deltas of one text block share its content index.
        assert_eq!(deltas[0]["assistantMessageEvent"]["contentIndex"], json!(0));
        assert_eq!(deltas[1]["assistantMessageEvent"]["contentIndex"], json!(0));
        let message_end = events
            .iter()
            .find(|event| event["type"] == json!("message_end"))
            .expect("message_end");
        assert_eq!(message_end["message"]["content"][0]["text"], json!("Hello"));
    }

    #[test]
    fn thinking_deltas_become_thinking_blocks() {
        let native = vec![
            Event::TurnStart,
            Event::ThinkingDelta { text: "hmm".into() },
            Event::AssistantText {
                text: "done".into(),
            },
            Event::TurnEnd,
        ];
        let events = translate_all(&native);
        let message_end = events
            .iter()
            .find(|event| event["type"] == json!("message_end"))
            .expect("message_end");
        assert_eq!(
            message_end["message"]["content"][0]["type"],
            json!("thinking")
        );
        assert_eq!(
            message_end["message"]["content"][0]["thinking"],
            json!("hmm")
        );
        assert_eq!(message_end["message"]["content"][1]["text"], json!("done"));
    }

    #[test]
    fn confirm_ui_requests_carry_a_message() {
        let native = vec![Event::UiRequest {
            id: "ui-1".into(),
            kind: "confirm".into(),
            prompt: "Run bash?".into(),
            options: vec!["allow".into(), "deny".into()],
        }];
        let events = translate_all(&native);
        let request = events
            .iter()
            .find(|event| event["type"] == json!("extension_ui_request"))
            .expect("ui request");
        assert_eq!(request["method"], json!("confirm"));
        assert_eq!(request["title"], json!("Run bash?"));
        // pi's confirm dialog reads `message`; omitting it crashed the client.
        assert_eq!(request["message"], json!("Run bash?"));
    }

    #[test]
    fn a_text_turn_reconstructs_the_pi_lifecycle() {
        let native = vec![
            Event::AgentStart,
            Event::TurnStart,
            Event::AssistantText {
                text: "hello".into(),
            },
            Event::TurnEnd,
            Event::Done {
                stop_reason: Some("end_turn".into()),
            },
            Event::AgentSettled,
        ];
        let events = translate_all(&native);
        assert_eq!(
            types(&events),
            vec![
                "agent_start",
                "turn_start",
                "message_start",
                "message_update",
                "message_update",
                "message_end",
                "turn_end",
                "agent_end",
                "agent_settled",
            ]
        );
        let message_end = events
            .iter()
            .find(|event| event["type"] == json!("message_end"))
            .expect("message_end");
        assert_eq!(message_end["message"]["content"][0]["text"], json!("hello"));
        let delta = events
            .iter()
            .find(|event| event["assistantMessageEvent"]["type"] == json!("text_delta"))
            .expect("delta");
        assert_eq!(delta["assistantMessageEvent"]["delta"], json!("hello"));
    }

    #[test]
    fn turn_end_carries_the_turns_tool_results() {
        let native = vec![
            Event::TurnStart,
            Event::ToolStart {
                tool_call_id: "call-1".into(),
                name: "bash".into(),
                input: json!({ "command": "ls" }),
            },
            Event::ToolEnd {
                tool_call_id: "call-1".into(),
                name: "bash".into(),
                is_error: false,
                content: "ok".into(),
            },
            Event::TurnEnd,
        ];
        let events = translate_all(&native);
        let turn_end = events
            .iter()
            .find(|event| event["type"] == json!("turn_end"))
            .expect("turn_end");
        let results = turn_end["toolResults"].as_array().expect("toolResults");
        assert_eq!(results.len(), 1, "{events:?}");
        assert_eq!(results[0]["toolCallId"], json!("call-1"));
        assert_eq!(results[0]["content"][0]["text"], json!("ok"));
    }

    #[test]
    fn compaction_end_matches_pi_shape() {
        let native = vec![Event::Compacted {
            dropped: 3,
            summary: Some("SUMMARY".into()),
            reason: "threshold".into(),
            tokens_before: 120_000,
            estimated_tokens_after: 30_000,
            first_kept_entry_id: Some("entry-7".into()),
        }];
        let events = translate_all(&native);
        let end = events
            .iter()
            .find(|event| event["type"] == json!("compaction_end"))
            .expect("compaction_end");
        assert_eq!(end["reason"], json!("threshold"));
        assert_eq!(end["aborted"], json!(false));
        assert_eq!(end["willRetry"], json!(false));
        assert_eq!(end["result"]["summary"], json!("SUMMARY"));
        assert_eq!(end["result"]["firstKeptEntryId"], json!("entry-7"));
        assert_eq!(end["result"]["tokensBefore"], json!(120_000));
        assert_eq!(end["result"]["estimatedTokensAfter"], json!(30_000));
        assert!(end["result"].get("details").is_some());
    }

    #[test]
    fn tool_calls_get_ids_and_execution_events() {
        let native = vec![
            Event::TurnStart,
            Event::AssistantText {
                text: "working".into(),
            },
            Event::ToolStart {
                tool_call_id: "call-1".into(),
                name: "bash".into(),
                input: json!({ "command": "ls" }),
            },
            Event::ToolEnd {
                tool_call_id: "call-1".into(),
                name: "bash".into(),
                is_error: false,
                content: "ok".into(),
            },
            Event::TurnEnd,
        ];
        let events = translate_all(&native);
        let start = events
            .iter()
            .find(|event| event["type"] == json!("tool_execution_start"))
            .expect("tool start");
        assert_eq!(start["toolCallId"], json!("call-1"));
        assert_eq!(start["toolName"], json!("bash"));
        let message_end = events
            .iter()
            .find(|event| event["type"] == json!("message_end"))
            .expect("message_end");
        assert_eq!(
            message_end["message"]["content"][1]["type"],
            json!("toolCall")
        );
        assert_eq!(message_end["message"]["content"][1]["id"], json!("call-1"));
        // pi's tool result carries content blocks plus an (empty) `details`.
        let end = events
            .iter()
            .find(|event| event["type"] == json!("tool_execution_end"))
            .expect("tool end");
        assert_eq!(end["result"]["content"][0]["type"], json!("text"));
        assert_eq!(end["result"]["content"][0]["text"], json!("ok"));
        assert!(end["result"].get("details").is_some(), "{end}");
    }
}
