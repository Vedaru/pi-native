use super::*;
use pi_providers::{AssistantBlock, ContentPart, TranscriptMessage};
use pi_session::{SessionFile, SessionHeader};
use serde_json::{json, Value};

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
fn journal_records_a_compaction_entry_with_summary_and_first_kept() {
    let (dir, path) = temp_session_path("compaction-entry");
    let (mut journal, _) = SessionJournal::open(path.clone(), "/tmp").expect("open");
    let before: Vec<TranscriptMessage> = (0..4)
        .map(|index| TranscriptMessage::UserText(format!("m{index}")))
        .collect();
    journal.persist(&before).expect("persist");

    // The agent replaces the dropped prefix with one summary message and keeps
    // the last two messages.
    let compacted = vec![
        TranscriptMessage::UserText("SUMMARY".into()),
        before[2].clone(),
        before[3].clone(),
    ];
    journal.persist(&compacted).expect("persist");

    let recorded = journal.last_compaction().expect("recorded compaction");
    assert_eq!(recorded.summary, "SUMMARY");
    assert_eq!(recorded.dropped, 2);
    assert!(recorded.first_kept_entry_id.is_some());

    // The file carries the compaction entry, and reloading yields the summary
    // followed by the kept messages (not a plain rewrite that lost the span).
    let session = SessionFile::read(&path).expect("read");
    let compaction = session
        .entries
        .iter()
        .find(|entry| entry.kind == "compaction")
        .expect("compaction entry");
    assert_eq!(compaction.get("summary"), Some(&json!("SUMMARY")));
    let first_kept = compaction
        .get("firstKeptEntryId")
        .and_then(Value::as_str)
        .expect("firstKeptEntryId");
    assert_eq!(Some(first_kept), recorded.first_kept_entry_id.as_deref());

    let (_, seeded) = SessionJournal::open(path, "/tmp").expect("reopen");
    assert_eq!(seeded.len(), 3);
    assert!(matches!(&seeded[0], TranscriptMessage::UserText(text) if text == "SUMMARY"));
    assert!(matches!(&seeded[1], TranscriptMessage::UserText(text) if text == "m2"));
    assert!(matches!(&seeded[2], TranscriptMessage::UserText(text) if text == "m3"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn journal_detects_compaction_by_identity_not_length() {
    let (dir, path) = temp_session_path("identity");
    let (mut journal, _) = SessionJournal::open(path.clone(), "/tmp").expect("open");
    let before: Vec<TranscriptMessage> = (0..3)
        .map(|index| TranscriptMessage::UserText(format!("m{index}")))
        .collect();
    journal.persist(&before).expect("persist");

    // Compaction drops two messages but the same turn adds three new ones, so
    // the transcript is not shorter. A length check would append and keep the
    // summarized span; identity detection must rewrite and record compaction.
    let compacted = vec![
        TranscriptMessage::UserText("SUMMARY".into()),
        before[2].clone(),
        TranscriptMessage::UserText("n0".into()),
        TranscriptMessage::UserText("n1".into()),
        TranscriptMessage::UserText("n2".into()),
    ];
    assert!(compacted.len() > before.len());
    journal.persist(&compacted).expect("persist");

    let recorded = journal
        .last_compaction()
        .expect("compaction recorded despite growth");
    assert_eq!(recorded.summary, "SUMMARY");
    assert_eq!(recorded.dropped, 2);

    let (_, seeded) = SessionJournal::open(path, "/tmp").expect("reopen");
    assert_eq!(seeded.len(), compacted.len());
    assert!(matches!(&seeded[0], TranscriptMessage::UserText(text) if text == "SUMMARY"));
    assert!(matches!(&seeded[1], TranscriptMessage::UserText(text) if text == "m2"));
    assert!(matches!(&seeded[4], TranscriptMessage::UserText(text) if text == "n2"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn journal_rewrite_matches_the_in_memory_transcript() {
    let (dir, path) = temp_session_path("rewrite-match");
    let (mut journal, _) = SessionJournal::open(path.clone(), "/tmp").expect("open");
    journal
        .persist(&[
            TranscriptMessage::UserText("a".into()),
            TranscriptMessage::UserText("b".into()),
            TranscriptMessage::UserText("c".into()),
        ])
        .expect("persist");

    let rewritten = vec![
        TranscriptMessage::UserText("a".into()),
        TranscriptMessage::UserText("new".into()),
    ];
    journal.persist(&rewritten).expect("persist");

    let (_, seeded) = SessionJournal::open(path, "/tmp").expect("reopen");
    let seeded_text: Vec<String> = seeded
        .iter()
        .map(|message| match message {
            TranscriptMessage::UserText(text) => text.clone(),
            other => panic!("unexpected message: {other:?}"),
        })
        .collect();
    assert_eq!(seeded_text, vec!["a".to_string(), "new".to_string()]);
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
