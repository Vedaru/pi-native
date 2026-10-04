//! Session context building.
//!
//! Mirrors pi's `buildSessionPath` + `buildContextEntries` +
//! `sessionEntryToContextMessages`: walk the current leaf branch, apply the
//! latest compaction (which replaces summarized history with its summary plus
//! the kept entries), and produce the model-facing message list.

use crate::{SessionEntry, SessionFile, SessionHeader};
use serde_json::{json, Value};

/// One model-facing message derived from the session.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextMessage {
    pub role: String,
    pub message: Value,
}

/// The entries on the leaf branch, root-first.
pub fn build_path(session: &SessionFile, leaf_id: Option<&str>) -> Vec<SessionEntry> {
    let leaf = match leaf_id {
        Some(id) => Some(id.to_string()),
        None => session.entries.last().map(|entry| entry.id.clone()),
    };
    let Some(mut current) = leaf else {
        return Vec::new();
    };

    let mut path = Vec::new();
    // Bound the walk by the number of entries so a malformed cycle cannot loop.
    for _ in 0..=session.entries.len() {
        let Some(entry) = session.entries.iter().find(|entry| entry.id == current) else {
            break;
        };
        path.push(entry.clone());
        match &entry.parent_id {
            Some(parent) => current = parent.clone(),
            None => break,
        }
    }
    path.reverse();
    path
}

/// Apply the latest compaction on the path, as pi's `buildContextEntries` does.
pub fn build_context_entries(session: &SessionFile, leaf_id: Option<&str>) -> Vec<SessionEntry> {
    let path = build_path(session, leaf_id);
    let Some(compaction_index) = path.iter().rposition(|entry| entry.kind == "compaction") else {
        return path;
    };
    let compaction = &path[compaction_index];
    let first_kept = compaction
        .get("firstKeptEntryId")
        .and_then(Value::as_str)
        .map(str::to_string);

    let mut context = vec![compaction.clone()];
    let mut found_first_kept = first_kept.is_none();
    for entry in &path[..compaction_index] {
        if Some(entry.id.as_str()) == first_kept.as_deref() {
            found_first_kept = true;
        }
        let is_system_message = entry.kind == "message"
            && entry
                .message()
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                == Some("system");
        if found_first_kept && !is_system_message {
            context.push(entry.clone());
        }
    }
    context.extend_from_slice(&path[compaction_index + 1..]);
    context
}

fn entry_messages(entry: &SessionEntry) -> Vec<ContextMessage> {
    match entry.kind.as_str() {
        "message" => entry
            .message()
            .and_then(|message| {
                message
                    .get("role")
                    .and_then(Value::as_str)
                    .map(|role| ContextMessage {
                        role: role.to_string(),
                        message: message.clone(),
                    })
            })
            .into_iter()
            .collect(),
        "compaction" => entry
            .get("summary")
            .and_then(Value::as_str)
            .map(|summary| ContextMessage {
                role: "user".to_string(),
                message: json!({ "role": "user", "content": summary }),
            })
            .into_iter()
            .collect(),
        "branch_summary" => entry
            .get("summary")
            .and_then(Value::as_str)
            .map(|summary| ContextMessage {
                role: "user".to_string(),
                message: json!({ "role": "user", "content": summary }),
            })
            .into_iter()
            .collect(),
        "custom_message" => entry
            .get("content")
            .map(|content| ContextMessage {
                role: "user".to_string(),
                message: json!({ "role": "user", "content": content }),
            })
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// Build the finalized model context for the leaf branch, applying
/// `context_edit` entries (latest edit for a target wins; a null replacement
/// omits the target from context).
pub fn build_context(session: &SessionFile, leaf_id: Option<&str>) -> Vec<ContextMessage> {
    let entries = build_context_entries(session, leaf_id);

    let mut edits: std::collections::HashMap<&str, Option<&Value>> =
        std::collections::HashMap::new();
    for entry in &entries {
        if entry.kind == "context_edit" {
            if let Some(target) = entry.get("targetId").and_then(Value::as_str) {
                edits.insert(target, entry.get("replacement"));
            }
        }
    }

    entries
        .iter()
        .flat_map(|entry| apply_edit(entry_messages(entry), edits.get(entry.id.as_str()).copied()))
        .collect()
}

/// Apply a `context_edit` replacement to an entry's messages.
fn apply_edit(messages: Vec<ContextMessage>, edit: Option<Option<&Value>>) -> Vec<ContextMessage> {
    let Some(Some(replacement)) = edit else {
        return messages;
    };
    // A JSON null replacement omits the target entirely.
    if replacement.is_null() {
        return Vec::new();
    }
    let Some(content) = replacement.get("content") else {
        return messages;
    };
    messages
        .into_iter()
        .map(|mut message| {
            if let Some(object) = message.message.as_object_mut() {
                object.insert("content".to_string(), content.clone());
            }
            message
        })
        .collect()
}

/// Re-exported for callers constructing sessions in tests.
pub fn header(id: &str, cwd: &str) -> SessionHeader {
    SessionHeader {
        kind: "session".to_string(),
        id: id.to_string(),
        timestamp: "2026-01-01T00:00:00.000Z".to_string(),
        cwd: cwd.to_string(),
        version: Some(3),
        parent_session: None,
        extra: serde_json::Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionFile;

    const SAMPLE: &str = include_str!("../../../harness/fixtures/session-sample.jsonl");

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
}
