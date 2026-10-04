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
