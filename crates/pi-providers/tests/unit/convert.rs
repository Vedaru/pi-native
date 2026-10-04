use super::*;

#[test]
fn text_only_tool_result_becomes_string() {
    let parts = vec![
        ContentPart::Text { text: "a".into() },
        ContentPart::Text { text: "b".into() },
    ];
    match convert_content_blocks(&parts) {
        MessageContent::Text(t) => assert_eq!(t, "a\nb"),
        _ => panic!("expected string"),
    }
}

#[test]
fn image_forces_block_array() {
    let parts = vec![
        ContentPart::Text {
            text: "look".into(),
        },
        ContentPart::Image {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
        },
    ];
    match convert_content_blocks(&parts) {
        MessageContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 2);
            assert!(matches!(blocks[1], ContentBlock::Image { .. }));
        }
        _ => panic!("expected blocks"),
    }
}

#[test]
fn consecutive_tool_results_group_into_one_user_message() {
    let messages = vec![
        TranscriptMessage::ToolResult {
            tool_call_id: "a".into(),
            tool_name: "read".into(),
            content: vec![ContentPart::Text { text: "1".into() }],
            is_error: false,
        },
        TranscriptMessage::ToolResult {
            tool_call_id: "b".into(),
            tool_name: "bash".into(),
            content: vec![ContentPart::Text { text: "2".into() }],
            is_error: true,
        },
    ];
    let converted = convert_messages(&messages);
    assert_eq!(converted.len(), 1);
    match &converted[0].content {
        MessageContent::Blocks(blocks) => assert_eq!(blocks.len(), 2),
        _ => panic!("expected blocks"),
    }
}

#[test]
fn empty_assistant_is_dropped() {
    let messages = vec![TranscriptMessage::Assistant(vec![AssistantBlock::Text {
        text: "   ".into(),
    }])];
    assert!(convert_messages(&messages).is_empty());
}
