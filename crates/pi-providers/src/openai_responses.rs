//! OpenAI Responses request construction.
//!
//! Mirrors pi `packages/ai/src/api/openai-responses.ts` (`buildParams`) and
//! `openai-responses-shared.ts` (`convertResponsesMessages`,
//! `convertResponsesTools`, `convertToolResultOutput`).
//!
//! Validated today: system + user + function tools (the captured basic
//! scenario). Assistant/tool-result conversion is ported but not yet covered by
//! a captured fixture.

use crate::anthropic::ToolSpec;
use crate::convert::{AssistantBlock, ContentPart, TranscriptMessage};
use pi_cache::{openai_responses_prompt_cache_key, CacheRetention};
use serde::Serialize;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Serialize)]
pub struct OpenAiResponsesParams {
    pub model: String,
    pub input: Vec<Value>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_options: Option<Value>,
    pub store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
}

#[derive(Debug, Clone)]
pub struct OpenAiResponsesBuildOptions {
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub supports_long_cache_retention: bool,
    pub supports_explicit_prompt_cache_mode: bool,
    pub supports_strict_mode: bool,
    /// `compat.supportsDeveloperRole !== false`.
    pub supports_developer_role: bool,
    /// `model.reasoning`.
    pub reasoning: bool,
    /// `model.input.includes("image")`.
    pub supports_image_input: bool,
    /// `strict` default from `convertResponsesTools`.
    pub strict: bool,
}

/// `getPromptCacheRetention`.
fn prompt_cache_retention(options: &OpenAiResponsesBuildOptions) -> Option<&'static str> {
    if options.cache_retention == CacheRetention::Long
        && options.supports_long_cache_retention
        && !options.supports_explicit_prompt_cache_mode
    {
        Some("24h")
    } else {
        None
    }
}

/// `getPromptCacheOptions`.
fn prompt_cache_options(options: &OpenAiResponsesBuildOptions) -> Option<Value> {
    if !options.supports_explicit_prompt_cache_mode {
        return None;
    }
    if options.cache_retention == CacheRetention::None {
        return Some(json!({ "mode": "explicit" }));
    }
    if options.cache_retention == CacheRetention::Long && options.supports_long_cache_retention {
        return Some(json!({ "ttl": "30m" }));
    }
    None
}

/// `instructionRole`: developer for reasoning models unless disallowed.
fn instruction_role(options: &OpenAiResponsesBuildOptions) -> &'static str {
    if options.reasoning && options.supports_developer_role {
        "developer"
    } else {
        "system"
    }
}

/// `convertResponsesTools`.
pub fn convert_responses_tools(
    tools: &[ToolSpec],
    supports_strict_mode: bool,
    strict: bool,
) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut map = Map::new();
            map.insert("type".into(), json!("function"));
            map.insert("name".into(), json!(tool.name));
            map.insert("description".into(), json!(tool.description));
            map.insert("parameters".into(), tool.input_schema.clone());
            if supports_strict_mode {
                map.insert("strict".into(), json!(strict));
            }
            Value::Object(map)
        })
        .collect()
}

fn convert_parts(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => json!({ "type": "input_text", "text": text }),
            ContentPart::Image { data, mime_type } => json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{mime_type};base64,{data}"),
            }),
        })
        .collect()
}

/// `convertToolResultOutput` for text-only results (the common case).
fn tool_result_output(parts: &[ContentPart], supports_image_input: bool) -> Value {
    let text_result = parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images: Vec<&ContentPart> = parts
        .iter()
        .filter(|p| matches!(p, ContentPart::Image { .. }))
        .collect();
    if images.is_empty() || !supports_image_input {
        let text = if !text_result.is_empty() {
            text_result
        } else if !images.is_empty() {
            "(see attached image)".to_string()
        } else {
            "(no tool output)".to_string()
        };
        return json!(text);
    }
    let mut out = Vec::new();
    if !text_result.is_empty() {
        out.push(json!({ "type": "input_text", "text": text_result }));
    }
    for image in images {
        if let ContentPart::Image { data, mime_type } = image {
            out.push(json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": format!("data:{mime_type};base64,{data}"),
            }));
        }
    }
    Value::Array(out)
}

fn assistant_items(blocks: &[AssistantBlock], message_index: usize) -> Vec<Value> {
    let mut out = Vec::new();
    let mut text_block_index = 0usize;
    for block in blocks {
        match block {
            AssistantBlock::Text { text } => {
                let fallback = if text_block_index == 0 {
                    format!("msg_pi_{message_index}")
                } else {
                    format!("msg_pi_{message_index}_{text_block_index}")
                };
                text_block_index += 1;
                out.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                    "status": "completed",
                    "id": fallback,
                }));
            }
            AssistantBlock::ToolCall {
                id,
                name,
                arguments,
            } => {
                let (call_id, item_id_raw) = match id.split_once('|') {
                    Some((call, item)) => (call.to_string(), Some(item.to_string())),
                    None => (id.clone(), None),
                };
                let mut map = Map::new();
                map.insert("type".into(), json!("function_call"));
                // Same-model function-call ids must start with "fc_", else omit.
                if let Some(item) = item_id_raw.filter(|s| s.starts_with("fc_")) {
                    map.insert("id".into(), json!(item));
                }
                map.insert("call_id".into(), json!(call_id));
                map.insert("name".into(), json!(name));
                map.insert("arguments".into(), json!(arguments.to_string()));
                out.push(Value::Object(map));
            }
            // Thinking blocks carry an OpenAI reasoning item in their signature;
            // omitted until a captured fixture covers it.
            AssistantBlock::Thinking { .. } => {}
        }
    }
    out
}

/// Build the `input` array from a transcript, matching pi's conversion.
pub fn convert_responses_input(
    messages: &[TranscriptMessage],
    options: &OpenAiResponsesBuildOptions,
) -> Vec<Value> {
    let mut out = Vec::new();
    let mut message_index = 0usize;
    let mut index = 0usize;
    while index < messages.len() {
        match &messages[index] {
            TranscriptMessage::UserText(text) => {
                out.push(json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": text }],
                }));
            }
            TranscriptMessage::UserParts(parts) => {
                let content = convert_parts(parts);
                if !content.is_empty() {
                    out.push(json!({ "role": "user", "content": content }));
                }
            }
            TranscriptMessage::Assistant(blocks) => {
                out.extend(assistant_items(blocks, message_index));
            }
            TranscriptMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                let call_id = tool_call_id
                    .split('|')
                    .next()
                    .unwrap_or(tool_call_id)
                    .to_string();
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": tool_result_output(content, options.supports_image_input),
                }));
            }
        }
        message_index += 1;
        index += 1;
    }
    out
}

/// Assemble OpenAI Responses params with pi's cache semantics.
pub fn build_openai_responses_params(
    model: String,
    system_text: &str,
    tools: &[ToolSpec],
    messages: &[TranscriptMessage],
    options: &OpenAiResponsesBuildOptions,
) -> OpenAiResponsesParams {
    let mut input = Vec::new();
    if !system_text.is_empty() {
        input.push(json!({ "role": instruction_role(options), "content": system_text }));
    }
    input.extend(convert_responses_input(messages, options));

    let tools = if tools.is_empty() {
        None
    } else {
        Some(convert_responses_tools(
            tools,
            options.supports_strict_mode,
            options.strict,
        ))
    };

    OpenAiResponsesParams {
        model,
        input,
        stream: true,
        prompt_cache_key: openai_responses_prompt_cache_key(
            options.cache_retention,
            options.session_id.as_deref(),
        ),
        prompt_cache_retention: prompt_cache_retention(options),
        prompt_cache_options: prompt_cache_options(options),
        store: false,
        tools,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn options() -> OpenAiResponsesBuildOptions {
        OpenAiResponsesBuildOptions {
            cache_retention: CacheRetention::Short,
            session_id: Some("harness-session".into()),
            supports_long_cache_retention: true,
            supports_explicit_prompt_cache_mode: false,
            supports_strict_mode: true,
            supports_developer_role: true,
            reasoning: true,
            supports_image_input: true,
            strict: false,
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
    /// Regenerate with `node harness/capture.mjs harness/scenarios/openai-responses-basic.json`.
    #[test]
    fn matches_captured_pi_openai_responses_basic() {
        let expected: Value = serde_json::from_str(include_str!(
            "../../../harness/fixtures/openai-responses-basic.json"
        ))
        .expect("fixture parses");

        let built = build_openai_responses_params(
            "gpt-5".into(),
            "You are pi, a coding agent. Be concise.",
            &tools(),
            &[TranscriptMessage::UserText("hello".into())],
            &options(),
        );
        let actual = serde_json::to_value(built).expect("params serialize");
        assert_eq!(actual, expected, "openai-responses basic differs from pi");
    }

    /// Parity for the multi-turn tool-use path (assistant function_call +
    /// function_call_output).
    #[test]
    fn matches_captured_pi_openai_responses_tool_use() {
        let expected: Value = serde_json::from_str(include_str!(
            "../../../harness/fixtures/openai-responses-tool-use.json"
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
                content: vec![ContentPart::Text {
                    text: "{\"name\":\"x\"}".into(),
                }],
                is_error: false,
            },
            TranscriptMessage::UserText("summarize it".into()),
        ];
        let built = build_openai_responses_params(
            "gpt-5".into(),
            "You are pi, a coding agent. Be concise.",
            &tools(),
            &transcript,
            &options(),
        );
        let actual = serde_json::to_value(built).expect("params serialize");
        assert_eq!(
            actual, expected,
            "openai-responses tool-use differs from pi"
        );
    }

    #[test]
    fn developer_role_falls_back_to_system_for_non_reasoning_models() {
        let mut opts = options();
        opts.reasoning = false;
        let built = build_openai_responses_params(
            "gpt-4o".into(),
            "sys",
            &[],
            &[TranscriptMessage::UserText("hi".into())],
            &opts,
        );
        assert_eq!(built.input[0]["role"], json!("system"));
    }

    #[test]
    fn prompt_cache_key_omitted_when_caching_off() {
        let mut opts = options();
        opts.cache_retention = CacheRetention::None;
        let built = build_openai_responses_params(
            "gpt-5".into(),
            "s",
            &[],
            &[TranscriptMessage::UserText("hi".into())],
            &opts,
        );
        assert!(built.prompt_cache_key.is_none());
    }

    #[test]
    fn long_retention_sets_24h() {
        let mut opts = options();
        opts.cache_retention = CacheRetention::Long;
        let built = build_openai_responses_params(
            "gpt-5".into(),
            "s",
            &[],
            &[TranscriptMessage::UserText("hi".into())],
            &opts,
        );
        assert_eq!(built.prompt_cache_retention, Some("24h"));
    }
}
