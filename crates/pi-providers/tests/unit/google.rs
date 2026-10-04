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
            strict: false,
        },
        ToolSpec {
            name: "bash".into(),
            description: "Run a shell command".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }),
            strict: false,
        },
    ]
}

/// Parity against a request captured from pi (VED-313).
/// Regenerate with `node scripts/harness/capture.mjs scripts/harness/scenarios/google-basic.json`.
#[test]
fn matches_captured_pi_google_basic() {
    let expected: Value = serde_json::from_str(include_str!("../fixtures/google-basic.json"))
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
    let expected: Value = serde_json::from_str(include_str!("../fixtures/google-tool-use.json"))
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
