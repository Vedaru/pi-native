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

fn temp_session_path(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("pi-journal-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    (dir.clone(), dir.join("s.jsonl"))
}

#[test]
fn journal_appends_and_reloads() {
    let (dir, path) = temp_session_path("append");
    let (mut journal, seeded) = SessionJournal::open(path.clone(), "/tmp").expect("open");
    assert!(seeded.is_empty());

    let first = vec![
        TranscriptMessage::UserText("hello".into()),
        TranscriptMessage::Assistant(vec![AssistantBlock::Text { text: "hi".into() }]),
    ];
    assert_eq!(journal.persist(&first).expect("persist"), 2);

    let (mut journal, seeded) = SessionJournal::open(path.clone(), "/tmp").expect("reopen");
    assert_eq!(seeded.len(), 2);
    assert!(matches!(&seeded[0], TranscriptMessage::UserText(text) if text == "hello"));

    // Appending more keeps the earlier entries.
    let mut more = first.clone();
    more.push(TranscriptMessage::UserText("again".into()));
    assert_eq!(journal.persist(&more).expect("persist"), 1);

    let (_, seeded) = SessionJournal::open(path, "/tmp").expect("reopen");
    assert_eq!(seeded.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn journal_rewrites_after_compaction() {
    let (dir, path) = temp_session_path("compact");
    let (mut journal, _) = SessionJournal::open(path.clone(), "/tmp").expect("open");
    let many: Vec<TranscriptMessage> = (0..5)
        .map(|index| TranscriptMessage::UserText(format!("m{index}")))
        .collect();
    journal.persist(&many).expect("persist");

    // A shorter transcript (post-compaction) rewrites the file.
    let compacted = vec![TranscriptMessage::UserText("summary".into())];
    journal.persist(&compacted).expect("persist");
    let (_, seeded) = SessionJournal::open(path, "/tmp").expect("reopen");
    assert_eq!(seeded.len(), 1);
    assert!(matches!(&seeded[0], TranscriptMessage::UserText(text) if text == "summary"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_session_header_matches_pi() {
    let (dir, path) = temp_session_path("header-shape");
    let _ = SessionJournal::open(path.clone(), "/tmp").expect("open");
    // pi (and pi-web's session scanner) only accepts `type: "session"`.
    let first_line = std::fs::read_to_string(&path)
        .expect("read")
        .lines()
        .next()
        .expect("header line")
        .to_string();
    let header: serde_json::Value = serde_json::from_str(&first_line).expect("json header");
    assert_eq!(header["type"], json!("session"));
    assert_eq!(header["version"], json!(3));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_session_path_uses_the_pi_layout() {
    let base = std::env::temp_dir().join(format!("pi-agent-dir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let cwd = std::path::Path::new("/home/me/proj");
    let path = new_session_path_in(&base, cwd).expect("create");
    assert!(
        path.starts_with(base.join("sessions").join("--home-me-proj--")),
        "{path:?}"
    );
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.ends_with(".jsonl"), "{name}");
    let session = pi_session::SessionFile::read(&path).expect("read");
    assert_eq!(session.header.kind, "session");
    assert_eq!(session.header.version, Some(3));
    // Filename id and header id must agree, or pi-web cannot address the session.
    let file_id = name.trim_end_matches(".jsonl").rsplit('_').next().unwrap();
    assert_eq!(session.header.id, file_id);
    let _ = std::fs::remove_dir_all(&base);
}
