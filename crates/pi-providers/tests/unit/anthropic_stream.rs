use super::*;

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":900,"cache_creation_input_tokens":200}}}"#;
const TEXT_START: &str =
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
const TEXT_DELTA: &str =
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
const TOOL_START: &str = r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read"}}"#;
const TOOL_DELTA: &str = r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"x\"}"}}"#;
const MESSAGE_DELTA: &str =
    r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":50}}"#;

#[test]
fn parses_usage_and_cache_hit_rate() {
    let mut stream = AnthropicStream::new();
    let event = stream.handle("message_start", MESSAGE_START);
    match event {
        AnthropicStreamEvent::MessageStart { id, usage } => {
            assert_eq!(id.as_deref(), Some("msg_1"));
            assert_eq!(usage.input, 100);
            assert_eq!(usage.cache_read, 900);
            assert_eq!(usage.cache_write, 200);
            // 900 / (100 + 900 + 200) = 0.75
            assert_eq!(usage.cache_hit_rate(), Some(0.75));
        }
        _ => panic!("expected message_start"),
    }
}

#[test]
fn output_tokens_update_on_message_delta() {
    let mut stream = AnthropicStream::new();
    stream.handle("message_start", MESSAGE_START);
    let event = stream.handle("message_delta", MESSAGE_DELTA);
    match event {
        AnthropicStreamEvent::MessageDelta { stop_reason, usage } => {
            assert_eq!(stop_reason.as_deref(), Some("tool_use"));
            assert_eq!(usage.output, 50);
            // prior fields retained
            assert_eq!(usage.cache_read, 900);
        }
        _ => panic!("expected message_delta"),
    }
    assert_eq!(stream.usage().output, 50);
}

#[test]
fn collects_text_and_tool_calls_in_order() {
    let mut stream = AnthropicStream::new();
    let events = [
        stream.handle("message_start", MESSAGE_START),
        stream.handle("content_block_start", TEXT_START),
        stream.handle("content_block_delta", TEXT_DELTA),
        stream.handle("content_block_start", TOOL_START),
        stream.handle("content_block_delta", TOOL_DELTA),
        stream.handle("message_delta", MESSAGE_DELTA),
        stream.handle("message_stop", "{}"),
    ];
    let (blocks, text) = collect_content(&events);
    assert_eq!(text, "Hello");
    assert_eq!(blocks.len(), 2);
    match &blocks[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "toolu_1");
            assert_eq!(name, "read");
            assert_eq!(input["path"], serde_json::json!("x"));
        }
        _ => panic!("expected tool_use"),
    }
}

#[test]
fn error_events_surface_message() {
    let mut stream = AnthropicStream::new();
    let event = stream.handle(
        "error",
        r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
    );
    assert_eq!(
        event,
        AnthropicStreamEvent::Error {
            message: "busy".to_string()
        }
    );
}

#[test]
fn invalid_json_becomes_error() {
    let mut stream = AnthropicStream::new();
    assert!(matches!(
        stream.handle("message_start", "{not json"),
        AnthropicStreamEvent::Error { .. }
    ));
}
