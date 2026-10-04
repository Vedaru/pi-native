//! The agent loop.
//!
//! Ties a model provider, tools, and a transcript into a turn: ask the model,
//! run any tool calls, append the results, and repeat until it stops. The
//! provider is a trait so the loop is testable without a network; the native
//! Anthropic/OpenAI/Google implementations live in `pi-net`.
//!
//! Events are produced for the protocol layer (VED-324): streamed text is
//! reported as assistant text, and each tool call emits start/end.

use pi_providers::{AssistantBlock, ContentPart, ToolSpec, TranscriptMessage, Usage};
use pi_tools::{Tool, ToolResult};
use serde_json::Value;
use std::sync::Arc;

pub mod providers;
pub mod session;

pub use pi_tools::ToolContext;
pub use providers::{
    anthropic_provider, google_provider, openai_responses_provider, turn_from_stream, HttpProvider,
};
pub use session::{append_messages, messages_from_session};

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A completed assistant turn.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantTurn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub stop_reason: Option<String>,
}

/// Everything the provider needs for one completion. Borrows the transcript so
/// the loop does not clone it every iteration (the pressure test showed that
/// cloning was quadratic in the number of turns).
#[derive(Debug, Clone, Copy)]
pub struct CompletionRequest<'a> {
    pub system: &'a str,
    pub messages: &'a [TranscriptMessage],
    pub tools: &'a [ToolSpec],
}

#[derive(Debug)]
pub enum AgentError {
    Provider(String),
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentError::Provider(message) => write!(f, "provider: {message}"),
        }
    }
}

impl std::error::Error for AgentError {}

/// Something the agent produced during a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    AssistantText(String),
    ToolStart {
        name: String,
        input: Value,
    },
    ToolEnd {
        name: String,
        is_error: bool,
        content: String,
    },
    Done {
        stop_reason: Option<String>,
    },
}

/// A provider that returns pre-scripted turns; for tests and embedding.
pub struct FauxProvider {
    turns: std::sync::Mutex<Vec<AssistantTurn>>,
}

impl FauxProvider {
    pub fn new(turns: Vec<AssistantTurn>) -> Self {
        Self {
            turns: std::sync::Mutex::new(turns),
        }
    }

    pub fn push(&self, turn: AssistantTurn) {
        self.turns.lock().expect("faux lock").push(turn);
    }
}

impl Default for FauxProvider {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ModelProvider for FauxProvider {
    fn complete(&self, _request: &CompletionRequest<'_>) -> Result<AssistantTurn, AgentError> {
        let mut turns = self.turns.lock().expect("faux lock");
        if turns.is_empty() {
            Ok(AssistantTurn::default())
        } else {
            Ok(turns.remove(0))
        }
    }
}

/// A provider that generates the i-th turn on demand.
///
/// Unlike [`FauxProvider`], it stores nothing, so a long turn does not
/// pre-allocate every scripted turn (and does not shift a `Vec` per call).
pub struct FnProvider<F: Fn(usize) -> AssistantTurn> {
    build: F,
    index: std::sync::atomic::AtomicUsize,
}

impl<F: Fn(usize) -> AssistantTurn> FnProvider<F> {
    pub fn new(build: F) -> Self {
        Self {
            build,
            index: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl<F: Fn(usize) -> AssistantTurn> ModelProvider for FnProvider<F> {
    fn complete(&self, _request: &CompletionRequest<'_>) -> Result<AssistantTurn, AgentError> {
        let index = self
            .index
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok((self.build)(index))
    }
}

/// A model backend. Returns a full turn; streaming is a provider concern.
pub trait ModelProvider {
    fn complete(&self, request: &CompletionRequest<'_>) -> Result<AssistantTurn, AgentError>;
}

/// Whether a tool may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Allow,
    Deny,
}

/// Decides whether approval-required tools may run. One generic hook — no
/// per-tool branching in the loop.
pub trait Approver: Send + Sync {
    fn approve(&self, tool: &str, input: &Value) -> Approval;
}

/// Trust everything (default; explicit trust is the caller's choice).
pub struct AllowAll;

impl Approver for AllowAll {
    fn approve(&self, _tool: &str, _input: &Value) -> Approval {
        Approval::Allow
    }
}

/// Deny every approval-required tool (safe default for headless units).
pub struct DenyAll;

impl Approver for DenyAll {
    fn approve(&self, _tool: &str, _input: &Value) -> Approval {
        Approval::Deny
    }
}

/// Runs turns over a transcript.
pub struct Agent {
    provider: Box<dyn ModelProvider>,
    tools: Vec<Box<dyn Tool>>,
    system: String,
    messages: Vec<TranscriptMessage>,
    tool_context: ToolContext,
    approver: Arc<dyn Approver>,
    max_iterations: usize,
    context_window: Option<usize>,
}

impl Agent {
    pub fn new(
        provider: Box<dyn ModelProvider>,
        tools: Vec<Box<dyn Tool>>,
        system: impl Into<String>,
        tool_context: ToolContext,
    ) -> Self {
        Self {
            provider,
            tools,
            system: system.into(),
            messages: Vec::new(),
            tool_context,
            approver: Arc::new(AllowAll),
            max_iterations: 16,
            context_window: None,
        }
    }

    pub fn with_max_iterations(mut self, max: usize) -> Self {
        self.max_iterations = max.max(1);
        self
    }

    /// Set the approval policy for tools that require approval.
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// Bound retained context to the most recent `max_messages` entries.
    ///
    /// This caps memory for long sessions by dropping the oldest messages.
    /// Callers that need durability should persist before the window drops
    /// them. Full-fidelity history needs summarization (compaction), not a
    /// window; this is the simple bound.
    pub fn with_context_window(mut self, max_messages: usize) -> Self {
        self.context_window = Some(max_messages.max(2));
        self
    }

    fn trim_context(&mut self) {
        if let Some(window) = self.context_window {
            if self.messages.len() > window {
                let excess = self.messages.len() - window;
                self.messages.drain(0..excess);
            }
        }
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(TranscriptMessage::UserText(text.into()));
    }

    pub fn messages(&self) -> &[TranscriptMessage] {
        &self.messages
    }

    /// Run the model/tool loop, collecting events. Convenience wrapper over
    /// [`Agent::run_with`]; prefer `run_with` for long runs so events are not
    /// accumulated (which would duplicate tool output for the whole turn).
    pub fn run(&mut self) -> Result<Vec<AgentEvent>, AgentError> {
        let mut events = Vec::new();
        self.run_with(|event| events.push(event.clone()))?;
        Ok(events)
    }

    /// Run the model/tool loop, streaming each event to `on_event` as it occurs.
    pub fn run_with<F: FnMut(&AgentEvent)>(&mut self, mut on_event: F) -> Result<(), AgentError> {
        let tools: Vec<ToolSpec> = self.tools.iter().map(|tool| tool.spec()).collect();

        for _ in 0..self.max_iterations {
            let request = CompletionRequest {
                system: &self.system,
                messages: &self.messages,
                tools: &tools,
            };
            let turn = self.provider.complete(&request)?;

            if !turn.text.is_empty() {
                on_event(&AgentEvent::AssistantText(turn.text.clone()));
            }

            let mut blocks = Vec::new();
            if !turn.text.is_empty() {
                blocks.push(AssistantBlock::Text {
                    text: turn.text.clone(),
                });
            }
            for call in &turn.tool_calls {
                blocks.push(AssistantBlock::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                });
            }
            if !blocks.is_empty() {
                self.messages.push(TranscriptMessage::Assistant(blocks));
            }
            self.trim_context();

            if turn.tool_calls.is_empty() {
                on_event(&AgentEvent::Done {
                    stop_reason: turn.stop_reason.clone(),
                });
                return Ok(());
            }

            for call in &turn.tool_calls {
                on_event(&AgentEvent::ToolStart {
                    name: call.name.clone(),
                    input: call.arguments.clone(),
                });
                let result = self.run_tool(call);
                on_event(&AgentEvent::ToolEnd {
                    name: call.name.clone(),
                    is_error: result.is_error,
                    content: result.content.clone(),
                });
                self.messages.push(TranscriptMessage::ToolResult {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: vec![ContentPart::Text {
                        text: result.content,
                    }],
                    is_error: result.is_error,
                });
            }
            self.trim_context();
        }

        on_event(&AgentEvent::Done {
            stop_reason: Some("max_iterations".to_string()),
        });
        Ok(())
    }

    fn run_tool(&self, call: &ToolCall) -> ToolResult {
        let Some(tool) = self.tools.iter().find(|tool| tool.name() == call.name) else {
            return ToolResult::error(format!("unknown tool: {}", call.name));
        };
        if tool.requires_approval()
            && self.approver.approve(&call.name, &call.arguments) == Approval::Deny
        {
            return ToolResult::error(format!("{}: denied (no approval)", call.name));
        }
        tool.run(&call.arguments, &self.tool_context)
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
