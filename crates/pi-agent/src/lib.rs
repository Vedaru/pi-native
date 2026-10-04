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

pub mod anthropic;

pub use anthropic::{turn_from_stream, AnthropicProvider};
pub use pi_tools::ToolContext;

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

/// Everything the provider needs for one completion.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: String,
    pub messages: Vec<TranscriptMessage>,
    pub tools: Vec<ToolSpec>,
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
    fn complete(&self, _request: &CompletionRequest) -> Result<AssistantTurn, AgentError> {
        let mut turns = self.turns.lock().expect("faux lock");
        if turns.is_empty() {
            Ok(AssistantTurn::default())
        } else {
            Ok(turns.remove(0))
        }
    }
}

/// A model backend. Returns a full turn; streaming is a provider concern.
pub trait ModelProvider {
    fn complete(&self, request: &CompletionRequest) -> Result<AssistantTurn, AgentError>;
}

/// Runs turns over a transcript.
pub struct Agent {
    provider: Box<dyn ModelProvider>,
    tools: Vec<Box<dyn Tool>>,
    system: String,
    messages: Vec<TranscriptMessage>,
    tool_context: ToolContext,
    max_iterations: usize,
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
            max_iterations: 16,
        }
    }

    pub fn with_max_iterations(mut self, max: usize) -> Self {
        self.max_iterations = max.max(1);
        self
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.messages.push(TranscriptMessage::UserText(text.into()));
    }

    pub fn messages(&self) -> &[TranscriptMessage] {
        &self.messages
    }

    /// Run the model/tool loop until it stops or the iteration bound is hit.
    pub fn run(&mut self) -> Result<Vec<AgentEvent>, AgentError> {
        let tools: Vec<ToolSpec> = self.tools.iter().map(|tool| tool.spec()).collect();
        let mut events = Vec::new();

        for _ in 0..self.max_iterations {
            let request = CompletionRequest {
                system: self.system.clone(),
                messages: self.messages.clone(),
                tools: tools.clone(),
            };
            let turn = self.provider.complete(&request)?;

            if !turn.text.is_empty() {
                events.push(AgentEvent::AssistantText(turn.text.clone()));
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

            if turn.tool_calls.is_empty() {
                events.push(AgentEvent::Done {
                    stop_reason: turn.stop_reason.clone(),
                });
                return Ok(events);
            }

            for call in &turn.tool_calls {
                events.push(AgentEvent::ToolStart {
                    name: call.name.clone(),
                    input: call.arguments.clone(),
                });
                let result = self.run_tool(call);
                events.push(AgentEvent::ToolEnd {
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
        }

        events.push(AgentEvent::Done {
            stop_reason: Some("max_iterations".to_string()),
        });
        Ok(events)
    }

    fn run_tool(&self, call: &ToolCall) -> ToolResult {
        match self.tools.iter().find(|tool| tool.name() == call.name) {
            Some(tool) => tool.run(&call.arguments, &self.tool_context),
            None => ToolResult::error(format!("unknown tool: {}", call.name)),
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
