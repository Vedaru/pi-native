use super::*;
use pi_agent::{Agent, AssistantTurn, FauxProvider, ToolContext};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pi-gateway-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn start_gateway(dir: &Path, text: &str) -> GatewayServer {
    let turns = vec![AssistantTurn {
        text: text.to_string(),
        stop_reason: Some("end_turn".to_string()),
        ..Default::default()
    }];
    let cwd = dir.to_path_buf();
    let host = Host::new(cwd.to_string_lossy().to_string(), move |unit_cwd: &str| {
        Agent::new(
            Box::new(FauxProvider::new(turns.clone())),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
    });
    bind(host).expect("bind")
}

/// A blocking one-shot HTTP request (the server closes the connection).
fn request(addr: SocketAddr, method: &str, path: &str, body: Option<Value>) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let body = body
        .map(|value| serde_json::to_vec(&value).unwrap())
        .unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    stream.write_all(&body).expect("write body");
    stream.flush().expect("flush");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    (status, body)
}

fn create_session(server: &GatewayServer, dir: &Path) -> String {
    let path = dir.join("s.jsonl");
    let (status, body) = request(
        server.addr,
        "POST",
        "/sessions",
        Some(json!({ "sessionPath": path.to_string_lossy() })),
    );
    assert_eq!(status, 201, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

/// Open an SSE connection and consume the response headers.
fn open_sse(addr: SocketAddr, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let head =
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n");
    stream.write_all(head.as_bytes()).expect("write");
    stream.flush().expect("flush");
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while !seen.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).expect("read header");
        if read == 0 {
            break;
        }
        seen.push(byte[0]);
    }
    stream
}

/// Read SSE bytes until `needle` appears, or the timeout elapses.
fn wait_for_sse(stream: &mut TcpStream, needle: &str, timeout: Duration) -> String {
    stream.set_read_timeout(Some(timeout)).expect("set timeout");
    let mut accumulated = String::new();
    let mut buffer = [0u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                accumulated.push_str(&String::from_utf8_lossy(&buffer[..n]));
                if accumulated.contains(needle) {
                    return accumulated;
                }
            }
            Err(_) => break,
        }
    }
    accumulated
}

#[test]
fn creates_lists_and_resolves_a_session() {
    let dir = temp_dir("create");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    let (status, body) = request(server.addr, "GET", "/sessions", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(&id), "{body}");

    let (status, body) = request(server.addr, "GET", &format!("/sessions/{id}"), None);
    assert_eq!(status, 200, "{body}");
    let state: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(state["type"], json!("state"), "{state}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn streams_native_events_over_sse() {
    let dir = temp_dir("sse");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    let mut sse = open_sse(server.addr, &format!("/sessions/{id}/events"));
    let (status, body) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "prompt", "text": "go" })),
    );
    assert_eq!(status, 202, "{body}");

    let text = wait_for_sse(&mut sse, "\"done\"", Duration::from_secs(3));
    assert!(text.contains("assistant_text"), "{text}");
    assert!(text.contains("\"type\":\"done\""), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pi_format_emits_pi_event_shapes() {
    let dir = temp_dir("pi-format");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    let mut sse = open_sse(server.addr, &format!("/sessions/{id}/events?format=pi"));
    let (status, _) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "prompt", "text": "go" })),
    );
    assert_eq!(status, 202);

    let text = wait_for_sse(&mut sse, "\"agent_settled\"", Duration::from_secs(3));
    assert!(text.contains("message_start"), "{text}");
    assert!(text.contains("text_delta"), "{text}");
    assert!(text.contains("message_end"), "{text}");
    assert!(text.contains("agent_start"), "{text}");
    assert!(text.contains("turn_end"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_session_is_404() {
    let dir = temp_dir("unknown");
    let server = start_gateway(&dir, "hello");
    let (status, body) = request(server.addr, "GET", "/sessions/missing", None);
    assert_eq!(status, 404, "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn triggers_fire_through_the_gateway() {
    let dir = temp_dir("triggers");
    let turns = vec![AssistantTurn {
        text: "ok".to_string(),
        stop_reason: Some("end_turn".to_string()),
        ..Default::default()
    }];
    let cwd = dir.clone();
    let host = Host::new(cwd.to_string_lossy().to_string(), move |unit_cwd: &str| {
        Agent::new(
            Box::new(FauxProvider::new(turns.clone())),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
    });
    let runner = pi_triggers::Runner::new(
        vec![pi_triggers::Trigger::interval(
            "t",
            Duration::from_secs(60),
            "go",
        )],
        dir.join("sessions"),
    );
    let gateway =
        Gateway::with_triggers(host, runner, Box::new(pi_triggers::InMemoryRuns::default()));
    let episodes = gateway.tick_triggers(1_000_000);
    assert_eq!(episodes.len(), 1);
    assert!(episodes[0].session_path.exists());
    // Same minute/interval: not due again.
    assert!(gateway.tick_triggers(1_000_000).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reports_extension_commands() {
    let dir = temp_dir("commands");
    let cwd = dir.clone();
    let host = Host::new(cwd.to_string_lossy().to_string(), move |unit_cwd: &str| {
        Agent::new(
            Box::new(FauxProvider::new(Vec::new())),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
        .with_commands(vec![json!({
            "name": "demo",
            "description": "Demo command",
            "source": "extension",
        })])
    });
    let server = bind(host).expect("bind");
    let id = create_session(&server, &dir);

    let (status, body) = request(
        server.addr,
        "GET",
        &format!("/sessions/{id}/commands"),
        None,
    );
    assert_eq!(status, 200, "{body}");
    let data: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(data["commands"][0]["name"], json!("demo"));
    assert_eq!(data["commands"][0]["source"], json!("extension"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn swarm_lists_units_with_status() {
    let dir = temp_dir("swarm");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    let (status, body) = request(server.addr, "GET", "/swarm", None);
    assert_eq!(status, 200, "{body}");
    let data: Value = serde_json::from_str(&body).unwrap();
    let units = data["units"].as_array().expect("units");
    assert_eq!(units.len(), 1, "{data}");
    assert_eq!(units[0]["sessionId"], json!(id));
    assert_eq!(units[0]["running"], json!(true));
    assert!(units[0]["lastEventAt"].as_i64().unwrap_or(0) > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn generates_a_session_title() {
    let dir = temp_dir("title");
    let server = start_gateway(&dir, "My Session Title");
    let id = create_session(&server, &dir);

    let (status, body) = request(server.addr, "POST", &format!("/sessions/{id}/title"), None);
    assert_eq!(status, 200, "{body}");
    let data: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(data["title"], json!("My Session Title"));
    let _ = std::fs::remove_dir_all(&dir);
}
