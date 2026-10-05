use super::*;
use pi_net::StreamResult;
use pi_providers::{ContentBlock, Usage};
use serde_json::json;

fn usage() -> Usage {
    Usage {
        input: 10,
        output: 2,
        ..Default::default()
    }
}

#[test]
fn text_only_turn_stops_normally() {
    let result = StreamResult {
        message_id: Some("msg_1".into()),
        content: vec![ContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
        }],
        text: "hello".into(),
        usage: usage(),
    };
    let turn = turn_from_stream(result);
    assert_eq!(turn.text, "hello");
    assert!(turn.tool_calls.is_empty());
    assert_eq!(turn.stop_reason.as_deref(), Some("end_turn"));
}

#[test]
fn tool_use_becomes_a_tool_call() {
    let result = StreamResult {
        message_id: Some("msg_2".into()),
        content: vec![ContentBlock::ToolUse {
            id: "toolu_1".into(),
            name: "bash".into(),
            input: json!({ "command": "ls" }),
        }],
        text: String::new(),
        usage: usage(),
    };
    let turn = turn_from_stream(result);
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].name, "bash");
    assert_eq!(turn.tool_calls[0].arguments["command"], json!("ls"));
    assert_eq!(turn.stop_reason.as_deref(), Some("tool_use"));
}

#[test]
fn all_providers_share_the_generic_type() {
    // Each factory returns the same `HttpProvider<P>` mechanism with a different
    // protocol; no per-provider adapter type exists.
    fn assert_provider<P: SseProtocol + 'static>(_: &HttpProvider<P>) {}
    assert_provider(&openai_completions_provider(
        "http://x",
        "k",
        "deepseek-flash",
        None,
        ThinkingFormat::None,
        None,
    ));
    assert_provider(&openai_responses_provider("http://x", "k", "gpt-5"));
}

#[test]
fn thinking_level_maps_to_a_reasoning_effort() {
    assert_eq!(level_effort("off"), None);
    assert_eq!(level_effort("minimal").as_deref(), Some("low"));
    assert_eq!(level_effort("low").as_deref(), Some("low"));
    assert_eq!(level_effort("medium").as_deref(), Some("medium"));
    assert_eq!(level_effort("high").as_deref(), Some("high"));
    assert_eq!(level_effort("xhigh").as_deref(), Some("high"));
    assert_eq!(level_effort("max").as_deref(), Some("high"));
}

/// Wire parity: the thinking level is the only switch, and the same level
/// always renders the same request bytes.
#[test]
fn thinking_level_is_the_only_thing_that_changes_the_request() {
    let messages: Vec<pi_providers::TranscriptMessage> = Vec::new();
    let tools: Vec<pi_providers::ToolSpec> = Vec::new();
    let params = |effort: Option<&str>| {
        build_openai_completions_params(
            "deepseek-flash".into(),
            "system",
            &tools,
            &messages,
            &OpenAiCompletionsBuildOptions {
                cache_retention: CacheRetention::Short,
                session_id: None,
                base_url_is_openai_api: false,
                supports_long_cache_retention: true,
                supports_usage_in_streaming: true,
                supports_store: false,
                max_tokens_field: MaxTokensField::MaxTokens,
                supports_developer_role: false,
                supports_strict_mode: true,
                supports_image_input: true,
                requires_reasoning_content_on_assistant_messages: true,
                reasoning: true,
                thinking_format: ThinkingFormat::Deepseek,
                max_tokens: None,
                off_supported: true,
                reasoning_effort: effort.map(str::to_string),
            },
        )
    };

    // The same level renders identical bytes.
    assert_eq!(params(None), params(None));
    assert_eq!(params(Some("high")), params(Some("high")));

    // `off` disables thinking and sends no effort...
    let off = params(None);
    assert_eq!(off.get("thinking"), Some(&json!({ "type": "disabled" })));
    assert_eq!(off.get("reasoning_effort"), None);

    // ...a raised level enables it and sends the mapped effort.
    let high = params(Some("high"));
    assert_eq!(high.get("thinking"), Some(&json!({ "type": "enabled" })));
    assert_eq!(high.get("reasoning_effort"), Some(&json!("high")));
    assert_ne!(off, high);
}
