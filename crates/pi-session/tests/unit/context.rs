use super::*;
use crate::SessionFile;

const SAMPLE: &str = include_str!("../fixtures/session-sample.jsonl");

#[test]
fn builds_messages_from_the_leaf_branch() {
    let session = SessionFile::parse(SAMPLE).expect("parses");
    let context = build_context(&session, None);
    assert!(!context.is_empty());
    // Every produced message has a role and matches a message entry.
    let roles: Vec<&str> = context.iter().map(|m| m.role.as_str()).collect();
    assert!(roles
        .iter()
        .all(|role| ["user", "assistant", "toolResult", "system"].contains(role)));
}

#[test]
fn compaction_replaces_summarized_history() {
    let header = header("s1", "/tmp");
    let entries = [
        r#"{"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"role":"user","content":"old 1"}}"#,
        r#"{"type":"message","id":"b","parentId":"a","timestamp":"t","message":{"role":"assistant","content":"old 2"}}"#,
        r#"{"type":"compaction","id":"c","parentId":"b","timestamp":"t","summary":"SUMMARY","firstKeptEntryId":"b","tokensBefore":100}"#,
        r#"{"type":"message","id":"d","parentId":"c","timestamp":"t","message":{"role":"user","content":"new"}}"#,
    ];
    let jsonl = format!(
        "{}\n{}\n",
        serde_json::to_string(&header).unwrap(),
        entries.join("\n")
    );
    let session = SessionFile::parse(&jsonl).expect("parses");
    let context = build_context(&session, None);

    // The summary is first, the earliest summarized entry ("old 1") is gone,
    // the kept entry ("old 2") and later messages remain.
    assert_eq!(context[0].message["content"], json!("SUMMARY"));
    let contents: Vec<&str> = context
        .iter()
        .filter_map(|m| m.message.get("content").and_then(Value::as_str))
        .collect();
    assert!(
        !contents.contains(&"old 1"),
        "summarized entry should be dropped: {contents:?}"
    );
    assert!(
        contents.contains(&"old 2"),
        "kept entry missing: {contents:?}"
    );
    assert!(
        contents.contains(&"new"),
        "post-compaction entry missing: {contents:?}"
    );
}

#[test]
fn context_edit_omits_a_target() {
    let header = header("s1", "/tmp");
    let lines = [
        r#"{"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"role":"user","content":"secret"}}"#,
        r#"{"type":"context_edit","id":"e1","parentId":"a","timestamp":"t","targetId":"a","replacement":null}"#,
    ];
    let jsonl = format!(
        "{}\n{}\n",
        serde_json::to_string(&header).unwrap(),
        lines.join("\n")
    );
    let session = SessionFile::parse(&jsonl).expect("parses");
    let context = build_context(&session, None);
    assert!(
        context
            .iter()
            .all(|m| m.message.get("content") != Some(&json!("secret"))),
        "edited-out target should be omitted: {context:?}"
    );
}

#[test]
fn context_edit_replaces_content() {
    let header = header("s1", "/tmp");
    let lines = [
        r#"{"type":"message","id":"a","parentId":null,"timestamp":"t","message":{"role":"user","content":"original"}}"#,
        r#"{"type":"context_edit","id":"e1","parentId":"a","timestamp":"t","targetId":"a","replacement":{"content":"redacted"}}"#,
    ];
    let jsonl = format!(
        "{}\n{}\n",
        serde_json::to_string(&header).unwrap(),
        lines.join("\n")
    );
    let session = SessionFile::parse(&jsonl).expect("parses");
    let context = build_context(&session, None);
    assert_eq!(context[0].message["content"], json!("redacted"));
}
