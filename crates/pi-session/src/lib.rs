//! pi session store: JSONL parsing and serialization.
//!
//! A session file is a header line followed by entry lines. Entries share a
//! common shape (`type`, `id`, `parentId`, `timestamp`) plus type-specific
//! fields. This store keeps the common fields typed and preserves every other
//! field verbatim, so new entry kinds and new fields round-trip without changes
//! here — the generic approach rather than an enum that must be extended for
//! every entry type.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub mod context;

pub use context::{build_context, ContextMessage};

/// pi's cwd-to-directory encoding for session storage.
///
/// Mirrors `getDefaultSessionDirPath`: strip one leading separator, replace
/// `/`, `\\`, and `:` with `-`, then wrap in `--…--`.
/// `/home/me` becomes `--home-me--`.
pub fn encode_cwd_dir(cwd: &Path) -> String {
    let text = cwd.to_string_lossy();
    let mut chars = text.chars().peekable();
    let mut stripped = String::new();
    if let Some('/' | '\\') = chars.peek() {
        chars.next();
    }
    for ch in chars {
        stripped.push(match ch {
            '/' | '\\' | ':' => '-',
            other => other,
        });
    }
    format!("--{stripped}--")
}

/// The `sessions/<encoded-cwd>` directory under an agent directory.
pub fn session_dir_for(agent_dir: &Path, cwd: &Path) -> PathBuf {
    agent_dir.join("sessions").join(encode_cwd_dir(cwd))
}

#[derive(Debug)]
pub enum SessionError {
    Io(std::io::Error),
    Json(serde_json::Error),
    MissingHeader,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(error) => write!(f, "io: {error}"),
            SessionError::Json(error) => write!(f, "json: {error}"),
            SessionError::MissingHeader => write!(f, "session file has no header line"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(error: std::io::Error) -> Self {
        SessionError::Io(error)
    }
}

impl From<serde_json::Error> for SessionError {
    fn from(error: serde_json::Error) -> Self {
        SessionError::Json(error)
    }
}

/// The first line of a session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(rename = "parentSession", skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// Any header field this version does not know, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A session entry with typed common fields and preserved extra fields.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    pub kind: String,
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: String,
    pub data: Map<String, Value>,
}

impl SessionEntry {
    /// Any field other than the common four.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    /// The `message` field, when this is a message entry.
    pub fn message(&self) -> Option<&Value> {
        self.get("message")
    }

    /// The `usage` field of a message or usage entry.
    pub fn usage(&self) -> Option<&Value> {
        if self.kind == "message" {
            self.message()?.get("usage")
        } else {
            self.get("usage")
        }
    }
}

impl Serialize for SessionEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = self.data.clone();
        map.insert("type".into(), Value::String(self.kind.clone()));
        map.insert("id".into(), Value::String(self.id.clone()));
        if let Some(parent_id) = &self.parent_id {
            map.insert("parentId".into(), Value::String(parent_id.clone()));
        }
        map.insert("timestamp".into(), Value::String(self.timestamp.clone()));
        Value::Object(map).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = Value::deserialize(deserializer)?;
        let mut map = value
            .as_object()
            .cloned()
            .ok_or_else(|| D::Error::custom("session entry must be a JSON object"))?;
        let kind =
            take_string(&mut map, "type").ok_or_else(|| D::Error::custom("entry missing type"))?;
        let id = take_string(&mut map, "id").ok_or_else(|| D::Error::custom("entry missing id"))?;
        let parent_id = take_string(&mut map, "parentId");
        let timestamp = take_string(&mut map, "timestamp")
            .ok_or_else(|| D::Error::custom("entry missing timestamp"))?;
        Ok(SessionEntry {
            kind,
            id,
            parent_id,
            timestamp,
            data: map,
        })
    }
}

fn take_string(map: &mut Map<String, Value>, key: &str) -> Option<String> {
    map.remove(key)
        .and_then(|value| value.as_str().map(str::to_string))
}

/// A parsed session file.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionFile {
    pub header: SessionHeader,
    pub entries: Vec<SessionEntry>,
}

impl SessionFile {
    /// Parse a JSONL session file from a string.
    pub fn parse(input: &str) -> Result<Self, SessionError> {
        Self::parse_reader(input.as_bytes())
    }

    /// Parse from any buffered reader, line by line, so the whole file is never
    /// held in memory as one string.
    pub fn parse_reader<R: std::io::BufRead>(reader: R) -> Result<Self, SessionError> {
        let mut lines = reader.lines();
        let header_line = loop {
            match lines.next() {
                Some(Ok(line)) if !line.trim().is_empty() => break line,
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(SessionError::Io(error)),
                None => return Err(SessionError::MissingHeader),
            }
        };
        let header: SessionHeader = serde_json::from_str(&header_line)?;
        let mut entries = Vec::new();
        for line in lines {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            entries.push(serde_json::from_str(&line)?);
        }
        Ok(SessionFile { header, entries })
    }

    /// Serialize back to JSONL (header first, one entry per line).
    pub fn to_jsonl(&self) -> Result<String, SessionError> {
        let mut out = String::new();
        out.push_str(&serde_json::to_string(&self.header)?);
        out.push('\n');
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry)?);
            out.push('\n');
        }
        Ok(out)
    }

    pub fn read(path: &std::path::Path) -> Result<Self, SessionError> {
        let file = std::fs::File::open(path)?;
        Self::parse_reader(std::io::BufReader::new(file))
    }

    pub fn write(&self, path: &std::path::Path) -> Result<(), SessionError> {
        std::fs::write(path, self.to_jsonl()?)?;
        Ok(())
    }

    /// Entries whose `type` is `message`.
    pub fn message_entries(&self) -> impl Iterator<Item = &SessionEntry> {
        self.entries.iter().filter(|entry| entry.kind == "message")
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
