//! Google Generative AI (Gemini) request construction.
//!
//! Mirrors pi `packages/ai/src/api/google-generative-ai.ts` (`buildParams`) and
//! `google-shared.ts` (`convertTools`). The captured fixture is the SDK's
//! serialized body, so this builder emits the flattened wire shape
//! (`contents`, `generationConfig`, `systemInstruction`, `tools`).
//!
//! Validated today: system + user text + function tools, thinking disabled.

use crate::anthropic::ToolSpec;
use crate::convert::{AssistantBlock, ContentPart, TranscriptMessage};
use serde::Serialize;
use serde_json::{json, Map, Value};

/// A Gemini request: the model id (carried in the URL path) plus the body.
#[derive(Debug, Clone)]
pub struct GoogleParams {
    pub model: String,
    pub body: Value,
}

impl Serialize for GoogleParams {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Only the body goes over the wire; the model is in the path.
        self.body.serialize(serializer)
    }
}

#[derive(Debug, Clone)]
pub struct GoogleBuildOptions {
    pub max_tokens: Option<i64>,
    pub reasoning: bool,
    /// Thinking explicitly disabled (`{"thinkingBudget":0}`).
    pub thinking_disabled: bool,
}

fn parts_from(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => json!({ "text": text }),
            ContentPart::Image { data, mime_type } => json!({
                "inlineData": { "mimeType": mime_type, "data": data },
            }),
        })
        .collect()
}

fn convert_tools(tools: &[ToolSpec]) -> Value {
    let declarations: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "parametersJsonSchema": tool.input_schema,
            })
        })
        .collect();
    json!([{ "functionDeclarations": declarations }])
}

/// Convert transcript messages to Gemini `contents`.
pub fn convert_google_contents(messages: &[TranscriptMessage]) -> Vec<Value> {
    let mut out = Vec::new();
    for message in messages {
        match message {
            TranscriptMessage::UserText(text) => {
                out.push(json!({ "role": "user", "parts": [{ "text": text }] }));
            }
            TranscriptMessage::UserParts(parts) => {
                let parts = parts_from(parts);
                if !parts.is_empty() {
                    out.push(json!({ "role": "user", "parts": parts }));
                }
            }
            TranscriptMessage::Assistant(blocks) => {
                let parts: Vec<Value> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        AssistantBlock::Text { text } => Some(json!({ "text": text })),
                        AssistantBlock::ToolCall {
                            name, arguments, ..
                        } => Some(json!({
                            "functionCall": { "name": name, "args": arguments }
                        })),
                        AssistantBlock::Thinking { .. } => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    out.push(json!({ "role": "model", "parts": parts }));
                }
            }
            TranscriptMessage::ToolResult {
                tool_name, content, ..
            } => {
                let texts: Vec<&str> = content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        ContentPart::Image { .. } => None,
                    })
                    .collect();
                out.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "name": tool_name,
                            "response": { "output": texts.join("\n") },
                        }
                    }],
                }));
            }
        }
    }
    out
}

/// Assemble the Gemini `generateContent` request body.
pub fn build_google_params(
    model: String,
    system_text: &str,
    tools: &[ToolSpec],
    messages: &[TranscriptMessage],
    options: &GoogleBuildOptions,
) -> GoogleParams {
    let mut body = Map::new();
    body.insert(
        "contents".into(),
        Value::Array(convert_google_contents(messages)),
    );

    if !system_text.is_empty() {
        body.insert(
            "systemInstruction".into(),
            json!({ "parts": [{ "text": system_text }], "role": "user" }),
        );
    }

    let mut generation = Map::new();
    if let Some(max_tokens) = options.max_tokens {
        generation.insert("maxOutputTokens".into(), json!(max_tokens));
    }
    if options.reasoning && options.thinking_disabled {
        generation.insert("thinkingConfig".into(), json!({ "thinkingBudget": 0 }));
    }
    if !generation.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation));
    }

    if !tools.is_empty() {
        body.insert("tools".into(), convert_tools(tools));
    }

    // `model` is carried in the URL for Gemini, not the body.
    GoogleParams {
        model,
        body: Value::Object(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn options() -> GoogleBuildOptions {
        GoogleBuildOptions {
            max_tokens: Some(65_536),
            reasoning: true,
            thinking_disabled: true,
        }
    }

    fn tools() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            },
            ToolSpec {
                name: "bash".into(),
                description: "Run a shell command".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"]
                }),
            },
        ]
    }

    /// Parity against a request captured from pi (VED-313).
    /// Regenerate with `node harness/capture.mjs harness/scenarios/google-basic.json`.
    #[test]
    fn matches_captured_pi_google_basic() {
        let expected: Value =
            serde_json::from_str(include_str!("../../../harness/fixtures/google-basic.json"))
                .expect("fixture parses");
        let actual = serde_json::to_value(build_google_params(
            "gemini-2.5-flash".into(),
            "You are pi, a coding agent. Be concise.",
            &tools(),
            &[TranscriptMessage::UserText("hello".into())],
            &options(),
        ))
        .expect("serializes");
        assert_eq!(actual, expected, "google basic differs from pi");
    }

    /// Parity for the multi-turn tool-use path (functionCall / functionResponse).
    #[test]
    fn matches_captured_pi_google_tool_use() {
        let expected: Value = serde_json::from_str(include_str!(
            "../../../harness/fixtures/google-tool-use.json"
        ))
        .expect("fixture parses");
        let transcript = vec![
            TranscriptMessage::UserText("read package.json".into()),
            TranscriptMessage::Assistant(vec![AssistantBlock::ToolCall {
                id: "toolu_1".into(),
                name: "read".into(),
                arguments: json!({"path": "package.json"}),
            }]),
            TranscriptMessage::ToolResult {
                tool_call_id: "toolu_1".into(),
                tool_name: "read".into(),
                content: vec![ContentPart::Text {
                    text: "{\"name\":\"x\"}".into(),
                }],
                is_error: false,
            },
            TranscriptMessage::UserText("summarize it".into()),
        ];
        let actual = serde_json::to_value(build_google_params(
            "gemini-2.5-flash".into(),
            "You are pi, a coding agent. Be concise.",
            &tools(),
            &transcript,
            &options(),
        ))
        .expect("serializes");
        assert_eq!(actual, expected, "google tool-use differs from pi");
    }

    #[test]
    fn thinking_config_omitted_when_enabled() {
        let mut opts = options();
        opts.thinking_disabled = false;
        let value = build_google_params(
            "m".into(),
            "s",
            &[],
            &[TranscriptMessage::UserText("hi".into())],
            &opts,
        )
        .body;
        assert!(value["generationConfig"].get("thinkingConfig").is_none());
    }
}
