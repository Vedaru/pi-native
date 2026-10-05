use super::*;
use pi_tools::{BashTool, Tool, ToolContext, ToolResult};
use serde_json::json;
use std::sync::Mutex;

/// Returns pre-scripted turns in order.
struct FakeProvider {
    turns: Mutex<Vec<AssistantTurn>>,
}

impl FakeProvider {
    fn new(turns: Vec<AssistantTurn>) -> Self {
        Self {
            turns: Mutex::new(turns),
        }
    }
}

impl ModelProvider for FakeProvider {
    fn complete(&self, _request: &CompletionRequest<'_>) -> Result<AssistantTurn, AgentError> {
        let mut turns = self.turns.lock().expect("turns lock");
        if turns.is_empty() {
            Ok(AssistantTurn::default())
        } else {
            Ok(turns.remove(0))
        }
    }
}

struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }
    fn description(&self) -> &'static str {
        "Echo the `text` argument"
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] })
    }
    fn run(&self, input: &Value, _ctx: &ToolContext) -> ToolResult {
        match input.get("text").and_then(Value::as_str) {
            Some(text) => ToolResult::ok(format!("echoed: {text}")),
            None => ToolResult::error("echo: missing text"),
        }
    }
}

fn tool_call_turn() -> AssistantTurn {
    AssistantTurn {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: "call_1".into(),
            name: "echo".into(),
            arguments: json!({ "text": "hi" }),
        }],
        usage: None,
        stop_reason: Some("tool_use".into()),
    }
}

fn agent(provider: FakeProvider) -> Agent {
    Agent::new(
        Box::new(provider),
        vec![Box::new(EchoTool)],
        "system",
        ToolContext::new(std::env::temp_dir()),
    )
}

#[test]
fn runs_a_tool_call_then_finishes() {
    let provider = FakeProvider::new(vec![
        tool_call_turn(),
        AssistantTurn {
            text: "done".into(),
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        },
    ]);
    let mut agent = agent(provider);
    agent.push_user("go");

    let events = agent.run().expect("runs");

    assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolStart { name, input, .. } if name == "echo" && *input == json!({ "text": "hi" }))));
    assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolEnd { name, is_error: false, content, .. } if name == "echo" && content == "echoed: hi")));
    assert!(events.contains(&AgentEvent::AssistantText("done".into())));
    assert!(events.contains(&AgentEvent::Done {
        stop_reason: Some("end_turn".into())
    }));
    assert_eq!(events.last(), Some(&AgentEvent::AgentSettled));

    // user, assistant(tool call), tool result, assistant(text)
    assert_eq!(agent.messages().len(), 4);
    assert!(matches!(
        agent.messages().last(),
        Some(TranscriptMessage::Assistant(_))
    ));
}

#[test]
fn unknown_tool_is_reported_and_the_loop_continues() {
    let provider = FakeProvider::new(vec![
        AssistantTurn {
            tool_calls: vec![ToolCall {
                id: "call_9".into(),
                name: "nope".into(),
                arguments: json!({}),
            }],
            stop_reason: Some("tool_use".into()),
            ..Default::default()
        },
        AssistantTurn {
            text: "recovered".into(),
            ..Default::default()
        },
    ]);
    let mut agent = agent(provider);
    agent.push_user("go");

    let events = agent.run().expect("runs");
    let tool_end = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolEnd {
                name,
                is_error,
                content,
                ..
            } => Some((name.clone(), *is_error, content.clone())),
            _ => None,
        })
        .expect("a tool end event");
    assert!(tool_end.1, "unknown tool should be an error");
    assert!(tool_end.2.contains("unknown tool"));
    assert!(events.contains(&AgentEvent::AssistantText("recovered".into())));
}

#[test]
fn iteration_bound_stops_a_runaway_loop() {
    let provider = FakeProvider::new(vec![
        tool_call_turn(),
        tool_call_turn(),
        tool_call_turn(),
        tool_call_turn(),
    ]);
    let mut agent = agent(provider).with_max_iterations(3);
    agent.push_user("go");

    let events = agent.run().expect("runs");
    assert!(events.contains(&AgentEvent::Done {
        stop_reason: Some("max_iterations".into())
    }));
    assert_eq!(events.last(), Some(&AgentEvent::AgentSettled));
}

fn bash_turn() -> AssistantTurn {
    AssistantTurn {
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: json!({ "command": "echo hi" }),
        }],
        stop_reason: Some("tool_use".into()),
        ..Default::default()
    }
}

fn tool_end(events: &[AgentEvent]) -> (bool, String) {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolEnd {
                is_error, content, ..
            } => Some((*is_error, content.clone())),
            _ => None,
        })
        .expect("a tool end event")
}

#[test]
fn bash_tool_runs_without_approval() {
    let provider = FakeProvider::new(vec![
        bash_turn(),
        AssistantTurn {
            text: "ok".into(),
            ..Default::default()
        },
    ]);
    let mut agent = Agent::new(
        Box::new(provider),
        vec![Box::new(BashTool)],
        "s",
        ToolContext::new(std::env::temp_dir()),
    );
    agent.push_user("go");

    let events = agent.run().expect("runs");
    let (is_error, content) = tool_end(&events);
    assert!(!is_error, "{content}");
    assert!(content.contains("hi"), "{content}");
}

#[test]
fn context_window_bounds_retained_messages() {
    let provider = FnProvider::new(|index| {
        if index < 20 {
            AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: format!("c{index}"),
                    name: "echo".into(),
                    arguments: json!({ "text": "x" }),
                }],
                stop_reason: Some("tool_use".into()),
                ..Default::default()
            }
        } else {
            AssistantTurn {
                text: "done".into(),
                ..Default::default()
            }
        }
    });
    let mut agent = Agent::new(
        Box::new(provider),
        vec![Box::new(EchoTool)],
        "s",
        ToolContext::new(std::env::temp_dir()),
    )
    .with_max_iterations(30)
    .with_context_window(4);
    agent.push_user("go");

    agent.run().expect("runs");
    assert!(
        agent.messages().len() <= 4,
        "window not enforced: {}",
        agent.messages().len()
    );
}

#[test]
fn context_byte_limit_bounds_retained_output() {
    // Each tool result is ~8 KB; a 16 KB budget keeps only a couple of messages,
    // even though the message-count window is much larger.
    let provider = FnProvider::new(|index| {
        if index < 50 {
            AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: format!("c{index}"),
                    name: "echo".into(),
                    arguments: json!({ "text": "x".repeat(8000) }),
                }],
                stop_reason: Some("tool_use".into()),
                ..Default::default()
            }
        } else {
            AssistantTurn {
                text: "done".into(),
                ..Default::default()
            }
        }
    });
    let mut agent = Agent::new(
        Box::new(provider),
        vec![Box::new(EchoTool)],
        "s",
        ToolContext::new(std::env::temp_dir()),
    )
    .with_max_iterations(60)
    .with_context_window(4096)
    .with_context_byte_limit(16 * 1024);
    agent.push_user("go");

    agent.run().expect("runs");
    assert!(
        agent.messages().len() <= 4,
        "byte budget not enforced: {} messages",
        agent.messages().len()
    );
}

struct StubSummarizer;

impl Summarizer for StubSummarizer {
    fn summarize(&self, messages: &[TranscriptMessage]) -> Result<String, AgentError> {
        Ok(format!("summarized {} messages", messages.len()))
    }
}

#[test]
fn compaction_summarizes_older_messages() {
    let provider = FnProvider::new(|index| {
        if index < 30 {
            AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: format!("c{index}"),
                    name: "echo".into(),
                    arguments: json!({ "text": "x".repeat(400) }),
                }],
                stop_reason: Some("tool_use".into()),
                ..Default::default()
            }
        } else {
            AssistantTurn {
                text: "done".into(),
                ..Default::default()
            }
        }
    });
    let mut agent = Agent::new(
        Box::new(provider),
        vec![Box::new(EchoTool)],
        "s",
        ToolContext::new(std::env::temp_dir()),
    )
    .with_max_iterations(40)
    .with_compaction(2_000, 500)
    .with_summarizer(std::sync::Arc::new(StubSummarizer));
    agent.push_user("go");

    let mut compactions = 0usize;
    agent
        .run_with(|event| {
            if matches!(event, AgentEvent::Compacted { .. }) {
                compactions += 1;
            }
        })
        .expect("runs");

    assert!(compactions > 0, "compaction never ran");
    match &agent.messages()[0] {
        TranscriptMessage::UserText(text) => assert!(
            text.starts_with("[Earlier conversation summary]"),
            "first message is not a summary: {text}"
        ),
        other => panic!("expected summary, got {other:?}"),
    }
}

#[test]
fn compaction_does_not_churn_when_reserve_is_large() {
    // window 2000, reserve 1500 -> threshold 500. If the kept tail were allowed
    // to reach the reserve (1500 > threshold), every check would compact again.
    // With the tail capped at threshold/2, compaction happens occasionally.
    let provider = FnProvider::new(|index| {
        if index < 60 {
            AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: format!("c{index}"),
                    name: "echo".into(),
                    arguments: json!({ "text": "x".repeat(400) }),
                }],
                stop_reason: Some("tool_use".into()),
                ..Default::default()
            }
        } else {
            AssistantTurn {
                text: "done".into(),
                ..Default::default()
            }
        }
    });
    let mut agent = Agent::new(
        Box::new(provider),
        vec![Box::new(EchoTool)],
        "s",
        ToolContext::new(std::env::temp_dir()),
    )
    .with_max_iterations(70)
    .with_compaction(2_000, 1_500)
    .with_summarizer(std::sync::Arc::new(StubSummarizer));
    agent.push_user("go");

    let mut compactions = 0usize;
    agent
        .run_with(|event| {
            if matches!(event, AgentEvent::Compacted { .. }) {
                compactions += 1;
            }
        })
        .expect("runs");

    assert!(compactions > 0, "no compaction ran");
    assert!(
        compactions <= 60,
        "compaction churned: {compactions} over 60 turns"
    );
}

#[test]
fn extend_messages_seeds_the_transcript() {
    let mut agent = Agent::new(
        Box::new(FauxProvider::new(Vec::new())),
        Vec::new(),
        "system",
        ToolContext::new(std::env::temp_dir()),
    );
    agent.extend_messages([
        TranscriptMessage::UserText("hello".into()),
        TranscriptMessage::Assistant(vec![AssistantBlock::Text { text: "hi".into() }]),
    ]);
    assert_eq!(agent.messages().len(), 2);
    assert!(agent.retained_bytes() > 0);
}

#[test]
fn provider_summarizer_asks_the_model() {
    let provider = FauxProvider::new(vec![AssistantTurn {
        text: "SUMMARY".into(),
        ..Default::default()
    }]);
    let summarizer = ProviderSummarizer::new(Box::new(provider));
    let summary = summarizer
        .summarize(&[TranscriptMessage::UserText("old".into())])
        .expect("summary");
    assert_eq!(summary, "SUMMARY");
}
