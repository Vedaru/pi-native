use super::*;
use crate::convert::{convert_messages, AssistantBlock, ContentPart, TranscriptMessage};

fn options() -> AnthropicBuildOptions {
    AnthropicBuildOptions {
        cache_retention: CacheRetention::Short,
        supports_long_cache_retention: true,
        supports_cache_control_on_tools: true,
        supports_eager_tool_input_streaming: true,
        strict_tools: false,
        max_tokens: None,
        default_max_tokens: 4096,
        temperature: None,
        reasoning: true,
        force_adaptive_thinking: false,
        thinking: ThinkingOptions {
            enabled: Some(false),
            ..Default::default()
        },
    }
}

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: format!("{name} tool"),
        input_schema: json!({"type": "object", "properties": {}, "required": []}),
    }
}

fn user_text(text: &str) -> AnthropicMessage {
    AnthropicMessage {
        role: "user",
        content: MessageContent::Text(text.to_string()),
    }
}

fn user_blocks(blocks: Vec<ContentBlock>) -> AnthropicMessage {
    AnthropicMessage {
        role: "user",
        content: MessageContent::Blocks(blocks),
    }
}

#[test]
fn system_block_carries_marker() {
    let params = build_anthropic_params(
        "claude".into(),
        "system",
        &[],
        vec![user_text("hi")],
        &options(),
    );
    let system = params.system.unwrap();
    assert_eq!(system.len(), 1);
    assert!(system[0].cache_control.is_some());
}

#[test]
fn empty_system_is_omitted() {
    let params = build_anthropic_params("claude".into(), "", &[], vec![], &options());
    assert!(params.system.is_none());
}

#[test]
fn only_last_tool_is_marked() {
    let params = build_anthropic_params(
        "claude".into(),
        "s",
        &[tool("read"), tool("bash"), tool("edit")],
        vec![],
        &options(),
    );
    let tools = params.tools.unwrap();
    assert!(tools[0].cache_control.is_none());
    assert!(tools[1].cache_control.is_none());
    assert!(tools[2].cache_control.is_some());
}

#[test]
fn tool_markers_suppressed_when_unsupported() {
    let mut opts = options();
    opts.supports_cache_control_on_tools = false;
    let params = build_anthropic_params("claude".into(), "s", &[tool("read")], vec![], &opts);
    assert!(params.tools.unwrap()[0].cache_control.is_none());
}

#[test]
fn bare_string_is_converted_to_marked_block() {
    let mut messages = vec![user_text("hi")];
    let marker = Some(CacheControlEphemeral {
        r#type: "ephemeral",
        ttl: None,
    });
    apply_conversation_cache_breakpoint(&mut messages, &marker);
    match &messages[0].content {
        MessageContent::Blocks(blocks) => match &blocks[0] {
            ContentBlock::Text { cache_control, .. } => assert!(cache_control.is_some()),
            _ => panic!("expected text"),
        },
        _ => panic!("expected blocks"),
    }
}

#[test]
fn trailing_assistant_message_is_not_marked() {
    let params = build_anthropic_params(
        "claude".into(),
        "s",
        &[],
        vec![AnthropicMessage {
            role: "assistant",
            content: MessageContent::Blocks(vec![ContentBlock::Text {
                text: "a".into(),
                cache_control: None,
            }]),
        }],
        &options(),
    );
    match &params.messages[0].content {
        MessageContent::Blocks(blocks) => {
            assert!(!block_has_cache_marker(&blocks[0]));
        }
        _ => panic!("expected blocks"),
    }
}

#[test]
fn tool_result_block_is_marked() {
    let mut messages = vec![user_blocks(vec![ContentBlock::ToolResult {
        tool_use_id: "t1".into(),
        content: MessageContent::Text("ok".into()),
        is_error: false,
        cache_control: None,
    }])];
    let marker = Some(CacheControlEphemeral {
        r#type: "ephemeral",
        ttl: None,
    });
    apply_conversation_cache_breakpoint(&mut messages, &marker);
    match &messages[0].content {
        MessageContent::Blocks(blocks) => {
            assert!(block_has_cache_marker(&blocks[0]));
        }
        _ => panic!("expected blocks"),
    }
}

#[test]
fn none_retention_emits_no_markers_anywhere() {
    let mut opts = options();
    opts.cache_retention = CacheRetention::None;
    let params = build_anthropic_params(
        "claude".into(),
        "s",
        &[tool("read")],
        vec![user_text("hi")],
        &opts,
    );
    assert!(params.system.unwrap()[0].cache_control.is_none());
    assert!(params.tools.unwrap()[0].cache_control.is_none());
    match &params.messages[0].content {
        MessageContent::Text(_) => {}
        MessageContent::Blocks(blocks) => assert!(!block_has_cache_marker(&blocks[0])),
    }
}

#[test]
fn thinking_disabled_on_reasoning_model() {
    let value = resolve_thinking(
        true,
        false,
        &ThinkingOptions {
            enabled: Some(false),
            ..Default::default()
        },
    );
    assert_eq!(value, Some(json!({"type": "disabled"})));
}

#[test]
fn no_thinking_on_non_reasoning_model() {
    let value = resolve_thinking(
        false,
        false,
        &ThinkingOptions {
            enabled: Some(true),
            ..Default::default()
        },
    );
    assert_eq!(value, None);
}

#[test]
fn budget_thinking_shape() {
    let value = resolve_thinking(
        true,
        false,
        &ThinkingOptions {
            enabled: Some(true),
            budget_tokens: Some(2048),
            ..Default::default()
        },
    );
    assert_eq!(
        value,
        Some(json!({"type": "enabled", "budget_tokens": 2048, "display": "summarized"}))
    );
}

fn parity_options(cache_retention: CacheRetention) -> AnthropicBuildOptions {
    AnthropicBuildOptions {
        cache_retention,
        supports_long_cache_retention: true,
        supports_cache_control_on_tools: true,
        supports_eager_tool_input_streaming: true,
        strict_tools: false,
        max_tokens: Some(64_000),
        default_max_tokens: 64_000,
        temperature: None,
        reasoning: true,
        force_adaptive_thinking: false,
        thinking: ThinkingOptions {
            enabled: Some(false),
            ..Default::default()
        },
    }
}

fn parity_tools() -> Vec<ToolSpec> {
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

fn assert_parity(fixture: &str, built: AnthropicParams, label: &str) {
    let expected: Value = serde_json::from_str(fixture).expect("fixture parses");
    let actual = serde_json::to_value(built).expect("params serialize");
    assert_eq!(actual, expected, "{label} differs from pi");
}

/// End-to-end parity against requests captured from pi itself (VED-313).
/// Regenerate fixtures with `./scripts/parity-check.sh`.
#[test]
fn matches_captured_pi_basic() {
    let built = build_anthropic_params(
        "claude-sonnet-4-5".into(),
        "You are pi, a coding agent. Be concise.",
        &parity_tools(),
        vec![user_text("hello")],
        &parity_options(CacheRetention::Short),
    );
    assert_parity(
        include_str!("../../../../harness/fixtures/anthropic-basic.json"),
        built,
        "basic",
    );
}

#[test]
fn matches_captured_pi_long_retention() {
    let built = build_anthropic_params(
        "claude-sonnet-4-5".into(),
        "You are pi, a coding agent. Be concise.",
        &parity_tools(),
        vec![user_text("hello")],
        &parity_options(CacheRetention::Long),
    );
    assert_parity(
        include_str!("../../../../harness/fixtures/anthropic-long-retention.json"),
        built,
        "long-retention",
    );
}

#[test]
fn matches_captured_pi_no_tools() {
    let built = build_anthropic_params(
        "claude-sonnet-4-5".into(),
        "You are pi.",
        &[],
        vec![user_text("hi")],
        &parity_options(CacheRetention::Short),
    );
    assert_parity(
        include_str!("../../../../harness/fixtures/anthropic-no-tools.json"),
        built,
        "no-tools",
    );
}

#[test]
fn matches_captured_pi_tool_use() {
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
    let built = build_anthropic_params(
        "claude-sonnet-4-5".into(),
        "You are pi, a coding agent. Be concise.",
        &parity_tools(),
        convert_messages(&transcript),
        &parity_options(CacheRetention::Short),
    );
    assert_parity(
        include_str!("../../../../harness/fixtures/anthropic-tool-use.json"),
        built,
        "tool-use",
    );
}
