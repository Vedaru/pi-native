use super::*;
use pi_providers::{AssistantBlock, ContentPart, TranscriptMessage};
use pi_session::{SessionFile, SessionHeader};
use serde_json::json;

fn empty_session() -> SessionFile {
    SessionFile {
        header: SessionHeader {
            kind: "session".into(),
            id: "s1".into(),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
            cwd: "/tmp".into(),
            version: Some(3),
            parent_session: None,
            extra: Default::default(),
        },
        entries: Vec::new(),
    }
}

#[test]
fn round_trips_user_and_assistant() {
    let mut session = empty_session();
    let messages = vec![
        TranscriptMessage::UserText("hello".into()),
        TranscriptMessage::Assistant(vec![AssistantBlock::Text { text: "hi".into() }]),
    ];
    assert_eq!(append_messages(&mut session, &messages), 2);
    assert_eq!(session.entries.len(), 2);
    // parentId chains: the first entry has no parent, the second points at it.
    assert_eq!(session.entries[0].parent_id, None);
    assert_eq!(
        session.entries[1].parent_id.as_deref(),
        Some(session.entries[0].id.as_str())
    );

    let loaded = messages_from_session(&session);
    assert_eq!(loaded.len(), 2);
    assert!(matches!(&loaded[0], TranscriptMessage::UserText(text) if text == "hello"));
    match &loaded[1] {
        TranscriptMessage::Assistant(blocks) => match &blocks[0] {
            AssistantBlock::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected text, got {other:?}"),
        },
        other => panic!("expected assistant, got {other:?}"),
    }
}

#[test]
fn round_trips_a_tool_result() {
    let mut session = empty_session();
    let messages = vec![TranscriptMessage::ToolResult {
        tool_call_id: "call_1".into(),
        tool_name: "bash".into(),
        content: vec![ContentPart::Text {
            text: "hi\n".into(),
        }],
        is_error: false,
    }];
    append_messages(&mut session, &messages);

    let loaded = messages_from_session(&session);
    match &loaded[0] {
        TranscriptMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            is_error,
        } => {
            assert_eq!(tool_call_id, "call_1");
            assert_eq!(tool_name, "bash");
            assert_eq!(
                content,
                &vec![ContentPart::Text {
                    text: "hi\n".into()
                }]
            );
            assert!(!is_error);
        }
        other => panic!("expected tool result, got {other:?}"),
    }
}

#[test]
fn assistant_tool_call_round_trips() {
    let mut session = empty_session();
    append_messages(
        &mut session,
        &[TranscriptMessage::Assistant(vec![
            AssistantBlock::ToolCall {
                id: "call_9".into(),
                name: "read".into(),
                arguments: json!({ "path": "a.txt" }),
            },
        ])],
    );
    let loaded = messages_from_session(&session);
    match &loaded[0] {
        TranscriptMessage::Assistant(blocks) => match &blocks[0] {
            AssistantBlock::ToolCall {
                name, arguments, ..
            } => {
                assert_eq!(name, "read");
                assert_eq!(arguments["path"], json!("a.txt"));
            }
            other => panic!("expected tool call, got {other:?}"),
        },
        other => panic!("expected assistant, got {other:?}"),
    }
}

#[test]
fn compaction_entry_round_trips_through_pi_session() {
    let mut session = empty_session();
    let messages = vec![
        TranscriptMessage::UserText("old".into()),
        TranscriptMessage::Assistant(vec![AssistantBlock::Text {
            text: "old reply".into(),
        }]),
    ];
    append_messages(&mut session, &messages);
    let first_kept = session.entries.last().expect("entry").id.clone();
    append_compaction(&mut session, "SUMMARY", &first_kept, 1234);

    // pi-session's context builder must honor firstKeptEntryId: the summary
    // replaces the summarized span, the kept entry and later ones remain.
    let context = messages_from_session(&session);
    assert!(context.iter().any(|message| {
        matches!(message, TranscriptMessage::UserText(text) if text == "SUMMARY")
    }));
    assert!(context.iter().any(|message| {
        matches!(message, TranscriptMessage::Assistant(blocks) if matches!(&blocks[0], AssistantBlock::Text { text } if text == "old reply"))
    }));
}
