use super::*;

#[test]
fn parses_text_chunks_and_usage() {
    let mut stream = GoogleStream::new();
    let first =
        stream.handle(r#"{"candidates":[{"content":{"parts":[{"text":"Hel"}],"role":"model"}}]}"#);
    assert_eq!(first, vec![GoogleStreamEvent::TextDelta("Hel".into())]);
    let second = stream.handle(
        r#"{"candidates":[{"content":{"parts":[{"text":"lo"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":5,"cachedContentTokenCount":80}}"#,
    );
    assert!(second.contains(&GoogleStreamEvent::TextDelta("lo".into())));
    assert!(second.contains(&GoogleStreamEvent::Done {
        finish_reason: Some("STOP".into())
    }));
    assert_eq!(stream.usage().input, 20);
    assert_eq!(stream.usage().cache_read, 80);
    assert_eq!(stream.usage().output, 5);
    assert_eq!(stream.usage().cache_hit_rate(), Some(0.8));

    let (text, blocks) = collect_google(&first);
    assert_eq!(text, "Hel");
    assert!(blocks.is_empty());
}

#[test]
fn parses_function_calls() {
    let mut stream = GoogleStream::new();
    let events = stream.handle(
        r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{"path":"x"}}}],"role":"model"}}]}"#,
    );
    let (_, blocks) = collect_google(&events);
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        ContentBlock::ToolUse { name, input, .. } => {
            assert_eq!(name, "read");
            assert_eq!(input["path"], json!("x"));
        }
        _ => panic!("expected tool use"),
    }
}

#[test]
fn invalid_json_is_other() {
    let mut stream = GoogleStream::new();
    assert_eq!(stream.handle("{nope"), vec![GoogleStreamEvent::Other]);
}
