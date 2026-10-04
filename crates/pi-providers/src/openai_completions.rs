//! OpenAI-compatible Chat Completions request construction.
//!
//! Mirrors pi `packages/ai/src/api/openai-completions.ts` (`buildParams`,
//! `convertMessages`, `convertTools`, and the `thinkingFormat` branches).
//!
//! Validated today: system + user + function tools on a DeepSeek model
//! (`thinkingFormat: "deepseek"`). Other `thinkingFormat` variants and
//! assistant/tool message conversion are ported only where noted.

use crate::convert::{AssistantBlock, ContentPart, TranscriptMessage};
use crate::types::ToolSpec;
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
            // pi applies strict-mode schema expansion only to tools that prefer
            // it (`constrainedSampling: json_schema`), and only when the model
            // supports strict mode.
            let (parameters, strict) = if supports_strict_mode && tool.strict {
                match make_strict_schema(&tool.input_schema) {
                    Ok(schema) => (schema, true),
                    Err(_) => (tool.input_schema.clone(), false),
                }
            } else {
                (tool.input_schema.clone(), false)
            };
            let mut function = Map::new();
            function.insert("name".into(), json!(tool.name));
            function.insert("description".into(), json!(tool.description));
            function.insert("parameters".into(), parameters);
            if supports_strict_mode {
                function.insert("strict".into(), json!(strict));
            }
            json!({ "type": "function", "function": Value::Object(function) })
        })
        .collect()
}

/// Keys pi rejects in strict schemas.
const UNSUPPORTED_STRICT_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
];

/// Convert a tool schema to the strict subset pi sends with `strict: true`.
///
/// Mirrors `makeStrictJsonSchema`: every object gets `additionalProperties:
/// false`, all properties become required, and optional non-null properties are
/// wrapped in `anyOf: [schema, {"type": "null"}]`.
pub fn make_strict_schema(schema: &Value) -> Result<Value, String> {
    let mut cloned = schema.clone();
    make_node_strict(&mut cloned)?;
    if cloned.get("type").and_then(Value::as_str) != Some("object") {
        return Err("root schema must have type object".to_string());
    }
    Ok(cloned)
}

fn make_node_strict(node: &mut Value) -> Result<(), String> {
    let Some(object) = node.as_object_mut() else {
        return Err("boolean schemas are unsupported".to_string());
    };
    for key in UNSUPPORTED_STRICT_KEYS {
        if object.contains_key(*key) {
            return Err(format!("{key} schemas are unsupported"));
        }
    }
    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".to_string());
        }
        make_node_strict(items)?;
    }
    if let Some(variants) = object.get_mut("anyOf").and_then(Value::as_array_mut) {
        for variant in variants.iter_mut() {
            make_node_strict(variant)?;
        }
    }
    let is_object = object.get("type").and_then(Value::as_str) == Some("object");
    if object.contains_key("properties") && !is_object {
        return Err("properties require type object".to_string());
    }
    if !is_object {
        return Ok(());
    }
    if let Some(additional) = object.get("additionalProperties") {
        if additional != &Value::Bool(false) {
            return Err("schema-valued additionalProperties is unsupported".to_string());
        }
    }
    let property_names: Vec<String> = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    let required: Vec<String> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for (key, property) in properties.iter_mut() {
            make_node_strict(property)?;
            if !required.contains(key) && !schema_allows_null(property) {
                let original = property.clone();
                *property = json!({ "anyOf": [original, { "type": "null" }] });
            }
        }
    }
    object.insert("required".into(), json!(property_names));
    object.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}

fn schema_allows_null(schema: &Value) -> bool {
    if let Some(kind) = schema.get("type") {
        if kind == "null" {
            return true;
        }
        if let Some(types) = kind.as_array() {
            if types.iter().any(|kind| kind == "null") {
                return true;
            }
        }
    }
    schema
        .get("anyOf")
        .and_then(Value::as_array)
        .map(|variants| variants.iter().any(schema_allows_null))
        .unwrap_or(false)
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
#[path = "../tests/unit/openai_completions.rs"]
mod tests;
