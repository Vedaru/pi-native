//! The agent loop.
//!
//! Ties a model provider, tools, and a transcript into a turn: ask the model,
//! run any tool calls, append the results, and repeat until it stops. The
//! provider is a trait so the loop is testable without a network; the native
//! OpenAI provider implementations live in `pi-net`.
//!
//! Events are produced for the protocol layer (VED-324): streamed text is
//! reported as assistant text, and each tool call emits start/end.

use pi_providers::{AssistantBlock, ContentPart, ToolSpec, TranscriptMessage, Usage};
use pi_tools::{Tool, ToolResult};
use serde_json::Value;
use std::rc::Rc;
use std::sync::Arc;

pub mod prompt;
pub mod providers;
pub mod session;

pub use pi_providers::ThinkingFormat;
pub use pi_tools::ToolContext;
pub use providers::{
    openai_completions_provider, openai_responses_provider, turn_from_stream, HttpProvider,
};
pub use session::{
    append_compaction, append_messages, message_value, messages_from_session, transcript_values,
    SessionJournal,
};

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
    /// Context was compacted: `dropped` older messages were replaced by `summary`.
    Compacted {
        dropped: usize,
        summary: Option<String>,
    },
    /// Provider token usage for one model call (for cache accounting).
    Usage(Usage),
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
pub struct FnProvider<F: Fn(usize) -> AssistantTurn + Send + Sync> {
    build: F,
    index: std::sync::atomic::AtomicUsize,
}

impl<F: Fn(usize) -> AssistantTurn + Send + Sync> FnProvider<F> {
    pub fn new(build: F) -> Self {
        Self {
            build,
            index: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

impl<F: Fn(usize) -> AssistantTurn + Send + Sync> ModelProvider for FnProvider<F> {
    fn complete(&self, _request: &CompletionRequest<'_>) -> Result<AssistantTurn, AgentError> {
        let index = self
            .index
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok((self.build)(index))
    }
}

/// A model backend. Returns a full turn; streaming is a provider concern.
///
/// `Send + Sync` so a second instance can power the compaction summarizer.
pub trait ModelProvider: Send + Sync {
    fn complete(&self, request: &CompletionRequest<'_>) -> Result<AssistantTurn, AgentError>;
}

/// Default tokens reserved for the prompt tail and the model's response, matching
/// pi's `reserveTokens`.
pub const DEFAULT_RESERVE_TOKENS: usize = 16_384;

/// Turns a span of older messages into a short summary.
pub trait Summarizer: Send + Sync {
    fn summarize(&self, messages: &[TranscriptMessage]) -> Result<String, AgentError>;
}

/// pi's compaction summary system prompt.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// pi's compaction instruction, appended as the final user message.
pub const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Summarizes dropped history by asking a model, matching pi's compaction call.
pub struct ProviderSummarizer {
    provider: Box<dyn ModelProvider>,
}

impl ProviderSummarizer {
    pub fn new(provider: Box<dyn ModelProvider>) -> Self {
        Self { provider }
    }
}

impl Summarizer for ProviderSummarizer {
    fn summarize(&self, messages: &[TranscriptMessage]) -> Result<String, AgentError> {
        let mut conversation = messages.to_vec();
        conversation.push(TranscriptMessage::UserText(
            SUMMARIZATION_PROMPT.to_string(),
        ));
        let request = CompletionRequest {
            system: SUMMARIZATION_SYSTEM_PROMPT,
            messages: &conversation,
            tools: &[],
        };
        let turn = self.provider.complete(&request)?;
        Ok(turn.text)
    }
}

/// Whether a tool may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Allow,
    Deny,
}

/// Decides whether approval-required tools may run. One generic hook — no
/// per-tool branching in the loop. Not `Send`/`Sync`: a unit runs the agent on
/// one thread, and the hook may hold protocol I/O.
pub trait Approver {
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
    approver: Rc<dyn Approver>,
    max_iterations: usize,
    context_window: Option<usize>,
    context_bytes: Option<usize>,
    retained_bytes: usize,
    retained_tokens: usize,
    compaction: Option<(usize, usize)>,
    summarizer: Option<Arc<dyn Summarizer>>,
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
            approver: Rc::new(AllowAll),
            max_iterations: 16,
            context_window: None,
            context_bytes: None,
            retained_bytes: 0,
            retained_tokens: 0,
            compaction: None,
            summarizer: None,
        }
    }

    pub fn with_max_iterations(mut self, max: usize) -> Self {
        self.max_iterations = max.max(1);
        self
    }

    /// Set the approval policy for tools that require approval.
    pub fn with_approver(mut self, approver: Rc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// Replace the approval hook (e.g. after a protocol client connects).
    pub fn set_approver(&mut self, approver: Rc<dyn Approver>) {
        self.approver = approver;
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

    /// Bound retained context to `max_bytes` of approximate message size.
    ///
    /// This is the bound that matters when tools return large output: a message
    /// count alone still allows `count x max_tool_output` bytes. Oldest messages
    /// are dropped until the retained total is under budget.
    pub fn with_context_byte_limit(mut self, max_bytes: usize) -> Self {
        self.context_bytes = Some(max_bytes);
        self
    }

    /// Compact when estimated context tokens exceed
    /// `context_window_tokens - reserve_tokens` (pi's rule). The reserve also
    /// determines how much recent tail is kept.
    pub fn with_compaction(mut self, context_window_tokens: usize, reserve_tokens: usize) -> Self {
        self.compaction = Some((context_window_tokens, reserve_tokens));
        self
    }

    /// Use `summarizer` to summarize the dropped span. Without one, a visible
    /// placeholder marks the omission (never a silent drop).
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// Running total of approximate tokens (no per-call scan of the transcript).
    fn total_tokens(&self) -> usize {
        self.retained_tokens
    }

    /// Approximate tokens currently retained.
    pub fn retained_tokens(&self) -> usize {
        self.retained_tokens
    }

    /// Force compaction now (pi's manual `compact`), ignoring the threshold.
    pub fn force_compact(&mut self) -> Option<AgentEvent> {
        let (window, reserve) = self.compaction?;
        let threshold = window.saturating_sub(reserve);
        let tail_budget = reserve.min(threshold / 2).max(1);
        self.compact_tail(tail_budget)
    }

    /// Drop the oldest messages into one summary, keeping a tail of at most
    /// `tail_budget` tokens. Returns the `Compacted` event, if anything moved.
    fn compact_tail(&mut self, tail_budget: usize) -> Option<AgentEvent> {
        let mut tail_tokens = 0usize;
        let mut cut = self.messages.len();
        while cut > 0 {
            let tokens = self.messages[cut - 1].approx_tokens();
            if tail_tokens + tokens > tail_budget {
                break;
            }
            tail_tokens += tokens;
            cut -= 1;
        }
        if cut == 0 {
            return None;
        }
        let dropped = cut;
        let (dropped_bytes, dropped_tokens) =
            self.messages[..cut]
                .iter()
                .fold((0usize, 0usize), |(bytes, tokens), message| {
                    (
                        bytes + message.approx_bytes(),
                        tokens + message.approx_tokens(),
                    )
                });
        let summary = if let Some(summarizer) = &self.summarizer {
            summarizer.summarize(&self.messages[..cut]).ok()
        } else {
            None
        };
        let marker = match &summary {
            Some(text) => format!("[Earlier conversation summary]\n{text}"),
            None => format!("[{dropped} earlier messages omitted to fit the context window]"),
        };
        self.messages.drain(0..cut);
        let summary_message = TranscriptMessage::UserText(marker);
        self.retained_bytes =
            self.retained_bytes.saturating_sub(dropped_bytes) + summary_message.approx_bytes();
        self.retained_tokens =
            self.retained_tokens.saturating_sub(dropped_tokens) + summary_message.approx_tokens();
        self.messages.insert(0, summary_message);
        Some(AgentEvent::Compacted { dropped, summary })
    }

    fn push(&mut self, message: TranscriptMessage) {
        self.retained_bytes += message.approx_bytes();
        self.retained_tokens += message.approx_tokens();
        self.messages.push(message);
    }

    /// Apply context policy: token-based compaction (pi's rule), then the
    /// message-count window, then the byte budget. Returns a `Compacted` event
    /// when compaction happened.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push(TranscriptMessage::UserText(text.into()));
    }

    /// Seed the transcript from existing messages (e.g. a loaded session),
    /// without running a turn or counting tokens against a provider request.
    pub fn extend_messages(&mut self, messages: impl IntoIterator<Item = TranscriptMessage>) {
        for message in messages {
            self.push(message);
        }
    }

    /// Replace the whole transcript (e.g. after switching sessions).
    pub fn replace_messages(&mut self, messages: Vec<TranscriptMessage>) {
        self.messages.clear();
        self.retained_bytes = 0;
        self.retained_tokens = 0;
        self.extend_messages(messages);
    }

    pub fn messages(&self) -> &[TranscriptMessage] {
        &self.messages
    }

    /// The system prompt this agent was built with.
    pub fn system(&self) -> &str {
        &self.system
    }

    /// Approximate retained transcript size in bytes (for the byte budget).
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    fn enforce_context(&mut self) -> Option<AgentEvent> {
        // Token-based compaction: compact when estimated tokens exceed
        // `context_window - reserve_tokens`; keep a recent tail worth `reserve`.
        if let Some((window, reserve)) = self.compaction {
            let threshold = window.saturating_sub(reserve);
            // The kept tail must be strictly smaller than the threshold, or the
            // next check compacts again immediately (pathological churn). Cap it
            // at half the threshold.
            let tail_budget = reserve.min(threshold / 2).max(1);
            if self.total_tokens() > threshold {
                if let Some(event) = self.compact_tail(tail_budget) {
                    return Some(event);
                }
            }
        }

        if let Some(window) = self.context_window {
            if self.messages.len() > window {
                let excess = self.messages.len() - window;
                for message in self.messages.drain(0..excess) {
                    self.retained_bytes =
                        self.retained_bytes.saturating_sub(message.approx_bytes());
                    self.retained_tokens =
                        self.retained_tokens.saturating_sub(message.approx_tokens());
                }
            }
        }
        if let Some(budget) = self.context_bytes {
            while self.retained_bytes > budget && self.messages.len() > 1 {
                let message = self.messages.remove(0);
                self.retained_bytes = self.retained_bytes.saturating_sub(message.approx_bytes());
                self.retained_tokens = self.retained_tokens.saturating_sub(message.approx_tokens());
            }
        }
        None
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
            if let Some(usage) = &turn.usage {
                on_event(&AgentEvent::Usage(usage.clone()));
            }

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
                self.push(TranscriptMessage::Assistant(blocks));
            }
            if let Some(event) = self.enforce_context() {
                on_event(&event);
            }

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
                self.push(TranscriptMessage::ToolResult {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: vec![ContentPart::Text {
                        text: result.content,
                    }],
                    is_error: result.is_error,
                });
            }
            if let Some(event) = self.enforce_context() {
                on_event(&event);
            }
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
