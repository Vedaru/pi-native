use super::*;
use pi_agent::{AssistantTurn, FauxProvider, ToolContext};
use serde_json::json;

fn agent_with(turns: Vec<AssistantTurn>) -> Agent {
    Agent::new(
        Box::new(FauxProvider::new(turns)),
        Vec::new(),
        "system",
        ToolContext::new(std::env::temp_dir()),
    )
}

#[test]
fn prompt_runs_the_agent_and_reports_events() {
    let mut agent = agent_with(vec![AssistantTurn {
        text: "hello".into(),
        stop_reason: Some("end_turn".into()),
        ..Default::default()
    }]);
    let events = handle(&mut agent, Request::Prompt { text: "hi".into() });
    assert_eq!(
        events,
        vec![
            Event::AssistantText {
                text: "hello".into()
            },
            Event::Done {
                stop_reason: Some("end_turn".into())
            }
        ]
    );
}

#[test]
fn get_state_reports_the_resolved_context() {
    let mut agent = agent_with(vec![AssistantTurn::default()]);
    agent.push_user("hello");
    let events = handle(&mut agent, Request::GetState);
    let Event::State {
        messages,
        system,
        transcript,
    } = &events[0]
    else {
        panic!("expected state, got {events:?}");
    };
    assert_eq!(*messages, 1);
    assert_eq!(system, "system");
    assert_eq!(transcript[0]["role"], serde_json::json!("user"));
    assert_eq!(transcript[0]["content"], serde_json::json!("hello"));
}

#[test]
fn serve_writes_ready_and_streams_events_as_json_lines() {
    let mut agent = agent_with(vec![AssistantTurn {
        text: "hi".into(),
        ..Default::default()
    }]);
    let input = concat!(
        "{\"type\":\"prompt\",\"text\":\"go\"}\n",
        "{\"type\":\"get_state\"}\n",
    );
    let mut output = Vec::new();
    serve(&mut agent, input.as_bytes(), &mut output).expect("serves");

    let lines: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid json"))
        .collect();

    assert_eq!(lines[0]["type"], json!("ready"));
    assert_eq!(lines[0]["version"], json!(1));
    assert!(lines
        .iter()
        .any(|line| line["type"] == json!("assistant_text")));
    assert!(lines.iter().any(|line| line["type"] == json!("done")));
    assert!(lines.iter().any(|line| line["type"] == json!("state")));
}

#[test]
fn invalid_request_yields_an_error_event() {
    let mut agent = agent_with(Vec::new());
    let mut output = Vec::new();
    serve(&mut agent, "{not json}\n".as_bytes(), &mut output).expect("serves");
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("\"type\":\"error\""), "{text}");
}

#[test]
fn serve_with_runs_the_hook_after_each_prompt() {
    let mut agent = agent_with(vec![AssistantTurn {
        text: "hi".into(),
        ..Default::default()
    }]);
    let input = "{\"type\":\"prompt\",\"text\":\"go\"}\n";
    let mut output = Vec::new();
    let mut counts = Vec::new();
    serve_with(&mut agent, input.as_bytes(), &mut output, |agent| {
        counts.push(agent.messages().len());
    })
    .expect("serves");
    // The hook sees the finished transcript: the user prompt and the reply.
    assert_eq!(counts, vec![2]);
}

#[test]
fn serve_unit_asks_the_client_before_approval_required_tools() {
    use pi_agent::ToolCall;
    use pi_tools::{default_tools, ToolContext};

    #[derive(Clone)]
    struct SharedBuf(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let turns = vec![
        AssistantTurn {
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: json!({ "command": "echo hi" }),
            }],
            stop_reason: Some("tool_use".into()),
            ..Default::default()
        },
        AssistantTurn {
            text: "done".into(),
            stop_reason: Some("end_turn".into()),
            ..Default::default()
        },
    ];
    let mut agent = Agent::new(
        Box::new(FauxProvider::new(turns)),
        default_tools(),
        "system",
        ToolContext::new(std::env::temp_dir()),
    );
    let input = concat!(
        "{\"type\":\"prompt\",\"text\":\"go\"}\n",
        "{\"type\":\"ui_response\",\"id\":\"ui-0\",\"value\":\"allow\"}\n",
    );
    let buf = SharedBuf(std::rc::Rc::new(std::cell::RefCell::new(Vec::new())));
    serve_unit(
        &mut agent,
        std::io::Cursor::new(input.as_bytes().to_vec()),
        buf.clone(),
    )
    .expect("serves");
    let text = String::from_utf8(buf.0.borrow().clone()).unwrap();
    assert!(text.contains("\"type\":\"ui_request\""), "{text}");
    assert!(text.contains("\"type\":\"tool_end\""), "{text}");
    assert!(!text.contains("\"is_error\":true"), "{text}");
}
