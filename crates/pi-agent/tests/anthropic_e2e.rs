//! End-to-end: agent loop over the native Anthropic transport, against a local
//! mock server. Proves prompt -> SSE tool call -> tool execution -> second
//! request -> final text, with no Node and no network provider.

use pi_agent::{Agent, AgentEvent, AnthropicProvider};
use pi_tools::{default_tools, ToolContext};
use std::io::{Read, Write};
use std::net::TcpListener;

const TOOL_USE_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"echo hi\\\"}\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

const TEXT_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m2\",\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Serve two canned SSE responses, one per connection.
fn spawn_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for body in [TOOL_USE_SSE, TEXT_SSE] {
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
fn agent_executes_a_tool_then_finishes() {
    let base_url = spawn_server();
    let cwd = std::env::temp_dir().join(format!("pi-agent-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).expect("temp cwd");

    let provider = AnthropicProvider::new(base_url, "test-key", "claude-sonnet-4-5");
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
        events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolEnd { name, is_error: false, content } if name == "bash" && content.contains("hi")
        )),
        "bash did not run or did not return hi: {events:?}"
    );
    assert!(
        events.contains(&AgentEvent::AssistantText("done".into())),
        "final assistant text missing: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&AgentEvent::Done {
            stop_reason: Some("end_turn".into())
        })
    );
}
