//! Bridging the agent transcript and a pi session file.
//!
//! `messages_from_session` seeds an agent from a session's resolved context;
//! `append_messages` persists transcript messages back as session entries. Both
//! speak pi's message shapes (`user`/`assistant`/`toolResult`), so a native turn
//! round-trips through the same files pi writes.

use pi_providers::{AssistantBlock, ContentPart, TranscriptMessage};
use pi_session::{build_context, session_dir_for, SessionEntry, SessionFile, SessionHeader};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// pi's timestamp precision on disk is milliseconds (`Date.toISOString()`).
const ISO_MILLIS: &[time::format_description::FormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
);

fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(ISO_MILLIS)
        .unwrap_or_default()
}

/// The pi agent directory: `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
pub fn agent_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return expand_tilde(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home).join(".pi").join("agent")
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    path
}

/// Create a fresh session file under the pi agent directory and return its path.
///
/// The header is written immediately so its id and timestamp match the file
/// name, which is the layout pi and pi-web expect:
/// `<agent>/sessions/--<encoded-cwd>--/<timestamp>_<id>.jsonl`.
pub fn new_session_path(cwd: &Path) -> std::io::Result<PathBuf> {
    new_session_path_in(&agent_dir(), cwd)
}

/// Like [`new_session_path`] but with an explicit agent directory (testable).
pub fn new_session_path_in(agent_dir: &Path, cwd: &Path) -> std::io::Result<PathBuf> {
    let dir = session_dir_for(agent_dir, cwd);
    std::fs::create_dir_all(&dir)?;
    let id = new_id();
    let timestamp = now_iso();
    let file_timestamp = timestamp.replace([':', '.'], "-");
    let path = dir.join(format!("{file_timestamp}_{id}.jsonl"));
    let session = SessionFile {
        header: session_header(&id, &timestamp, &cwd.to_string_lossy()),
        entries: Vec::new(),
    };
    session.write(&path).map_err(io_err)?;
    Ok(path)
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{:x}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn content_parts(value: &Value) -> Vec<ContentPart> {
    match value {
        Value::String(text) => vec![ContentPart::Text { text: text.clone() }],
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    item.get("text")
                        .and_then(Value::as_str)
                        .map(|text| ContentPart::Text {
                            text: text.to_string(),
                        })
                }
                Some("image") => Some(ContentPart::Image {
                    data: item
                        .get("data")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    mime_type: item
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn assistant_blocks(value: &Value) -> Vec<AssistantBlock> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    item.get("text")
                        .and_then(Value::as_str)
                        .map(|text| AssistantBlock::Text {
                            text: text.to_string(),
                        })
                }
                Some("toolCall") => Some(AssistantBlock::ToolCall {
                    id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: item.get("arguments").cloned().unwrap_or_else(|| json!({})),
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Seed a transcript from a session's resolved context.
pub fn messages_from_session(session: &SessionFile) -> Vec<TranscriptMessage> {
    build_context(session, None)
        .into_iter()
        .filter_map(|context| {
            let message = context.message;
            match message.get("role").and_then(Value::as_str)? {
                "user" => match message.get("content") {
                    Some(Value::String(text)) => Some(TranscriptMessage::UserText(text.clone())),
                    Some(content) => Some(TranscriptMessage::UserParts(content_parts(content))),
                    None => None,
                },
                "assistant" => Some(TranscriptMessage::Assistant(assistant_blocks(
                    message.get("content").unwrap_or(&Value::Null),
                ))),
                "toolResult" => Some(TranscriptMessage::ToolResult {
                    tool_call_id: message
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    tool_name: message
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    content: content_parts(message.get("content").unwrap_or(&Value::Null)),
                    is_error: message
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                }),
                _ => None,
            }
        })
        .collect()
}

fn parts_value(parts: &[ContentPart]) -> Value {
    Value::Array(
        parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => json!({ "type": "text", "text": text }),
                ContentPart::Image { data, mime_type } => {
                    json!({ "type": "image", "data": data, "mimeType": mime_type })
                }
            })
            .collect(),
    )
}

/// pi's session message shape (`{role, content, ...}`) for one transcript message.
pub fn message_value(message: &TranscriptMessage) -> Value {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    match message {
        TranscriptMessage::UserText(text) => {
            json!({ "role": "user", "content": text, "timestamp": timestamp })
        }
        TranscriptMessage::UserParts(parts) => {
            json!({ "role": "user", "content": parts_value(parts), "timestamp": timestamp })
        }
        TranscriptMessage::Assistant(blocks) => {
            let content: Vec<Value> = blocks
                .iter()
                .map(|block| match block {
                    AssistantBlock::Text { text } => json!({ "type": "text", "text": text }),
                    AssistantBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    } => json!({ "type": "toolCall", "id": id, "name": name, "arguments": arguments }),
                    AssistantBlock::Thinking { thinking, .. } => {
                        json!({ "type": "thinking", "thinking": thinking })
                    }
                })
                .collect();
            json!({ "role": "assistant", "content": content, "timestamp": timestamp })
        }
        TranscriptMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            is_error,
        } => json!({
            "role": "toolResult",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "content": parts_value(content),
            "isError": is_error,
            "timestamp": timestamp,
        }),
    }
}

/// Serialize a whole transcript to pi's message shape, for a UI service that
/// wants to render the resolved context.
pub fn transcript_values(messages: &[TranscriptMessage]) -> Vec<Value> {
    messages.iter().map(message_value).collect()
}

/// Append a `compaction` entry (pi's shape: `summary`, `firstKeptEntryId`,
/// `tokensBefore`) so `pi-session` context building drops the summarized span.
pub fn append_compaction(
    session: &mut SessionFile,
    summary: &str,
    first_kept_entry_id: &str,
    tokens_before: usize,
) -> String {
    let id = new_id();
    let timestamp = now_iso();
    let mut data = serde_json::Map::new();
    data.insert("summary".to_string(), json!(summary));
    data.insert("firstKeptEntryId".to_string(), json!(first_kept_entry_id));
    data.insert("tokensBefore".to_string(), json!(tokens_before));
    let parent_id = session.entries.last().map(|entry| entry.id.clone());
    session.entries.push(SessionEntry {
        kind: "compaction".to_string(),
        id: id.clone(),
        parent_id,
        timestamp,
        data,
    });
    id
}

/// Append transcript messages as new session entries, chaining `parentId`.
///
/// Returns the number of entries appended.
pub fn append_messages(session: &mut SessionFile, messages: &[TranscriptMessage]) -> usize {
    let mut parent_id = session.entries.last().map(|entry| entry.id.clone());
    let mut appended = 0;
    for message in messages {
        let id = new_id();
        let timestamp = now_iso();
        let mut data = serde_json::Map::new();
        data.insert("message".to_string(), message_value(message));
        session.entries.push(SessionEntry {
            kind: "message".to_string(),
            id: id.clone(),
            parent_id: parent_id.clone(),
            timestamp,
            data,
        });
        parent_id = Some(id);
        appended += 1;
    }
    appended
}

/// A minimal header for a new native session. Matches pi's header shape
/// (`type: "session"`, version 3) so pi's own tooling (including the web UI's
/// session scanner) recognizes the file.
fn session_header(id: &str, timestamp: &str, cwd: &str) -> SessionHeader {
    SessionHeader {
        kind: "session".to_string(),
        id: id.to_string(),
        timestamp: timestamp.to_string(),
        cwd: cwd.to_string(),
        version: Some(3),
        parent_session: None,
        extra: serde_json::Map::new(),
    }
}

fn io_err(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

/// Persists a transcript to a pi session file.
///
/// Appends new messages each turn (O(1) writes); when compaction shrinks the
/// transcript, it rewrites the file from the current context. The file always
/// reflects what `messages_from_session` would load back.
pub struct SessionJournal {
    path: std::path::PathBuf,
    session: SessionFile,
    persisted: usize,
}

impl SessionJournal {
    /// The session id from the file header.
    pub fn session_id(&self) -> &str {
        &self.session.header.id
    }

    /// The working directory recorded in the file header.
    pub fn cwd(&self) -> &str {
        &self.session.header.cwd
    }

    /// Open an existing session (returning its transcript to seed) or create a
    /// new file with just the header.
    pub fn open(
        path: std::path::PathBuf,
        cwd: &str,
    ) -> std::io::Result<(Self, Vec<TranscriptMessage>)> {
        let (session, transcript) = if path.exists() {
            let session = SessionFile::read(&path).map_err(io_err)?;
            let transcript = messages_from_session(&session);
            (session, transcript)
        } else {
            let id = new_id();
            let timestamp = now_iso();
            let session = SessionFile {
                header: session_header(&id, &timestamp, cwd),
                entries: Vec::new(),
            };
            session.write(&path).map_err(io_err)?;
            (session, Vec::new())
        };
        let persisted = transcript.len();
        Ok((
            Self {
                path,
                session,
                persisted,
            },
            transcript,
        ))
    }

    /// Record the current transcript. Returns the number of new entries.
    pub fn persist(&mut self, messages: &[TranscriptMessage]) -> std::io::Result<usize> {
        if messages.len() < self.persisted {
            // Compaction dropped history: rewrite from the current context.
            let mut session = SessionFile {
                header: self.session.header.clone(),
                entries: Vec::new(),
            };
            let appended = append_messages(&mut session, messages);
            session.write(&self.path).map_err(io_err)?;
            self.session = session;
            self.persisted = messages.len();
            return Ok(appended);
        }

        let new = &messages[self.persisted..];
        if new.is_empty() {
            return Ok(0);
        }
        let before = self.session.entries.len();
        append_messages(&mut self.session, new);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)?;
        for entry in &self.session.entries[before..] {
            let line = serde_json::to_string(entry).map_err(io_err)?;
            writeln!(file, "{line}")?;
        }
        self.persisted = messages.len();
        Ok(self.session.entries.len() - before)
    }
}

#[cfg(test)]
#[path = "../tests/unit/session.rs"]
mod tests;
