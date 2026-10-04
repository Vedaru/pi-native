//! Headless RPC protocol and generic UI events.
//!
//! A unit is driven by JSON lines. Requests are commands (`prompt`,
//! `get_state`, `ui_response`); responses are events (`ready`, `assistant_text`,
//! `tool_start`, `tool_end`, `done`, `state`, `error`) plus **generic UI
//! requests**. The host does not assume a terminal: any client (a small
//! terminal client, a web page, a test harness) can render a `ui_request` and
//! send back a `ui_response`.

use pi_agent::{transcript_values, Agent, AgentError, AgentEvent, Approval, Approver};
use serde::{Deserialize, Serialize};

/// A protocol version, so clients can detect incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// A command sent to a unit.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Run a prompt through the agent.
    Prompt { text: String },
    /// Report current state.
    GetState,
    /// Answer a `ui_request` emitted earlier.
    UiResponse {
        id: String,
        value: serde_json::Value,
    },
}

/// An event emitted by a unit.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Ready {
        version: u32,
    },
    AssistantText {
        text: String,
    },
    ToolStart {
        name: String,
        input: serde_json::Value,
    },
    ToolEnd {
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

/// Apply one request to an agent, returning the events it produced.
pub fn handle(agent: &mut Agent, request: Request) -> Vec<Event> {
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
        Request::GetState => vec![state_event(agent)],
        // The bridge stores responses; the host decides what to do with them.
        Request::UiResponse { .. } => Vec::new(),
    }
}

fn state_event(agent: &Agent) -> Event {
    Event::State {
        messages: agent.messages().len(),
        system: agent.system().to_string(),
        transcript: transcript_values(agent.messages()),
    }
}

fn from_agent_event(event: AgentEvent) -> Event {
    match event {
        AgentEvent::AssistantText(text) => Event::AssistantText { text },
        AgentEvent::ToolStart { name, input } => Event::ToolStart { name, input },
        AgentEvent::ToolEnd {
            name,
            is_error,
            content,
        } => Event::ToolEnd {
            name,
            is_error,
            content,
        },
        AgentEvent::Done { stop_reason } => Event::Done { stop_reason },
        AgentEvent::Compacted { dropped, summary } => Event::Compacted { dropped, summary },
    }
}

/// A reader/writer pair shared between the serve loop and the approval hook.
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

    /// The next valid request, or `None` at end of input. Invalid lines emit an
    /// `error` event and are skipped.
    fn next_request(&self) -> Option<Request> {
        loop {
            let mut line = String::new();
            match self.reader.borrow_mut().read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Request>(&line) {
                Ok(request) => return Some(request),
                Err(error) => self.write(&Event::Error {
                    message: format!("invalid request: {error}"),
                }),
            }
        }
    }
}

fn drive<R: std::io::BufRead, W: std::io::Write, F: FnMut(&Agent)>(
    agent: &mut Agent,
    io: &std::rc::Rc<SharedIo<R, W>>,
    mut after_turn: F,
) {
    io.write(&Event::Ready {
        version: PROTOCOL_VERSION,
    });
    while let Some(request) = io.next_request() {
        match request {
            Request::Prompt { text } => {
                agent.push_user(text);
                match agent.run_with(|event| io.write(&from_agent_event(event.clone()))) {
                    Ok(()) => after_turn(agent),
                    Err(error) => io.write(&Event::Error {
                        message: error.to_string(),
                    }),
                }
            }
            Request::GetState => io.write(&state_event(agent)),
            // Answered by the approval hook while a turn is running.
            Request::UiResponse { .. } => {}
        }
    }
}

/// Serve a unit over any reader/writer pair, one JSON request per line.
///
/// Writes `ready`, then one or more events per request, each as a JSON line.
/// Events are streamed as the turn produces them (no whole-turn buffering).
pub fn serve<R: std::io::BufRead, W: std::io::Write>(
    agent: &mut Agent,
    reader: R,
    writer: W,
) -> Result<(), AgentError> {
    serve_with(agent, reader, writer, |_| {})
}

/// Like [`serve`], but calls `after_turn` once a prompt finishes (e.g. to
/// persist the session). Events are streamed, not collected.
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
    drive(agent, &io, after_turn);
    Ok(())
}

/// Serve a unit whose approval-required tools ask the client.
///
/// Emits `ui_request { kind: "confirm" }` and blocks the turn until the matching
/// `ui_response` arrives, so the UI is a separate service on the other end of
/// the protocol. Requires `'static` I/O because the hook is stored on the agent.
pub fn serve_unit<R: std::io::BufRead + 'static, W: std::io::Write + 'static>(
    agent: &mut Agent,
    reader: R,
    writer: W,
) -> Result<(), AgentError> {
    serve_unit_with(agent, reader, writer, |_| {})
}

/// Like [`serve_unit`], but calls `after_turn` once a prompt finishes.
pub fn serve_unit_with<
    R: std::io::BufRead + 'static,
    W: std::io::Write + 'static,
    F: FnMut(&Agent),
>(
    agent: &mut Agent,
    reader: R,
    writer: W,
    after_turn: F,
) -> Result<(), AgentError> {
    let io = std::rc::Rc::new(SharedIo {
        reader: std::cell::RefCell::new(reader),
        writer: std::cell::RefCell::new(writer),
    });
    agent.set_approver(std::rc::Rc::new(ProtocolApprover {
        io: io.clone(),
        counter: std::cell::Cell::new(0),
    }));
    drive(agent, &io, after_turn);
    Ok(())
}

/// Asks the connected client before running an approval-required tool.
struct ProtocolApprover<R, W> {
    io: std::rc::Rc<SharedIo<R, W>>,
    counter: std::cell::Cell<u64>,
}

impl<R: std::io::BufRead, W: std::io::Write> Approver for ProtocolApprover<R, W> {
    fn approve(&self, tool: &str, input: &serde_json::Value) -> Approval {
        let id = format!("ui-{}", self.counter.get());
        self.counter.set(self.counter.get() + 1);
        self.io.write(&Event::UiRequest {
            id: id.clone(),
            kind: "confirm".to_string(),
            prompt: format!("Run {tool} with {input}?"),
            options: vec!["allow".to_string(), "deny".to_string()],
        });
        while let Some(request) = self.io.next_request() {
            if let Request::UiResponse {
                id: response_id,
                value,
            } = request
            {
                if response_id == id {
                    let text = value.as_str().map(str::to_ascii_lowercase);
                    let allow = value.as_bool().unwrap_or(false)
                        || matches!(text.as_deref(), Some("y" | "yes" | "allow" | "true"));
                    return if allow {
                        Approval::Allow
                    } else {
                        Approval::Deny
                    };
                }
            }
        }
        Approval::Deny
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
