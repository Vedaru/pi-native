//! Headless RPC protocol and generic UI events.
//!
//! A unit is driven by JSON lines. Requests are commands (`prompt`,
//! `get_state`, `ui_response`); responses are events (`ready`, `assistant_text`,
//! `tool_start`, `tool_end`, `done`, `state`, `error`) plus **generic UI
//! requests**. The host does not assume a terminal: any client (a small
//! terminal client, a web page, a test harness) can render a `ui_request` and
//! send back a `ui_response`.

use pi_agent::{Agent, AgentError, AgentEvent};
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
    State {
        messages: usize,
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
        Request::GetState => vec![Event::State {
            messages: agent.messages().len(),
        }],
        // The bridge stores responses; the host decides what to do with them.
        Request::UiResponse { .. } => Vec::new(),
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
    }
}

/// Serve a unit over any reader/writer pair, one JSON request per line.
///
/// Writes `ready`, then one or more events per request, each as a JSON line.
pub fn serve<R: std::io::BufRead, W: std::io::Write>(
    agent: &mut Agent,
    reader: R,
    mut writer: W,
) -> Result<(), AgentError> {
    let mut write = |event: &Event| {
        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(writer, "{line}");
        }
        let _ = writer.flush();
    };

    write(&Event::Ready {
        version: PROTOCOL_VERSION,
    });

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                for event in handle(agent, request) {
                    write(&event);
                }
            }
            Err(error) => write(&Event::Error {
                message: format!("invalid request: {error}"),
            }),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
