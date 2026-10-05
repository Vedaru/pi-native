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
    new_session_path_with_unit(cwd, None)
}

/// Like [`new_session_path`] but records the owning swarm unit in the header.
pub fn new_session_path_with_unit(cwd: &Path, unit: Option<&str>) -> std::io::Result<PathBuf> {
    new_session_path_in_unit(&agent_dir(), cwd, unit)
}

/// Like [`new_session_path`] but with an explicit agent directory (testable).
pub fn new_session_path_in(agent_dir: &Path, cwd: &Path) -> std::io::Result<PathBuf> {
    new_session_path_in_unit(agent_dir, cwd, None)
}

/// Like [`new_session_path_in`] but records the owning swarm unit.
pub fn new_session_path_in_unit(
    agent_dir: &Path,
    cwd: &Path,
    unit: Option<&str>,
) -> std::io::Result<PathBuf> {
    let dir = session_dir_for(agent_dir, cwd);
    new_session_path_in_dir(&dir, cwd, unit, None)
}

/// Create a new session file in an explicit directory, using pi's layout
/// (`<timestamp>_<id>.jsonl`) with the header id matching the filename stem.
///
/// Callers that place session files themselves (the RPC `new_session`/`clone`)
/// use this so pi and pi-web can address the file by id. `parent_session`
/// records the parent session path in the header (pi's `parentSession`, set by
/// `new_session`/`clone`/`fork`).
pub fn new_session_path_in_dir(
    dir: &Path,
    cwd: &Path,
    unit: Option<&str>,
    parent_session: Option<&str>,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let id = new_id();
    let timestamp = now_iso();
    let file_timestamp = timestamp.replace([':', '.'], "-");
    let path = dir.join(format!("{file_timestamp}_{id}.jsonl"));
    let mut header = session_header(&id, &timestamp, &cwd.to_string_lossy());
    header.parent_session = parent_session.map(str::to_string);
    if let Some(unit) = unit {
        header
            .extra
            .insert("unit".to_string(), serde_json::json!(unit));
    }
    let session = SessionFile {
        header,
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

/// Append a `session_info` entry recording the display name (pi's shape).
fn append_session_info(session: &mut SessionFile, name: &str) -> String {
    let id = new_id();
    let timestamp = now_iso();
    let mut data = serde_json::Map::new();
    data.insert(
        "name".to_string(),
        json!(name.replace(['\r', '\n'], " ").trim()),
    );
    let parent_id = session.entries.last().map(|entry| entry.id.clone());
    session.entries.push(SessionEntry {
        kind: "session_info".to_string(),
        id: id.clone(),
        parent_id,
        timestamp,
        data,
    });
    id
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

/// A content fingerprint for a transcript message: its canonical pi JSON shape
/// without the volatile `timestamp`, so the same message keeps the same identity
/// across turns. Compaction is detected by comparing these (identity), not by
/// counting messages: a turn can drop a span and add at least as many new
/// messages, which a length check would misread as an append.
fn fingerprint(message: &TranscriptMessage) -> String {
    let mut value = message_value(message);
    if let Some(object) = value.as_object_mut() {
        object.remove("timestamp");
    }
    value.to_string()
}

/// One message as it currently exists on disk, keyed by identity.
#[derive(Clone)]
struct Persisted {
    fingerprint: String,
    /// The session entry id this message was written as.
    entry_id: String,
    /// Approximate token size, for the compaction entry's `tokensBefore`.
    tokens: usize,
}

/// The compaction recorded by the most recent rewrite, for callers that report
/// it (the RPC `compact` response).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCompaction {
    /// Id of the `compaction` entry written to the file.
    pub entry_id: String,
    /// The summary the dropped span was replaced with.
    pub summary: String,
    /// The first kept entry, or `None` when nothing after the summary remains.
    pub first_kept_entry_id: Option<String>,
    /// How many messages were replaced by the summary.
    pub dropped: usize,
    /// Approximate tokens in the dropped span.
    pub tokens_before: usize,
}

/// Persists a transcript to a pi session file.
///
/// Appends new messages each turn (O(1) writes); when the transcript is
/// rewritten (compaction, or a context-policy drop), it rewrites the file from
/// the current context and records a `compaction` entry carrying the summary and
/// `firstKeptEntryId`, so the summarized span is not silently lost. The file
/// always reflects what `messages_from_session` would load back.
pub struct SessionJournal {
    path: std::path::PathBuf,
    session: SessionFile,
    /// Identity of every message currently persisted, in order.
    persisted: Vec<Persisted>,
    /// The compaction recorded by the most recent `persist`, if it rewrote.
    last_compaction: Option<RecordedCompaction>,
}

/// The longest common prefix of two slices, by identity.
fn common_prefix(a: &[Persisted], b: &[String]) -> usize {
    let mut count = 0;
    while count < a.len() && count < b.len() && a[count].fingerprint == b[count] {
        count += 1;
    }
    count
}

/// Locate the longest suffix of the old transcript (from `lcp` on) that still
/// appears contiguously in the new fingerprints. Returns `(old_start,
/// new_start)`; `old_start == persisted.len()` means nothing was kept. New
/// messages may follow the kept run, so this searches for the run rather than
/// anchoring on the end.
fn kept_span(persisted: &[Persisted], fingerprints: &[String], lcp: usize) -> (usize, usize) {
    let mut best = (persisted.len(), fingerprints.len());
    let mut old_start = lcp;
    while old_start < persisted.len() {
        let run = persisted.len() - old_start;
        let mut new_start = lcp;
        let mut found = None;
        while new_start + run <= fingerprints.len() {
            if (0..run).all(|offset| {
                persisted[old_start + offset].fingerprint == fingerprints[new_start + offset]
            }) {
                found = Some(new_start);
                break;
            }
            new_start += 1;
        }
        if let Some(new_start) = found {
            best = (old_start, new_start);
            break;
        }
        old_start += 1;
    }
    best
}

/// Fingerprint of an already-serialized pi message value (a session entry's
/// `message` field), matching [`fingerprint`] for the same message.
fn fingerprint_of_value(message: &Value) -> String {
    let mut value = message.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("timestamp");
    }
    value.to_string()
}

/// The text a replacement message contributes as a compaction summary.
fn replacement_summary(replacements: &[TranscriptMessage]) -> String {
    let text = replacements
        .iter()
        .map(|message| match message {
            TranscriptMessage::UserText(text) => text.clone(),
            TranscriptMessage::UserParts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.clone()),
                    ContentPart::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            TranscriptMessage::Assistant(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    AssistantBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            TranscriptMessage::ToolResult { content, .. } => content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.clone()),
                    ContentPart::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.trim().is_empty() {
        "[earlier messages omitted to fit the context window]".to_string()
    } else {
        text
    }
}

impl Persisted {
    /// Rebuild the persisted identity list for a session loaded from disk,
    /// matching transcript messages to the message entries that produced them.
    /// The compaction summary message (which has no message entry) maps to the
    /// latest `compaction` entry.
    fn load(session: &SessionFile, transcript: &[TranscriptMessage]) -> Vec<Persisted> {
        let entries: Vec<(String, String)> = session
            .message_entries()
            .map(|entry| {
                (
                    fingerprint_of_value(entry.message().unwrap_or(&Value::Null)),
                    entry.id.clone(),
                )
            })
            .collect();
        let compaction_id = session
            .entries
            .iter()
            .rev()
            .find(|entry| entry.kind == "compaction")
            .map(|entry| entry.id.clone());
        let mut cursor = 0usize;
        transcript
            .iter()
            .map(|message| {
                let fp = fingerprint(message);
                let matched = (cursor..entries.len()).find(|&index| entries[index].0 == fp);
                let entry_id = match matched {
                    Some(index) => {
                        cursor = index + 1;
                        entries[index].1.clone()
                    }
                    None => compaction_id.clone().unwrap_or_default(),
                };
                Persisted {
                    fingerprint: fp,
                    entry_id,
                    tokens: message.approx_tokens(),
                }
            })
            .collect()
    }
}

impl SessionJournal {
    /// Append and persist a `session_info` entry with the display name.
    pub fn set_name(&mut self, name: &str) -> std::io::Result<()> {
        let before = self.session.entries.len();
        append_session_info(&mut self.session, name);
        let entry = &self.session.entries[before];
        let line = serde_json::to_string(entry).map_err(io_err)?;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    /// The session id from the file header.
    pub fn session_id(&self) -> &str {
        &self.session.header.id
    }

    /// The owning swarm unit recorded in the header, if any.
    pub fn unit(&self) -> Option<&str> {
        self.session.unit()
    }

    /// The working directory recorded in the file header.
    pub fn cwd(&self) -> &str {
        &self.session.header.cwd
    }

    /// The parsed session entries currently held in memory.
    ///
    /// The journal owns the file, so these are authoritative and callers avoid
    /// re-reading and re-serializing the whole file (the RPC `get_entries`,
    /// `get_tree`, and `get_session_stats` paths).
    pub fn entries(&self) -> &[SessionEntry] {
        &self.session.entries
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
        let persisted = Persisted::load(&session, &transcript);
        Ok((
            Self {
                path,
                session,
                persisted,
                last_compaction: None,
            },
            transcript,
        ))
    }

    /// The compaction recorded by the most recent [`Self::persist`], if it
    /// rewrote the transcript.
    pub fn last_compaction(&self) -> Option<&RecordedCompaction> {
        self.last_compaction.as_ref()
    }

    /// The session entry id of the persisted message at `index`, if any.
    ///
    /// A compaction drops a prefix and keeps the tail, so the entry id at the
    /// drop boundary is the `firstKeptEntryId` pi records for the compaction
    /// (the next `persist` writes it). Callers reporting a compaction event use
    /// this before the rewrite happens.
    pub fn entry_id_for_message(&self, index: usize) -> Option<&str> {
        self.persisted
            .get(index)
            .map(|entry| entry.entry_id.as_str())
    }

    /// Record the current transcript. Returns the number of entries written.
    ///
    /// A pure append (the transcript still begins with exactly what is on disk)
    /// writes only the new messages. Any divergence is detected by message
    /// identity, not by comparing lengths: a turn that drops a span and adds at
    /// least as many new messages still rewrites, so the file cannot silently
    /// keep a summarized span it no longer matches. When the rewrite is a
    /// compaction (the divergence introduces a summary in place of the dropped
    /// messages), a `compaction` entry carrying the summary and
    /// `firstKeptEntryId` is written too.
    pub fn persist(&mut self, messages: &[TranscriptMessage]) -> std::io::Result<usize> {
        let fingerprints: Vec<String> = messages.iter().map(fingerprint).collect();
        let lcp = common_prefix(&self.persisted, &fingerprints);

        if lcp == self.persisted.len() {
            // The transcript begins with exactly what is already on disk: append
            // only the new messages.
            if messages.len() == lcp {
                return Ok(0);
            }
            let before = self.session.entries.len();
            append_messages(&mut self.session, &messages[lcp..]);
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&self.path)?;
            for entry in &self.session.entries[before..] {
                let line = serde_json::to_string(entry).map_err(io_err)?;
                writeln!(file, "{line}")?;
            }
            for (offset, entry) in self.session.entries[before..].iter().enumerate() {
                self.persisted.push(Persisted {
                    fingerprint: fingerprints[lcp + offset].clone(),
                    entry_id: entry.id.clone(),
                    tokens: messages[lcp + offset].approx_tokens(),
                });
            }
            return Ok(self.session.entries.len() - before);
        }

        self.rewrite(messages, &fingerprints, lcp)
    }

    /// Rewrite the file for a transcript that diverged from disk, recording a
    /// compaction entry when the divergence summarizes a prefix.
    fn rewrite(
        &mut self,
        messages: &[TranscriptMessage],
        fingerprints: &[String],
        lcp: usize,
    ) -> std::io::Result<usize> {
        // The kept tail is the longest suffix of the old transcript that still
        // appears contiguously in the new one; everything between the common
        // prefix and it was dropped. New messages may follow the kept tail
        // (compaction plus the same turn's output), so anchoring on the end is
        // not enough.
        let (old_i, new_i) = kept_span(&self.persisted, fingerprints, lcp);
        let dropped = old_i.saturating_sub(lcp);
        let tokens_before = self.persisted[lcp..old_i].iter().map(|p| p.tokens).sum();

        // A compaction always summarizes history from the front of the
        // transcript (the agent inserts one summary message at index 0). Only
        // that shape is representable by a single `compaction` entry; a drop in
        // the middle of the transcript is rewritten plainly so the file still
        // matches the in-memory transcript exactly.
        let summarizable = lcp == 0 && new_i > 0 && !messages[..new_i].is_empty();
        let summary = summarizable.then(|| replacement_summary(&messages[..new_i]));

        let mut session = SessionFile {
            header: self.session.header.clone(),
            entries: Vec::new(),
        };
        self.last_compaction = None;
        let mut message_start = 0;
        if let Some(summary) = summary {
            // pi records a retain-none compaction's own id as `firstKeptEntryId`
            // (nothing before it is kept).
            let first_kept_entry_id = self.persisted.get(old_i).map(|p| p.entry_id.clone());
            let entry_id = append_compaction(
                &mut session,
                &summary,
                first_kept_entry_id.as_deref().unwrap_or(""),
                tokens_before,
            );
            let first_kept_entry_id = first_kept_entry_id.or_else(|| Some(entry_id.clone()));
            self.last_compaction = Some(RecordedCompaction {
                entry_id,
                summary,
                first_kept_entry_id,
                dropped,
                tokens_before,
            });
            message_start = new_i;
        }
        append_messages(&mut session, &messages[message_start..]);
        session.write(&self.path).map_err(io_err)?;

        let compaction_id = self.last_compaction.as_ref().map(|c| c.entry_id.clone());
        let message_ids: Vec<String> = session
            .entries
            .iter()
            .filter(|entry| entry.kind == "message")
            .map(|entry| entry.id.clone())
            .collect();
        self.persisted = messages
            .iter()
            .enumerate()
            .map(|(index, message)| Persisted {
                fingerprint: fingerprints[index].clone(),
                entry_id: if index < message_start {
                    compaction_id.clone().unwrap_or_default()
                } else {
                    message_ids[index - message_start].clone()
                },
                tokens: message.approx_tokens(),
            })
            .collect();
        let written = session.entries.len();
        self.session = session;
        Ok(written)
    }
}

#[cfg(test)]
#[path = "../tests/unit/session.rs"]
mod tests;
