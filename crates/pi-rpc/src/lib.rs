//! Headless RPC protocol and generic UI events.
//!
//! A unit is driven by JSON lines. Requests are commands; responses are events
//! (`ready`, `assistant_text`, `tool_start`, `tool_end`, `done`, `state`,
//! `response`, `error`) plus **generic UI requests**. The host does not assume a
//! terminal: any client (a small terminal client, a web page, a test harness, or
//! a separate UI service) can drive it.
//!
//! Command responses follow pi's envelope: `{type:"response", id?, command,
//! success, data}`. Session navigation commands (`get_messages`, `get_entries`,
//! `get_tree`, `switch_session`, `new_session`, `set_session_name`) operate on
//! the unit's session file.

use pi_agent::{
    messages_from_session, new_session_path_in_dir, transcript_values, Agent, AgentError,
    AgentEvent, SessionJournal,
};
use pi_session::SessionFile;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod pi_events;

pub use pi_events::{translate_all, PiEventAdapter};

/// A protocol version, so clients can detect incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// A command sent to a unit.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Run a prompt through the agent.
    Prompt {
        #[serde(alias = "message")]
        text: String,
    },
    /// Report current state.
    GetState,
    /// The resolved transcript in pi's message shape.
    GetMessages,
    /// Session entries, optionally only those after `since`.
    GetEntries {
        #[serde(default)]
        since: Option<String>,
    },
    /// The session as a tree of entries.
    GetTree,
    /// The most recent assistant text.
    GetLastAssistantText,
    /// Counts for the current session.
    GetSessionStats,
    /// Start an empty session (a new file when the unit has a session dir).
    NewSession {
        #[serde(default, rename = "parentSession")]
        parent_session: Option<String>,
    },
    /// Load a session file and continue from it.
    SwitchSession {
        #[serde(rename = "sessionPath")]
        session_path: String,
    },
    /// Give the current session a display name.
    SetSessionName {
        name: String,
    },
    /// Generate a short session title from the transcript.
    GenerateTitle,
    /// Queue a steering message (delivered with the next prompt).
    Steer {
        #[serde(alias = "message")]
        text: String,
    },
    /// Queue a follow-up message (delivered with the next prompt).
    FollowUp {
        #[serde(alias = "message")]
        text: String,
    },
    /// Abort the current operation (a unit runs one turn at a time).
    Abort,
    /// Drop queued steering/follow-up messages.
    ClearQueue,
    /// Set the active model (recorded; the provider is fixed at startup).
    SetModel {
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    CycleModel,
    GetAvailableModels,
    SetThinkingLevel {
        level: String,
    },
    CycleThinkingLevel,
    GetAvailableThinkingLevels,
    SetSteeringMode {
        mode: String,
    },
    SetFollowUpMode {
        mode: String,
    },
    /// Force context compaction now.
    Compact {
        #[serde(default, rename = "customInstructions")]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        enabled: bool,
    },
    SetAutoRetry {
        enabled: bool,
    },
    AbortRetry,
    /// Run a shell command out of band.
    Bash {
        command: String,
        #[serde(default, rename = "excludeFromContext")]
        exclude_from_context: bool,
    },
    AbortBash,
    /// Export the session to an HTML file.
    ExportHtml {
        #[serde(default, rename = "outputPath")]
        output_path: Option<String>,
    },
    /// Branch a new session from an entry on the active branch.
    Fork {
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    /// Duplicate the current session at the current position.
    Clone,
    /// User messages that can be forked from.
    GetForkMessages,
    /// Slash commands available for `prompt` (none built in).
    GetCommands,
    /// Answer a `ui_request` emitted earlier.
    UiResponse {
        id: String,
        value: serde_json::Value,
    },
}

/// An event emitted by a unit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ready {
        version: u32,
    },
    /// pi-shaped lifecycle events (see `pi_events` for the adapter).
    AgentStart,
    TurnStart,
    TurnEnd,
    AgentSettled,
    AssistantDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    /// The reply to a command, using pi's envelope.
    Response {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        command: String,
        success: bool,
        data: serde_json::Value,
    },
    AssistantText {
        text: String,
    },
    ToolStart {
        tool_call_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolEnd {
        tool_call_id: String,
        name: String,
        is_error: bool,
        content: String,
    },
    Done {
        stop_reason: Option<String>,
    },
    /// Older messages were compacted into a summary.
    Compacted {
        dropped: usize,
        summary: Option<String>,
        /// Why compaction ran: `manual`, `threshold`, or `overflow`.
        reason: String,
        /// Approximate tokens before compaction.
        tokens_before: usize,
        /// Approximate tokens retained after compaction.
        estimated_tokens_after: usize,
        /// First session entry kept after the summarized span, when known.
        first_kept_entry_id: Option<String>,
    },
    /// Provider token usage for one model call (for cache accounting).
    Usage {
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
    },
    /// The unit's resolved context, so a UI service can render it without
    /// owning the transcript. `transcript` uses pi's message shape.
    State {
        messages: usize,
        system: String,
        transcript: Vec<serde_json::Value>,
    },
    /// A generic UI request any frontend can render. `kind` is one of
    /// `confirm`/`select`/`input`/`notify`; `id` is echoed in `ui_response`.
    UiRequest {
        id: String,
        kind: String,
        prompt: String,
        options: Vec<String>,
    },
    Error {
        message: String,
    },
}

/// Apply one request to an agent with no session file (tests/embedding).
pub fn handle(agent: &mut Agent, request: Request) -> Vec<Event> {
    let mut session = SessionState::empty();
    match request {
        Request::Prompt { text } => {
            agent.push_user(text);
            match agent.run() {
                Ok(events) => events.into_iter().map(from_agent_event).collect(),
                Err(error) => vec![Event::Error {
                    message: error.to_string(),
                }],
            }
        }
        Request::UiResponse { .. } => Vec::new(),
        other => apply_command(agent, &mut session, ".", None, other)
            .into_iter()
            .collect(),
    }
}

/// Handle a non-prompt command. Returns the reply, if the command has one.
fn apply_command(
    agent: &mut Agent,
    session: &mut SessionState,
    cwd: &str,
    id: Option<String>,
    request: Request,
) -> Option<Event> {
    match request {
        Request::Prompt { .. } | Request::UiResponse { .. } => None,
        Request::GetState => Some(state_event(agent)),
        Request::GetMessages => Some(response(
            id,
            "get_messages",
            serde_json::json!({ "messages": transcript_values(agent.messages()) }),
        )),
        Request::GetEntries { since } => Some(response(
            id,
            "get_entries",
            serde_json::json!({ "entries": session.entries(since.as_deref()) }),
        )),
        Request::GetTree => {
            let (tree, leaf_id) = session.tree();
            Some(response(
                id,
                "get_tree",
                serde_json::json!({ "tree": tree, "leafId": leaf_id }),
            ))
        }
        Request::GetLastAssistantText => Some(response(
            id,
            "get_last_assistant_text",
            serde_json::json!({ "text": last_assistant_text(agent) }),
        )),
        Request::GetSessionStats => Some(response(id, "get_session_stats", session.stats(agent))),
        Request::SetSessionName { name } => {
            session.set_name(name.clone());
            Some(response(
                id,
                "set_session_name",
                serde_json::json!({ "name": name }),
            ))
        }
        Request::GenerateTitle => match agent.generate_title() {
            Ok(title) => Some(response(
                id,
                "generate_title",
                serde_json::json!({ "title": title }),
            )),
            Err(error) => Some(failure(id, "generate_title", &error.to_string())),
        },
        Request::NewSession { parent_session } => {
            match session.start_new(cwd, agent, parent_session) {
                Ok(()) => Some(response(
                    id,
                    "new_session",
                    serde_json::json!({ "sessionFile": session.path_string() }),
                )),
                Err(error) => Some(failure(id, "new_session", &error.to_string())),
            }
        }
        Request::SwitchSession { session_path } => {
            match session.switch(&session_path, cwd, agent) {
                Ok(()) => Some(response(
                    id,
                    "switch_session",
                    serde_json::json!({ "cancelled": false }),
                )),
                Err(error) => Some(failure(id, "switch_session", &error.to_string())),
            }
        }
        Request::Steer { text } => {
            session.steering.push(text);
            Some(response(
                id,
                "steer",
                serde_json::json!({ "disposition": "queued" }),
            ))
        }
        Request::FollowUp { text } => {
            session.follow_up.push(text);
            Some(response(
                id,
                "follow_up",
                serde_json::json!({ "disposition": "queued" }),
            ))
        }
        Request::Abort => Some(response(id, "abort", serde_json::json!({}))),
        Request::ClearQueue => {
            let steering = std::mem::take(&mut session.steering);
            let follow_up = std::mem::take(&mut session.follow_up);
            Some(response(
                id,
                "clear_queue",
                serde_json::json!({ "steering": steering, "followUp": follow_up }),
            ))
        }
        Request::SetModel { provider, model_id } => {
            session.model = Some((provider.clone(), model_id.clone()));
            Some(response(
                id,
                "set_model",
                serde_json::json!({ "model": { "id": model_id, "provider": provider } }),
            ))
        }
        Request::CycleModel => Some(response(
            id,
            "cycle_model",
            serde_json::json!({ "model": session.model_value() }),
        )),
        Request::GetAvailableModels => Some(response(
            id,
            "get_available_models",
            serde_json::json!({ "models": session.model_values() }),
        )),
        Request::SetThinkingLevel { level } => {
            session.thinking_level = level.clone();
            agent.set_thinking_level(&level);
            Some(response(
                id,
                "set_thinking_level",
                serde_json::json!({ "level": level }),
            ))
        }
        Request::CycleThinkingLevel => {
            let level = session.cycle_thinking();
            agent.set_thinking_level(&level);
            Some(response(
                id,
                "cycle_thinking_level",
                serde_json::json!({ "level": level }),
            ))
        }
        Request::GetAvailableThinkingLevels => Some(response(
            id,
            "get_available_thinking_levels",
            serde_json::json!({ "levels": THINKING_LEVELS }),
        )),
        Request::SetSteeringMode { mode } => {
            if !is_queue_mode(&mode) {
                return Some(failure(
                    id,
                    "set_steering_mode",
                    &format!(
                        "unsupported steering mode `{mode}` (expected `all` or `one-at-a-time`)"
                    ),
                ));
            }
            session.steering_mode = mode.clone();
            Some(response(
                id,
                "set_steering_mode",
                serde_json::json!({ "mode": mode }),
            ))
        }
        Request::SetFollowUpMode { mode } => {
            if !is_queue_mode(&mode) {
                return Some(failure(
                    id,
                    "set_follow_up_mode",
                    &format!(
                        "unsupported follow-up mode `{mode}` (expected `all` or `one-at-a-time`)"
                    ),
                ));
            }
            session.follow_up_mode = mode.clone();
            Some(response(
                id,
                "set_follow_up_mode",
                serde_json::json!({ "mode": mode }),
            ))
        }
        Request::Compact { .. } => {
            let tokens_before = agent.retained_tokens();
            let (summary, dropped) = match agent.force_compact() {
                Some(AgentEvent::Compacted {
                    dropped, summary, ..
                }) => (summary, Some(dropped)),
                _ => (None, None),
            };
            // Persist immediately so the on-disk file records the compaction
            // (summary + firstKeptEntryId) rather than waiting for the next
            // turn, which may never come.
            let first_kept = if dropped.is_some() {
                session.persist(agent);
                session
                    .journal
                    .as_ref()
                    .and_then(|journal| journal.last_compaction())
                    .and_then(|recorded| recorded.first_kept_entry_id.clone())
            } else {
                None
            };
            Some(response(
                id,
                "compact",
                serde_json::json!({
                    "reason": "manual",
                    "summary": summary,
                    "dropped": dropped,
                    "firstKeptEntryId": first_kept,
                    "tokensBefore": tokens_before,
                    "estimatedTokensAfter": agent.retained_tokens(),
                }),
            ))
        }
        Request::SetAutoCompaction { enabled } => {
            session.auto_compaction = enabled;
            agent.set_auto_compaction(enabled);
            Some(response(
                id,
                "set_auto_compaction",
                serde_json::json!({ "enabled": enabled }),
            ))
        }
        Request::SetAutoRetry { enabled } => {
            session.auto_retry = enabled;
            agent.set_auto_retry(enabled);
            Some(response(
                id,
                "set_auto_retry",
                serde_json::json!({ "enabled": enabled }),
            ))
        }
        Request::AbortRetry => Some(response(id, "abort_retry", serde_json::json!({}))),
        Request::Bash {
            command,
            exclude_from_context,
        } => {
            let result = pi_tools::shell::run_shell(&command, Path::new(cwd), None);
            let output = result.output;
            let exit_code = result.exit_code;
            if !exclude_from_context {
                agent.push_user(format!("$ {command}\n{output}"));
                session.persist(agent);
            }
            Some(response(
                id,
                "bash",
                serde_json::json!({ "output": output, "exitCode": exit_code }),
            ))
        }
        Request::AbortBash => Some(response(id, "abort_bash", serde_json::json!({}))),
        Request::ExportHtml { output_path } => {
            let base = session
                .path
                .as_ref()
                .and_then(|path| path.parent().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from(cwd));
            let path = match output_path {
                // An explicit path is an arbitrary file write unless jailed; keep
                // it inside the session directory or the workspace.
                Some(requested) => {
                    let context =
                        pi_tools::ToolContext::new(base).with_write_roots([PathBuf::from(cwd)]);
                    match context.resolve_write(&requested) {
                        Ok(path) => path,
                        Err(error) => return Some(failure(id, "export_html", &error.to_string())),
                    }
                }
                None => base.join("session.html"),
            };
            match export_html(agent, &path) {
                Ok(()) => Some(response(
                    id,
                    "export_html",
                    serde_json::json!({ "path": path.to_string_lossy() }),
                )),
                Err(error) => Some(failure(id, "export_html", &error.to_string())),
            }
        }
        Request::Fork { entry_id } => match session.fork(&entry_id, cwd, agent) {
            Ok(text) => Some(response(
                id,
                "fork",
                serde_json::json!({ "text": text, "cancelled": false }),
            )),
            Err(error) => Some(failure(id, "fork", &error.to_string())),
        },
        Request::Clone => match session.clone_session(cwd, agent) {
            Ok(()) => Some(response(
                id,
                "clone",
                serde_json::json!({ "cancelled": false }),
            )),
            Err(error) => Some(failure(id, "clone", &error.to_string())),
        },
        Request::GetForkMessages => Some(response(
            id,
            "get_fork_messages",
            serde_json::json!({ "messages": session.fork_messages() }),
        )),
        Request::GetCommands => Some(response(
            id,
            "get_commands",
            serde_json::json!({ "commands": agent.commands() }),
        )),
    }
}

fn response(id: Option<String>, command: &str, data: serde_json::Value) -> Event {
    Event::Response {
        id,
        command: command.to_string(),
        success: true,
        data,
    }
}

fn failure(id: Option<String>, command: &str, message: &str) -> Event {
    Event::Response {
        id,
        command: command.to_string(),
        success: false,
        data: serde_json::json!({ "error": message }),
    }
}

fn state_event(agent: &Agent) -> Event {
    Event::State {
        messages: agent.messages().len(),
        system: agent.system().to_string(),
        transcript: transcript_values(agent.messages()),
    }
}

fn last_assistant_text(agent: &Agent) -> Option<String> {
    agent.last_assistant_text()
}

fn export_html(agent: &Agent, path: &std::path::Path) -> std::io::Result<()> {
    let mut body = String::from("<h1>pi-native session</h1>");
    for message in transcript_values(agent.messages()) {
        let text = serde_json::to_string_pretty(&message).unwrap_or_default();
        body.push_str(&format!("<pre>{}</pre>", escape_html(&text)));
    }
    std::fs::write(
        path,
        format!("<!doctype html><meta charset=\"utf-8\"><body>{body}</body>"),
    )
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn from_agent_event(event: AgentEvent) -> Event {
    match event {
        AgentEvent::AgentStart => Event::AgentStart,
        AgentEvent::TurnStart => Event::TurnStart,
        AgentEvent::TurnEnd => Event::TurnEnd,
        AgentEvent::AgentSettled => Event::AgentSettled,
        AgentEvent::AssistantDelta(text) => Event::AssistantDelta { text },
        AgentEvent::ThinkingDelta(text) => Event::ThinkingDelta { text },
        AgentEvent::AssistantText(text) => Event::AssistantText { text },
        AgentEvent::ToolStart {
            tool_call_id,
            name,
            input,
        } => Event::ToolStart {
            tool_call_id,
            name,
            input,
        },
        AgentEvent::ToolEnd {
            tool_call_id,
            name,
            is_error,
            content,
        } => Event::ToolEnd {
            tool_call_id,
            name,
            is_error,
            content,
        },
        AgentEvent::Done { stop_reason } => Event::Done { stop_reason },
        AgentEvent::Compacted {
            dropped,
            summary,
            tokens_before,
            tokens_after,
        } => Event::Compacted {
            dropped,
            summary,
            // Auto compaction runs when the token threshold is crossed.
            reason: "threshold".to_string(),
            tokens_before,
            estimated_tokens_after: tokens_after,
            first_kept_entry_id: None,
        },
        AgentEvent::Usage(usage) => Event::Usage {
            input: usage.input,
            output: usage.output,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
        },
    }
}

/// A unit's session file and journal, when it has one.
struct SessionState {
    path: Option<PathBuf>,
    journal: Option<SessionJournal>,
    name: Option<String>,
    thinking_level: String,
    steering_mode: String,
    follow_up_mode: String,
    auto_compaction: bool,
    auto_retry: bool,
    model: Option<(String, String)>,
    /// Messages queued by `steer`, delivered before the next model call.
    steering: Vec<String>,
    /// Messages queued by `follow_up`, delivered after the current turn.
    follow_up: Vec<String>,
}

/// pi's thinking levels, in order.
const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// A queue delivery mode: deliver every queued message at once, or one per
/// turn. Anything else is rejected so a client cannot set a mode that is not
/// honoured.
fn is_queue_mode(mode: &str) -> bool {
    matches!(mode, "all" | "one-at-a-time")
}

impl SessionState {
    fn empty() -> Self {
        Self {
            path: None,
            journal: None,
            name: None,
            thinking_level: "off".to_string(),
            steering_mode: "all".to_string(),
            follow_up_mode: "all".to_string(),
            auto_compaction: true,
            auto_retry: true,
            model: None,
            steering: Vec::new(),
            follow_up: Vec::new(),
        }
    }

    fn from_path(path: Option<PathBuf>, cwd: &str, agent: &mut Agent) -> Self {
        let mut state = Self::empty();
        if let Some(path) = path {
            if let Ok((journal, transcript)) = SessionJournal::open(path.clone(), cwd) {
                agent.replace_messages(transcript);
                state.journal = Some(journal);
            }
            state.path = Some(path);
        }
        state
    }

    fn path_string(&self) -> Option<String> {
        self.path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
    }

    fn persist(&mut self, agent: &Agent) {
        if let Some(journal) = self.journal.as_mut() {
            let _ = journal.persist(agent.messages());
        }
    }

    fn set_name(&mut self, name: String) {
        if let Some(journal) = &mut self.journal {
            let _ = journal.set_name(&name);
        }
        self.name = Some(name);
    }

    fn entries(&self, since: Option<&str>) -> Vec<serde_json::Value> {
        // Read the journal's in-memory entries rather than re-reading and
        // re-serializing the whole session file on every call. The journal owns
        // the file, so its entries are authoritative.
        let entries: &[pi_session::SessionEntry] = match &self.journal {
            Some(journal) => journal.entries(),
            None => return Vec::new(),
        };
        let values: Vec<serde_json::Value> = entries
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap_or(serde_json::Value::Null))
            .collect();
        match since {
            Some(since) => match values.iter().position(|value| {
                value.get("id").and_then(serde_json::Value::as_str) == Some(since)
            }) {
                Some(index) => values.into_iter().skip(index + 1).collect(),
                None => values,
            },
            None => values,
        }
    }

    fn tree(&self) -> (Vec<serde_json::Value>, Option<String>) {
        build_tree(&self.entries(None))
    }

    fn stats(&self, agent: &Agent) -> serde_json::Value {
        serde_json::json!({
            "sessionFile": self.path_string(),
            "sessionName": self.name,
            "messageCount": agent.messages().len(),
            "entryCount": self.entries(None).len(),
        })
    }

    fn switch(&mut self, path: &str, cwd: &str, agent: &mut Agent) -> std::io::Result<()> {
        let path = PathBuf::from(path);
        let (journal, transcript) = SessionJournal::open(path.clone(), cwd)?;
        agent.replace_messages(transcript);
        self.journal = Some(journal);
        self.path = Some(path);
        self.name = None;
        Ok(())
    }

    fn start_new(
        &mut self,
        cwd: &str,
        agent: &mut Agent,
        parent_session: Option<String>,
    ) -> std::io::Result<()> {
        agent.replace_messages(Vec::new());
        self.name = None;
        // Start a new file next to the current one, or under the working dir.
        let dir = self
            .path
            .as_ref()
            .and_then(|path| path.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(cwd));
        // `NewSession { parentSession }` records the parent session path in the
        // new file header (pi's `parentSession`), so a branched session links
        // back to its origin.
        let path = new_session_path_in_dir(&dir, Path::new(cwd), None, parent_session.as_deref())?;
        let (journal, _) = SessionJournal::open(path.clone(), cwd)?;
        self.journal = Some(journal);
        self.path = Some(path);
        Ok(())
    }

    fn model_value(&self) -> serde_json::Value {
        match &self.model {
            Some((provider, model_id)) => {
                serde_json::json!({ "id": model_id, "provider": provider })
            }
            None => serde_json::Value::Null,
        }
    }

    fn model_values(&self) -> Vec<serde_json::Value> {
        if self.model.is_some() {
            vec![self.model_value()]
        } else {
            Vec::new()
        }
    }

    /// Drain queued messages per `mode`: all of them, or just the first one
    /// (`one-at-a-time`), leaving the rest queued.
    fn drain_queue(queue: &mut Vec<String>, mode: &str) -> Vec<String> {
        if mode == "one-at-a-time" {
            if queue.is_empty() {
                Vec::new()
            } else {
                vec![queue.remove(0)]
            }
        } else {
            std::mem::take(queue)
        }
    }

    /// Steering messages to deliver before the next model call, respecting
    /// `steering_mode` (leftover messages stay queued).
    fn take_steering(&mut self) -> Vec<String> {
        Self::drain_queue(&mut self.steering, &self.steering_mode)
    }

    /// Follow-up messages to deliver after the current turn, respecting
    /// `follow_up_mode` (leftover messages stay queued).
    fn take_follow_up(&mut self) -> Vec<String> {
        Self::drain_queue(&mut self.follow_up, &self.follow_up_mode)
    }

    fn cycle_thinking(&mut self) -> String {
        let index = THINKING_LEVELS
            .iter()
            .position(|level| *level == self.thinking_level)
            .unwrap_or(0);
        let next = THINKING_LEVELS[(index + 1) % THINKING_LEVELS.len()];
        self.thinking_level = next.to_string();
        self.thinking_level.clone()
    }

    /// Create a new session file with pi's `<timestamp>_<id>.jsonl` layout in
    /// the directory of the active session (or `cwd`), so the header id matches
    /// the filename and pi-web can address it.
    fn new_file_path(&self, cwd: &str) -> std::io::Result<PathBuf> {
        let dir = self
            .path
            .as_ref()
            .and_then(|path| path.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(cwd));
        new_session_path_in_dir(&dir, Path::new(cwd), None, None)
    }

    fn clone_session(&mut self, cwd: &str, agent: &Agent) -> std::io::Result<()> {
        let messages = agent.messages().to_vec();
        let path = self.new_file_path(cwd)?;
        let (mut journal, _) = SessionJournal::open(path.clone(), cwd)?;
        journal.persist(&messages)?;
        self.path = Some(path);
        self.journal = Some(journal);
        Ok(())
    }

    fn fork(
        &mut self,
        entry_id: &str,
        cwd: &str,
        agent: &mut Agent,
    ) -> std::io::Result<Option<String>> {
        let Some(path) = self.path.clone() else {
            return Err(std::io::Error::other("no session file"));
        };
        let session =
            SessionFile::read(&path).map_err(|error| std::io::Error::other(error.to_string()))?;
        // Walk parentId from the entry to the root, then reverse.
        let mut chain: Vec<String> = Vec::new();
        let mut current = Some(entry_id.to_string());
        while let Some(id) = current {
            chain.push(id.clone());
            current = session
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| entry.parent_id.clone());
        }
        chain.reverse();
        let entries: Vec<pi_session::SessionEntry> = chain
            .iter()
            .filter_map(|id| {
                session
                    .entries
                    .iter()
                    .find(|entry| &entry.id == id)
                    .cloned()
            })
            .collect();
        let branch = SessionFile {
            header: session.header,
            entries,
        };
        let text = branch
            .message_entries()
            .last()
            .and_then(|entry| entry.message())
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
            .map(str::to_string);
        let transcript = messages_from_session(&branch);
        let new_path = self.new_file_path(cwd)?;
        let (mut journal, _) = SessionJournal::open(new_path.clone(), cwd)?;
        journal.persist(&transcript)?;
        agent.replace_messages(transcript);
        self.path = Some(new_path);
        self.journal = Some(journal);
        Ok(text)
    }

    fn fork_messages(&self) -> Vec<serde_json::Value> {
        let Some(path) = &self.path else {
            return Vec::new();
        };
        let Ok(session) = SessionFile::read(path) else {
            return Vec::new();
        };
        session
            .message_entries()
            .filter_map(|entry| {
                let message = entry.message()?;
                if message.get("role").and_then(serde_json::Value::as_str) != Some("user") {
                    return None;
                }
                let text = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Some(serde_json::json!({ "entryId": entry.id, "text": text }))
            })
            .collect()
    }
}

/// Build pi's `SessionTreeNode[]` from flat entries (roots first).
fn build_tree(entries: &[serde_json::Value]) -> (Vec<serde_json::Value>, Option<String>) {
    use std::collections::{HashMap, HashSet};
    let ids: HashSet<String> = entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let parent = entry
            .get("parentId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let Some(parent) = parent.filter(|parent| ids.contains(parent)) {
            children.entry(parent).or_default().push(index);
        } else {
            roots.push(index);
        }
    }

    fn node(
        index: usize,
        entries: &[serde_json::Value],
        children: &HashMap<String, Vec<usize>>,
    ) -> serde_json::Value {
        let entry = &entries[index];
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let kids: Vec<serde_json::Value> = children
            .get(id)
            .map(|list| {
                list.iter()
                    .map(|&child| node(child, entries, children))
                    .collect()
            })
            .unwrap_or_default();
        serde_json::json!({ "entry": entry, "children": kids })
    }

    let tree = roots
        .iter()
        .map(|&index| node(index, entries, &children))
        .collect();
    let leaf_id = entries
        .last()
        .and_then(|entry| entry.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_string);
    (tree, leaf_id)
}

/// A reader/writer pair for the serve loop.
struct SharedIo<R, W> {
    reader: std::cell::RefCell<R>,
    writer: std::cell::RefCell<W>,
}

impl<R: std::io::BufRead, W: std::io::Write> SharedIo<R, W> {
    fn write(&self, event: &Event) {
        if let Ok(line) = serde_json::to_string(event) {
            let mut writer = self.writer.borrow_mut();
            let _ = writeln!(writer, "{line}");
            let _ = writer.flush();
        }
    }

    /// The next valid request with its optional correlation id, or `None` at end
    /// of input. Invalid lines emit an `error` event and are skipped.
    fn next_request(&self) -> Option<(Option<String>, Request)> {
        loop {
            let mut line = String::new();
            match self.reader.borrow_mut().read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            if line.trim().is_empty() {
                continue;
            }
            // Parse the value first: `ui_response` owns `id`, while other
            // commands use it only for correlation.
            let value: serde_json::Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => {
                    self.write(&Event::Error {
                        message: format!("invalid request: {error}"),
                    });
                    continue;
                }
            };
            match serde_json::from_value::<Request>(value.clone()) {
                Ok(request) => {
                    let id = if matches!(request, Request::UiResponse { .. }) {
                        None
                    } else {
                        value
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    };
                    return Some((id, request));
                }
                Err(error) => self.write(&Event::Error {
                    message: format!("invalid request: {error}"),
                }),
            }
        }
    }
}

/// Run one agent turn, translating and streaming each event. Compaction events
/// are annotated with the first kept session entry id, which pi reports on
/// `compaction_end` (the journal still holds the pre-rewrite entries here).
fn run_turn<R: std::io::BufRead, W: std::io::Write>(
    agent: &mut Agent,
    io: &std::rc::Rc<SharedIo<R, W>>,
    session: &SessionState,
) -> Result<(), AgentError> {
    agent.run_with(|event| {
        let event = match event {
            AgentEvent::Compacted { dropped, .. } => {
                let mut translated = from_agent_event(event.clone());
                if let Event::Compacted {
                    first_kept_entry_id,
                    ..
                } = &mut translated
                {
                    *first_kept_entry_id = session
                        .journal
                        .as_ref()
                        .and_then(|journal| journal.entry_id_for_message(*dropped))
                        .map(str::to_string);
                }
                translated
            }
            _ => from_agent_event(event.clone()),
        };
        io.write(&event)
    })
}

fn drive<R: std::io::BufRead, W: std::io::Write, F: FnMut(&Agent)>(
    agent: &mut Agent,
    io: &std::rc::Rc<SharedIo<R, W>>,
    session: &mut SessionState,
    cwd: &str,
    mut after_turn: F,
) {
    io.write(&Event::Ready {
        version: PROTOCOL_VERSION,
    });
    while let Some((id, request)) = io.next_request() {
        match request {
            Request::Prompt { text } => {
                // pi acknowledges immediately, then streams events.
                io.write(&response(
                    id,
                    "prompt",
                    serde_json::json!({ "disposition": "started" }),
                ));
                agent.push_user(text);
                // Steering messages are injected before the next model call.
                // `one-at-a-time` delivers one and leaves the rest queued.
                for queued in session.take_steering() {
                    agent.push_user(queued);
                }
                // Run the turn, then deliver follow-ups as subsequent turns
                // (pi's follow-up semantics: after the current turn, not inside
                // it). A failure stops the loop so the error is not masked.
                loop {
                    if let Err(error) = run_turn(agent, io, session) {
                        io.write(&Event::Error {
                            message: error.to_string(),
                        });
                        break;
                    }
                    session.persist(agent);
                    after_turn(agent);
                    let follow_ups = session.take_follow_up();
                    if follow_ups.is_empty() {
                        break;
                    }
                    for queued in follow_ups {
                        agent.push_user(queued);
                    }
                }
            }
            // Answered by the approval hook while a turn is running.
            Request::UiResponse { .. } => {}
            other => {
                if let Some(event) = apply_command(agent, session, cwd, id, other) {
                    io.write(&event);
                }
            }
        }
    }
}

/// Serve a unit over any reader/writer pair, one JSON request per line.
pub fn serve<R: std::io::BufRead, W: std::io::Write>(
    agent: &mut Agent,
    reader: R,
    writer: W,
) -> Result<(), AgentError> {
    serve_with(agent, reader, writer, |_| {})
}

/// Like [`serve`], but calls `after_turn` once a prompt finishes.
pub fn serve_with<R: std::io::BufRead, W: std::io::Write, F: FnMut(&Agent)>(
    agent: &mut Agent,
    reader: R,
    writer: W,
    after_turn: F,
) -> Result<(), AgentError> {
    let io = std::rc::Rc::new(SharedIo {
        reader: std::cell::RefCell::new(reader),
        writer: std::cell::RefCell::new(writer),
    });
    let mut session = SessionState::empty();
    drive(agent, &io, &mut session, ".", after_turn);
    Ok(())
}

/// Serve a unit with a session file: seeds the transcript, persists each turn,
/// and answers the session navigation commands (`get_tree`, `switch_session`, …).
///
/// Tools run without approval, matching pi.
pub fn serve_session<
    R: std::io::BufRead + 'static,
    W: std::io::Write + 'static,
    F: FnMut(&Agent),
>(
    agent: &mut Agent,
    session_path: Option<PathBuf>,
    cwd: &str,
    reader: R,
    writer: W,
    after_turn: F,
) -> Result<(), AgentError> {
    let io = std::rc::Rc::new(SharedIo {
        reader: std::cell::RefCell::new(reader),
        writer: std::cell::RefCell::new(writer),
    });
    let mut session = SessionState::from_path(session_path, cwd, agent);
    drive(agent, &io, &mut session, cwd, after_turn);
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
