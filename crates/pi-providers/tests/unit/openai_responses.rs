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
        "../../../../harness/fixtures/openai-responses-basic.json"
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
        "../../../../harness/fixtures/openai-responses-tool-use.json"
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
