use super::*;

#[test]
fn maps_usage_with_cached_tokens_excluded_from_input() {
    let mut stream = OpenAiResponsesStream::new();
    let event = stream.handle(
        "response.completed",
        r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":1000,"output_tokens":50,"total_tokens":1050,"input_tokens_details":{"cached_tokens":800},"output_tokens_details":{"reasoning_tokens":20}}}}"#,
    );
    match event {
        OpenAiResponsesStreamEvent::Completed { usage, .. } => {
            assert_eq!(usage.input, 200);
            assert_eq!(usage.cache_read, 800);
            assert_eq!(usage.output, 50);
            assert_eq!(usage.reasoning, 20);
            assert_eq!(usage.cache_hit_rate(), Some(0.8));
        }
        other => panic!("expected completed, got {other:?}"),
    }
    assert_eq!(stream.message_id(), Some("resp_1"));
}

#[test]
fn accumulates_text_and_tool_calls() {
    let mut stream = OpenAiResponsesStream::new();
    let events = [
        stream.handle("response.created", r#"{"response":{"id":"resp_2"}}"#),
        stream.handle("response.output_text.delta", r#"{"delta":"Let me "}"#),
        stream.handle("response.output_text.delta", r#"{"delta":"check."}"#),
        stream.handle(
            "response.output_item.added",
            r#"{"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read"}}"#,
        ),
        stream.handle(
            "response.function_call_arguments.delta",
            r#"{"call_id":"call_1","delta":"{\"path\":"}"#,
        ),
        stream.handle(
            "response.function_call_arguments.delta",
            r#"{"call_id":"call_1","delta":"\"x\"}"}"#,
        ),
    ];
    let (text, blocks) = collect_response(&events);
    assert_eq!(text, "Let me check.");
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "read");
            assert_eq!(input["path"], serde_json::json!("x"));
        }
        _ => panic!("expected tool use"),
    }
}

#[test]
fn failed_event_surfaces_message() {
    let mut stream = OpenAiResponsesStream::new();
    let event = stream.handle(
        "response.failed",
        r#"{"response":{"error":{"message":"rate limited"}}}"#,
    );
    assert_eq!(
        event,
        OpenAiResponsesStreamEvent::Failed {
            message: "rate limited".to_string()
        }
    );
}
