//! Swarm inlets and outlets.
//!
//! A unit's **outlet** is its action stream, mirrored one JSON line at a time to
//! `<dir>/<id>.outlet`. Its **inlet** is every peer's `.outlet`; new lines are
//! summarised into one short `<shouts>` block and queued as follow-up context
//! for the next turn.
//!
//! Enabled by `PIPELETS_SWARM_DIR`; a unit with no swarm dir is standalone and
//! writes nothing (pi parity). One append per action, one poll per turn — no
//! threads and no new dependencies.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::Event;

/// Most peer lines folded into one heard block; older ones collapse to a count
/// so a burst of activity cannot flood the context window.
const MAX_HEARD: usize = 10;

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

    /// Broadcast one action on the outlet. Reads, successful tool completions,
    /// and streaming deltas are not shouted; `assistant_text` is the whole
    /// reply, so a turn is one line, not one per token.
    pub fn emit(&mut self, event: &Event) {
        let (kind, text) = match event {
            Event::ToolStart { name, input, .. } => {
                if !worth_shouting(name) {
                    return;
                }
                ("tool", tool_summary(name, input))
            }
            Event::ToolEnd { name, is_error, .. } if *is_error => {
                ("error", format!("{name} failed"))
            }
            Event::ToolEnd { .. } => return,
            Event::AssistantText { text } => ("says", compact(text, 160)),
            Event::Error { message } => ("error", compact(message, 160)),
            _ => return,
        };
        let line = serde_json::json!({ "from": self.id, "kind": kind, "text": text });
        if writeln!(self.outlet, "{line}").is_ok() {
            let _ = self.outlet.flush();
        }
    }

    /// Read every peer's new outlet lines and fold them into one short, clean
    /// `<shouts>` block, or `None` when there is nothing new.
    pub fn poll(&mut self) -> Option<String> {
        let mut heard = self.new_peer_lines();
        if heard.is_empty() {
            return None;
        }
        let elided = heard.len().saturating_sub(MAX_HEARD);
        let tail = heard.split_off(elided.min(heard.len()));
        let mut block = String::from("<shouts>\n");
        if elided > 0 {
            block.push_str(&format!("({elided} earlier peer action(s) omitted)\n"));
        }
        let mut last: Option<&str> = None;
        for line in &tail {
            if last == Some(line.as_str()) {
                continue; // collapse an exact repeat
            }
            block.push_str(line);
            block.push('\n');
            last = Some(line.as_str());
        }
        block.push_str("</shouts>");
        Some(block)
    }

    fn new_peer_lines(&mut self) -> Vec<String> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        let own = format!("{}.outlet", self.id);
        let mut lines = Vec::new();
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
                if let Some(line) = heard_line(&line) {
                    lines.push(line);
                }
            }
            self.read.insert(path, consumed);
        }
        lines
    }
}

/// Turn one outlet line into `[unit] kind: detail`, or `None` if malformed.
fn heard_line(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let from = value.get("from")?.as_str()?;
    let kind = value.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    let text = value.get("text").and_then(|t| t.as_str()).unwrap_or("");
    Some(format!("[{from}] {kind}: {text}"))
}

/// Tools that change the workspace; a read or a search is peer noise.
fn worth_shouting(name: &str) -> bool {
    matches!(
        name,
        "bash" | "edit" | "write" | "apply_patch" | "create" | "delete" | "move"
    )
}

/// A tool call in the few words that matter: the command or the path.
fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let field = |key: &str| {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .map(|v| compact(v, 100))
    };
    let detail = match name {
        "bash" => field("command"),
        "edit" | "write" | "create" | "delete" | "apply_patch" => {
            field("path").or_else(|| field("file_path").or_else(|| field("file")))
        }
        "move" => field("to"),
        _ => None,
    };
    match detail.filter(|d| !d.is_empty()) {
        Some(detail) => format!("{name} {detail}"),
        None => name.to_string(),
    }
}

/// Whitespace-collapsed and clamped so a shout stays one short line.
fn compact(text: &str, max: usize) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed: String = collapsed.chars().take(max).collect();
    if collapsed.chars().count() > max {
        trimmed.push('…');
    }
    trimmed
}

#[cfg(test)]
#[path = "../tests/unit/swarm.rs"]
mod tests;
