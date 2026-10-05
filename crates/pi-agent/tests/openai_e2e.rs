//! End-to-end: agent loop over the native OpenAI Responses transport.

use pi_agent::{openai_responses_provider, Agent, AgentEvent};
use pi_tools::{default_tools, ToolContext};
use std::io::{Read, Write};
use std::net::TcpListener;

const FUNCTION_CALL_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
    "event: response.output_item.added\n",
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"bash\"}}\n\n",
    "event: response.function_call_arguments.delta\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"call_id\":\"call_1\",\"delta\":\"{\\\"command\\\":\\\"echo hi\\\"}\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
);

const TEXT_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r2\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"status\":\"completed\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1}}}\n\n",
);

fn spawn_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for body in [FUNCTION_CALL_SSE, TEXT_SSE] {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 8192];
            let _ = socket.read(&mut request);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

#[test]
fn openai_agent_executes_a_tool_then_finishes() {
    let base_url = spawn_server();
    let cwd = std::env::temp_dir().join(format!("pi-agent-openai-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("temp cwd");

    let provider = openai_responses_provider(base_url, "test-key", "gpt-5");
    let mut agent = Agent::new(
        Box::new(provider),
        default_tools(),
        "You are a test agent.",
        ToolContext::new(&cwd),
    );
    agent.push_user("run echo hi");

    let events = agent.run().expect("runs");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolStart { name, .. } if name == "bash")),
        "no bash tool start: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(event, AgentEvent::ToolEnd { name, content, is_error: false, .. } if name == "bash" && content.contains("hi"))),
        "bash did not run or return hi: {events:?}"
    );
    assert!(events.contains(&AgentEvent::AssistantText("done".into())));
}
