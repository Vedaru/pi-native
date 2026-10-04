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
fn get_state_reports_message_count() {
    let mut agent = agent_with(vec![AssistantTurn::default()]);
    let events = handle(&mut agent, Request::GetState);
    assert_eq!(events, vec![Event::State { messages: 0 }]);
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
