//! OpenAI-compatible Chat Completions request construction.
//!
//! Mirrors pi `packages/ai/src/api/openai-completions.ts` (`buildParams`,
//! `convertMessages`, `convertTools`, and the `thinkingFormat` branches).
//!
//! Validated today: system + user + function tools on a DeepSeek model
//! (`thinkingFormat: "deepseek"`). Other `thinkingFormat` variants and
//! assistant/tool message conversion are ported only where noted.

use crate::anthropic::ToolSpec;
use crate::convert::{AssistantBlock, ContentPart, TranscriptMessage};
use pi_cache::{openai_completions_prompt_cache_key, CacheRetention};
use serde_json::{json, Map, Value};

/// Which field carries the output cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingFormat {
    Deepseek,
    None,
}

#[derive(Debug, Clone)]
pub struct OpenAiCompletionsBuildOptions {
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    pub base_url_is_openai_api: bool,
    pub supports_long_cache_retention: bool,
    pub supports_usage_in_streaming: bool,
    pub supports_store: bool,
    pub max_tokens_field: MaxTokensField,
    pub supports_developer_role: bool,
    pub supports_strict_mode: bool,
    /// DeepSeek-style compat: assistant messages carry `reasoning_content`.
    pub requires_reasoning_content_on_assistant_messages: bool,
    pub reasoning: bool,
    pub thinking_format: ThinkingFormat,
    pub max_tokens: Option<i64>,
    /// `model.thinkingLevelMap.off !== null`.
    pub off_supported: bool,
    pub reasoning_effort: Option<String>,
}

fn convert_parts(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => json!({ "type": "text", "text": text }),
            ContentPart::Image { data, mime_type } => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime_type};base64,{data}") },
            }),
        })
        .collect()
}

fn convert_tools(tools: &[ToolSpec], supports_strict_mode: bool) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut function = Map::new();
            function.insert("name".into(), json!(tool.name));
            function.insert("description".into(), json!(tool.description));
            function.insert("parameters".into(), tool.input_schema.clone());
            if supports_strict_mode {
                function.insert("strict".into(), json!(false));
            }
            json!({ "type": "function", "function": Value::Object(function) })
        })
        .collect()
}

fn convert_messages(
    messages: &[TranscriptMessage],
    instruction_role: &str,
    system_text: &str,
    requires_reasoning_content: bool,
) -> Vec<Value> {
    let mut out = Vec::new();
    if !system_text.is_empty() {
        out.push(json!({ "role": instruction_role, "content": system_text }));
    }
    for message in messages {
        match message {
            TranscriptMessage::UserText(text) => {
                out.push(json!({ "role": "user", "content": text }));
            }
            TranscriptMessage::UserParts(parts) => {
                let content = convert_parts(parts);
                if !content.is_empty() {
                    out.push(json!({ "role": "user", "content": content }));
                }
            }
            TranscriptMessage::Assistant(blocks) => {
                let mut text: Option<String> = None;
                let mut tool_calls = Vec::new();
                for block in blocks {
                    match block {
                        AssistantBlock::Text { text: t } => {
                            text = Some(match text.take() {
                                Some(existing) => format!("{existing}{t}"),
                                None => t.clone(),
                            });
                        }
                        AssistantBlock::ToolCall {
                            id,
                            name,
                            arguments,
                        } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": arguments.to_string() },
                            }));
                        }
                        AssistantBlock::Thinking { .. } => {}
                    }
                }
                let mut map = Map::new();
                map.insert("role".into(), json!("assistant"));
                map.insert(
                    "content".into(),
                    text.map(Value::String).unwrap_or(Value::Null),
                );
                if requires_reasoning_content {
                    map.insert("reasoning_content".into(), json!(""));
                }
                if !tool_calls.is_empty() {
                    map.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                out.push(Value::Object(map));
            }
            TranscriptMessage::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                let joined = content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        ContentPart::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": joined,
                }));
            }
        }
    }
    out
}

fn resolve_thinking(options: &OpenAiCompletionsBuildOptions) -> Option<Value> {
    if !options.reasoning {
        return None;
    }
    match options.thinking_format {
        ThinkingFormat::Deepseek => {
            if options.reasoning_effort.is_some() {
                Some(json!({ "type": "enabled" }))
            } else if options.off_supported {
                Some(json!({ "type": "disabled" }))
            } else {
                None
            }
        }
        ThinkingFormat::None => None,
    }
}

/// Assemble Chat Completions params as a JSON object (field names vary by compat).
pub fn build_openai_completions_params(
    model: String,
    system_text: &str,
    tools: &[ToolSpec],
    messages: &[TranscriptMessage],
    options: &OpenAiCompletionsBuildOptions,
) -> Value {
    let instruction_role = if options.supports_developer_role {
        "developer"
    } else {
        "system"
    };

    let mut params = Map::new();
    params.insert("model".into(), json!(model));
    params.insert(
        "messages".into(),
        Value::Array(convert_messages(
            messages,
            instruction_role,
            system_text,
            options.requires_reasoning_content_on_assistant_messages,
        )),
    );
    params.insert("stream".into(), json!(true));

    let cache_key = openai_completions_prompt_cache_key(
        options.cache_retention,
        options.session_id.as_deref(),
        options.base_url_is_openai_api,
        options.supports_long_cache_retention,
    );
    if let Some(key) = cache_key {
        params.insert("prompt_cache_key".into(), json!(key));
    }
    if options.cache_retention == CacheRetention::Long && options.supports_long_cache_retention {
        params.insert("prompt_cache_retention".into(), json!("24h"));
    }

    if options.supports_usage_in_streaming {
        params.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    if options.supports_store {
        params.insert("store".into(), json!(false));
    }

    if let Some(max_tokens) = options.max_tokens {
        let field = match options.max_tokens_field {
            MaxTokensField::MaxTokens => "max_tokens",
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
        };
        params.insert(field.into(), json!(max_tokens));
    }

    if !tools.is_empty() {
        params.insert(
            "tools".into(),
            Value::Array(convert_tools(tools, options.supports_strict_mode)),
        );
    }

    if let Some(thinking) = resolve_thinking(options) {
        params.insert("thinking".into(), thinking);
    }

    Value::Object(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn options() -> OpenAiCompletionsBuildOptions {
        OpenAiCompletionsBuildOptions {
            cache_retention: CacheRetention::Short,
            session_id: Some("harness-session".into()),
            base_url_is_openai_api: false,
            supports_long_cache_retention: true,
            supports_usage_in_streaming: true,
            supports_store: false,
            max_tokens_field: MaxTokensField::MaxTokens,
            supports_developer_role: false,
            supports_strict_mode: true,
            requires_reasoning_content_on_assistant_messages: true,
            reasoning: true,
            thinking_format: ThinkingFormat::Deepseek,
            max_tokens: Some(384_000),
            off_supported: true,
            reasoning_effort: None,
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
    /// Regenerate with `node harness/capture.mjs harness/scenarios/openai-completions-basic.json`.
    #[test]
    fn matches_captured_pi_openai_completions_basic() {
        let expected: Value = serde_json::from_str(include_str!(
            "../../../harness/fixtures/openai-completions-basic.json"
        ))
        .expect("fixture parses");
        let actual = build_openai_completions_params(
            "deepseek-flash".into(),
            "You are pi, a coding agent. Be concise.",
            &tools(),
            &[TranscriptMessage::UserText("hello".into())],
            &options(),
        );
        assert_eq!(actual, expected, "openai-completions basic differs from pi");
    }

    #[test]
    fn matches_captured_pi_openai_completions_tool_use() {
        let expected: Value = serde_json::from_str(include_str!(
            "../../../harness/fixtures/openai-completions-tool-use.json"
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
        let actual = build_openai_completions_params(
            "deepseek-flash".into(),
            "You are pi, a coding agent. Be concise.",
            &tools(),
            &transcript,
            &options(),
        );
        assert_eq!(
            actual, expected,
            "openai-completions tool-use differs from pi"
        );
    }

    #[test]
    fn deepseek_thinking_enabled_with_effort() {
        let mut opts = options();
        opts.reasoning_effort = Some("high".into());
        let value = build_openai_completions_params(
            "m".into(),
            "s",
            &[],
            &[TranscriptMessage::UserText("hi".into())],
            &opts,
        );
        assert_eq!(value["thinking"]["type"], json!("enabled"));
    }

    #[test]
    fn prompt_cache_key_only_on_openai_base_url() {
        let value = build_openai_completions_params(
            "m".into(),
            "s",
            &[],
            &[TranscriptMessage::UserText("hi".into())],
            &options(),
        );
        assert!(value.get("prompt_cache_key").is_none());

        let mut opts = options();
        opts.base_url_is_openai_api = true;
        let value = build_openai_completions_params(
            "m".into(),
            "s",
            &[],
            &[TranscriptMessage::UserText("hi".into())],
            &opts,
        );
        assert_eq!(value["prompt_cache_key"], json!("harness-session"));
    }
}
