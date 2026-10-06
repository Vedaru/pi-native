//! Swarm inlets and outlets.
//!
//! A unit's **outlet** is its action stream, mirrored one JSON line at a time to
//! `<dir>/<id>.outlet`. Its **inlet** is every peer's `.outlet`: each new line is
//! framed as `<shout from="…" kind="…">…</shout>` and queued as follow-up
//! context for the next turn.
//!
//! Enabled by `PIPELETS_SWARM_DIR`; a unit with no swarm dir is standalone and
//! writes nothing (pi parity). One append per action, one poll per turn — no
//! threads and no new dependencies.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::Event;

/// One unit's view of the swarm.
pub struct Swarm {
    id: String,
    outlet: File,
    dir: PathBuf,
    /// Bytes already consumed from each peer's outlet.
    read: HashMap<PathBuf, u64>,
}

impl Swarm {
    /// Join a swarm when `PIPELETS_SWARM_DIR` names one. `default_id` is the
    /// unit's name when `PIPELETS_UNIT_ID` is unset.
    pub fn from_env(default_id: String) -> Option<Swarm> {
        let dir = PathBuf::from(std::env::var_os("PIPELETS_SWARM_DIR")?);
        let id = std::env::var("PIPELETS_UNIT_ID").unwrap_or(default_id);
        Swarm::join(dir, id)
    }

    pub(crate) fn join(dir: PathBuf, id: String) -> Option<Swarm> {
        std::fs::create_dir_all(&dir).ok()?;
        let outlet = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{id}.outlet")))
            .ok()?;
        Some(Swarm {
            id,
            outlet,
            dir,
            read: HashMap::new(),
        })
    }

    /// Broadcast one action on the outlet. Events that are not actions
    /// (deltas, ready, usage) are ignored; `assistant_text` is the whole reply,
    /// so a turn is one line, not one per token.
    pub fn emit(&mut self, event: &Event) {
        let (kind, text) = match event {
            Event::ToolStart { name, input, .. } => {
                ("tool", format!("{name} {}", compact_value(input)))
            }
            Event::ToolEnd { name, is_error, .. } => (
                "tool_done",
                format!("{name} {}", if *is_error { "failed" } else { "ok" }),
            ),
            Event::AssistantText { text } => ("says", compact(text)),
            Event::Error { message } => ("error", compact(message)),
            _ => return,
        };
        let line = serde_json::json!({ "from": self.id, "kind": kind, "text": text });
        if writeln!(self.outlet, "{line}").is_ok() {
            let _ = self.outlet.flush();
        }
    }

    /// Read every peer's new outlet lines and frame them as shouts. Own outlet
    /// is skipped.
    pub fn poll(&mut self) -> Vec<String> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let own = format!("{}.outlet", self.id);
        let mut shouts = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some(own.as_str()) {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("outlet") {
                continue;
            }
            let offset = self.read.get(&path).copied().unwrap_or(0);
            let Ok(mut file) = File::open(&path) else {
                continue;
            };
            if file.seek(SeekFrom::Start(offset)).is_err() {
                continue;
            }
            let mut consumed = offset;
            for line in BufReader::new(&mut file).lines().map_while(Result::ok) {
                consumed += line.len() as u64 + 1;
                if let Some(shout) = frame(&line) {
                    shouts.push(shout);
                }
            }
            self.read.insert(path, consumed);
        }
        shouts
    }
}

/// Turn one outlet line into the peer-context envelope, or `None` if malformed.
fn frame(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let from = value.get("from")?.as_str()?;
    let kind = value.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let text = value.get("text").and_then(|t| t.as_str()).unwrap_or("");
    Some(format!(
        "<shout from=\"{from}\" kind=\"{kind}\">{text}</shout>"
    ))
}

/// One line, whitespace-collapsed and clamped, for an outlet record.
fn compact_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => compact(text),
        serde_json::Value::Null => String::new(),
        other => compact(&other.to_string()),
    }
}

fn compact(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // Short and clean: enough to know what happened, not the whole reply.
    let mut trimmed: String = collapsed.chars().take(200).collect();
    if collapsed.chars().count() > 200 {
        trimmed.push('…');
    }
    trimmed
}

#[cfg(test)]
#[path = "../tests/unit/swarm.rs"]
mod tests;
