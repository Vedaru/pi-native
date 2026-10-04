use super::*;
use pi_tools::{BashTool, Tool, ToolContext, ToolResult};
use serde_json::json;
use std::cell::RefCell;

/// Returns pre-scripted turns in order.
struct FakeProvider {
    turns: RefCell<Vec<AssistantTurn>>,
}

impl FakeProvider {
    fn new(turns: Vec<AssistantTurn>) -> Self {
        Self {
            turns: RefCell::new(turns),
        }
    }
}

impl ModelProvider for FakeProvider {
    fn complete(&self, _request: &CompletionRequest<'_>) -> Result<AssistantTurn, AgentError> {
        let mut turns = self.turns.borrow_mut();
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

    assert!(events.contains(&AgentEvent::ToolStart {
        name: "echo".into(),
        input: json!({ "text": "hi" })
    }));
    assert!(events.contains(&AgentEvent::ToolEnd {
        name: "echo".into(),
        is_error: false,
        content: "echoed: hi".into()
    }));
    assert!(events.contains(&AgentEvent::AssistantText("done".into())));
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            stop_reason: Some("end_turn".into())
        })
    );

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
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            stop_reason: Some("max_iterations".into())
        })
    );
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
fn denied_approval_blocks_an_approval_tool() {
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
    )
    .with_approver(std::sync::Arc::new(DenyAll));
    agent.push_user("go");

    let events = agent.run().expect("runs");
    let (is_error, content) = tool_end(&events);
    assert!(is_error, "denied tool should be an error: {content}");
    assert!(content.contains("denied"), "{content}");
    // The loop continues after a denial.
    assert!(events.contains(&AgentEvent::AssistantText("ok".into())));
}

#[test]
fn allowed_approval_runs_the_tool() {
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
    )
    .with_approver(std::sync::Arc::new(AllowAll));
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
