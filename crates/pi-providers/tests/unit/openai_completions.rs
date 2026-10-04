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
        "../../../../harness/fixtures/openai-completions-basic.json"
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
        "../../../../harness/fixtures/openai-completions-tool-use.json"
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
