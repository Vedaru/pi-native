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
    start_gateway_with_token(dir, text, None)
}

fn start_gateway_with_token(dir: &Path, text: &str, token: Option<&str>) -> GatewayServer {
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let gateway = Arc::new(Gateway::new(host).with_token(token.map(str::to_string)));
    std::thread::spawn(move || serve(listener, gateway));
    GatewayServer { addr }
}

/// A gateway in single-unit mode (VED-420): the shipped serve hosts one unit,
/// so a later `POST /sessions` replaces the open unit rather than adding one.
fn start_single_unit_gateway(dir: &Path, text: &str, session: &Path) -> GatewayServer {
    let turns = vec![AssistantTurn {
        text: text.to_string(),
        stop_reason: Some("end_turn".to_string()),
        ..Default::default()
    }];
    let cwd = dir.to_path_buf();
    let mut host = Host::new(cwd.to_string_lossy().to_string(), move |unit_cwd: &str| {
        Agent::new(
            Box::new(FauxProvider::new(turns.clone())),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
    });
    host.open(session.to_path_buf()).expect("open the one unit");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    let gateway = Arc::new(Gateway::new(host).single_unit());
    std::thread::spawn(move || serve(listener, gateway));
    GatewayServer { addr }
}

/// A blocking one-shot HTTP request (the server closes the connection).
fn request(addr: SocketAddr, method: &str, path: &str, body: Option<Value>) -> (u16, String) {
    request_with_auth(addr, method, path, body, None)
}

/// Like [`request`], but attaching `Authorization: Bearer <token>` when set.
fn request_with_auth(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let body = body
        .map(|value| serde_json::to_vec(&value).unwrap())
        .unwrap_or_default();
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n",
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
fn deletes_a_session_and_404s_afterwards() {
    let dir = temp_dir("delete");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    let (status, body) = request(server.addr, "DELETE", &format!("/sessions/{id}"), None);
    assert_eq!(status, 200, "{body}");
    let data: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(data["removed"], json!(true));

    // Gone from the listing and no longer resolvable.
    let (status, body) = request(server.addr, "GET", "/sessions", None);
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains(&id), "{body}");
    let (status, body) = request(server.addr, "GET", &format!("/sessions/{id}"), None);
    assert_eq!(status, 404, "{body}");

    // Deleting an unknown session is a 404, not a 500.
    let (status, body) = request(server.addr, "DELETE", "/sessions/missing", None);
    assert_eq!(status, 404, "{body}");
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
    assert!(data["cwd"].as_str().is_some(), "{data}");
    let units = data["units"].as_array().expect("units");
    assert_eq!(units.len(), 1, "{data}");
    assert_eq!(units[0]["sessionId"], json!(id));
    assert_eq!(units[0]["running"], json!(true));
    assert!(units[0].get("lastEventAt").is_some());
    assert!(units[0]["name"].is_null(), "{data}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn swarm_reports_a_renamed_unit() {
    let dir = temp_dir("swarm-name");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    let (status, _) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "set_session_name", "name": "Research" })),
    );
    assert_eq!(status, 202);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let (status, body) = request(server.addr, "GET", "/swarm", None);
        assert_eq!(status, 200, "{body}");
        let data: Value = serde_json::from_str(&body).unwrap();
        if data["units"][0]["name"] == json!("Research") {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("name was not persisted: {data}");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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

#[test]
fn resets_a_session_context_in_place() {
    let dir = temp_dir("reset");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    // Run a prompt so the session has context.
    let (status, _) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "prompt", "text": "work" })),
    );
    assert_eq!(status, 202);
    // Give the unit a moment to finish the turn before resetting.
    std::thread::sleep(Duration::from_millis(100));

    let (status, body) = request(server.addr, "POST", &format!("/sessions/{id}/reset"), None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"cleared\":true"), "{body}");
    assert!(body.contains("\"reran\":false"), "{body}");
    // The session id is unchanged, so the unit stays addressable.
    let (status, body) = request(server.addr, "GET", "/sessions", None);
    assert_eq!(status, 200);
    assert!(body.contains(&id), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Parse `id: <n>` lines from an SSE transcript, in order.
fn sse_ids(transcript: &str) -> Vec<u64> {
    transcript
        .lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .filter_map(|value| value.trim().parse().ok())
        .collect()
}

/// Open an SSE connection carrying a `Last-Event-ID` header.
fn open_sse_with_last_event_id(addr: SocketAddr, path: &str, last: u64) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    let head = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nLast-Event-ID: {last}\r\n\r\n"
    );
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

#[test]
fn sse_frames_carry_increasing_ids() {
    let dir = temp_dir("sse-ids");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    let mut sse = open_sse(server.addr, &format!("/sessions/{id}/events"));
    let (status, _) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "prompt", "text": "go" })),
    );
    assert_eq!(status, 202);

    let text = wait_for_sse(&mut sse, "\"done\"", Duration::from_secs(3));
    assert!(text.contains("id: "), "every frame needs an id: {text}");
    let ids = sse_ids(&text);
    assert!(ids.len() >= 2, "expected several ids: {ids:?}");
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "ids must increase: {ids:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_unauthenticated_requests_when_token_is_set() {
    let dir = temp_dir("auth-missing");
    let server = start_gateway_with_token(&dir, "hello", Some("secret"));

    // Every route is gated, including GETs and the POST control plane.
    for (method, path) in [
        ("GET", "/sessions"),
        ("GET", "/swarm"),
        ("GET", "/sessions/missing"),
        ("GET", "/sessions/missing/events"),
        ("POST", "/sessions"),
        ("DELETE", "/sessions/missing"),
        ("POST", "/sessions/missing/commands"),
        ("POST", "/sessions/missing/ui_response"),
    ] {
        let (status, body) = request(server.addr, method, path, Some(json!({})));
        assert_eq!(status, 401, "{method} {path} -> {status}: {body}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rejects_a_wrong_token() {
    let dir = temp_dir("auth-wrong");
    let server = start_gateway_with_token(&dir, "hello", Some("secret"));

    let (status, body) = request_with_auth(server.addr, "GET", "/sessions", None, Some("wrong"));
    assert_eq!(status, 401, "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn accepts_the_correct_token_on_every_transport() {
    let dir = temp_dir("auth-ok");
    let server = start_gateway_with_token(&dir, "hello", Some("secret"));

    let (status, body) = request_with_auth(server.addr, "GET", "/sessions", None, Some("secret"));
    assert_eq!(status, 200, "{body}");
    let data: Value = serde_json::from_str(&body).unwrap();
    assert!(data["sessions"].is_array(), "{data}");

    // `X-Pi-Token` works too.
    let mut stream = TcpStream::connect(server.addr).expect("connect");
    let head = "GET /sessions HTTP/1.1\r\nHost: localhost\r\nX-Pi-Token: secret\r\nConnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).expect("write");
    stream.flush().expect("flush");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    // The control plane works end to end with the token.
    let (status, body) = request_with_auth(
        server.addr,
        "POST",
        "/sessions",
        Some(json!({ "sessionPath": dir.join("s.jsonl").to_string_lossy() })),
        Some("secret"),
    );
    assert_eq!(status, 201, "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn last_event_id_replays_only_newer_events() {
    let dir = temp_dir("sse-resume");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    // First connection: run a turn and record the ids we saw.
    let mut first = open_sse(server.addr, &format!("/sessions/{id}/events"));
    let (status, _) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "prompt", "text": "go" })),
    );
    assert_eq!(status, 202);
    let text = wait_for_sse(&mut first, "\"done\"", Duration::from_secs(3));
    let ids = sse_ids(&text);
    let last_seen = *ids.last().expect("at least one id");
    drop(first);

    // Reconnect from the last id we saw: the buffered replay must be skipped.
    let mut resumed =
        open_sse_with_last_event_id(server.addr, &format!("/sessions/{id}/events"), last_seen);
    // The first thing on the resumed stream must be strictly newer than last_seen.
    let resumed_text = wait_for_sse(&mut resumed, ": keepalive", Duration::from_millis(500));
    let resumed_ids = sse_ids(&resumed_text);
    assert!(
        resumed_ids.iter().all(|id| *id > last_seen),
        "resume replayed old events: last_seen={last_seen}, got={resumed_ids:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Read SSE bytes for `timeout`, returning everything received (draining the
/// whole replay buffer rather than stopping at the first match).
fn drain_sse(stream: &mut TcpStream, timeout: Duration) -> String {
    stream.set_read_timeout(Some(timeout)).expect("set timeout");
    let mut accumulated = String::new();
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => accumulated.push_str(&String::from_utf8_lossy(&buffer[..n])),
            Err(_) => break,
        }
    }
    accumulated
}

#[test]
fn rolled_over_replay_emits_a_resync_signal() {
    let dir = temp_dir("sse-gap");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    // Pump cheap `get_state` commands until the replay buffer has rolled over,
    // i.e. the unit has published more than `REPLAY_LIMIT` events. Each emits a
    // `state` event and returns 202 as soon as it is queued.
    let pump = pi_host::REPLAY_LIMIT + 64;
    for _ in 0..pump {
        let (status, _) = request(
            server.addr,
            "POST",
            &format!("/sessions/{id}/commands"),
            Some(json!({ "type": "get_state" })),
        );
        assert_eq!(status, 202);
    }

    // Wait until the buffer has actually rolled: the full replay must include an
    // id past the original limit. Drain from id 0 until we see it.
    let rolled = |server: &GatewayServer, id: &str| -> bool {
        let mut probe = open_sse(server.addr, &format!("/sessions/{id}/events"));
        let text = drain_sse(&mut probe, Duration::from_millis(500));
        sse_ids(&text)
            .last()
            .is_some_and(|last| *last > pi_host::REPLAY_LIMIT as u64)
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !rolled(&server, &id) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        rolled(&server, &id),
        "replay buffer never rolled over; cannot test the gap"
    );

    // Reconnect from id 0: the events in between are gone, so the stream must
    // emit an explicit resync frame rather than silently dropping them.
    let mut gap = open_sse_with_last_event_id(server.addr, &format!("/sessions/{id}/events"), 0);
    let gap_text = wait_for_sse(&mut gap, "event: resync", Duration::from_secs(1));
    assert!(
        gap_text.contains("event: resync"),
        "expected an explicit resync frame, got: {gap_text}"
    );
    assert!(
        gap_text.contains("replay_buffer_rolled_over"),
        "resync frame should explain the gap: {gap_text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- VED-386 / VED-387: shared blocking provider -------------------------

/// A gate the test opens to let a blocked provider continue.
type Gate = std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

fn new_gate() -> Gate {
    std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()))
}

fn open_gate(gate: &Gate) {
    let (lock, cv) = &**gate;
    *lock.lock().expect("gate lock") = true;
    cv.notify_all();
}

/// A provider whose first call blocks until the test opens the gate, so the
/// unit stays mid-turn (with no further events) until released.
struct BlockingProvider {
    gate: Gate,
    blocked: std::sync::atomic::AtomicBool,
    text: String,
}

impl pi_agent::ModelProvider for BlockingProvider {
    fn complete(
        &self,
        _request: &pi_agent::CompletionRequest<'_>,
    ) -> Result<AssistantTurn, pi_agent::AgentError> {
        if !self.blocked.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let (lock, cv) = &*self.gate;
            let mut released = lock.lock().expect("gate lock");
            while !*released {
                released = cv.wait(released).expect("gate wait");
            }
        }
        Ok(AssistantTurn {
            text: self.text.clone(),
            stop_reason: Some("end_turn".to_string()),
            ..Default::default()
        })
    }
}

/// Start a gateway whose unit blocks on its first model call until the gate is
/// opened.
fn start_blocking_gateway(dir: &Path) -> (GatewayServer, Gate) {
    let gate = new_gate();
    let provider_gate = gate.clone();
    let cwd = dir.to_path_buf();
    let host = Host::new(cwd.to_string_lossy().to_string(), move |unit_cwd: &str| {
        Agent::new(
            Box::new(BlockingProvider {
                gate: provider_gate.clone(),
                blocked: std::sync::atomic::AtomicBool::new(false),
                text: "released".to_string(),
            }),
            Vec::new(),
            "system",
            ToolContext::new(unit_cwd),
        )
    });
    (bind(host).expect("bind"), gate)
}

/// A unit mid-turn is busy, not broken: `GET /sessions/:id` must report
/// `busy: true`, not `error: timed out` (VED-386).
#[test]
fn get_state_reports_busy_for_a_unit_mid_turn() {
    let dir = temp_dir("busy");
    let (server, release) = start_blocking_gateway(&dir);
    let id = create_session(&server, &dir);

    // Start a turn and wait until the unit is actually inside the model call:
    // the `agent_start` event is published before `complete` blocks.
    let mut sse = open_sse(server.addr, &format!("/sessions/{id}/events"));
    let (status, body) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "prompt", "text": "go" })),
    );
    assert_eq!(status, 202, "{body}");
    let seen = wait_for_sse(&mut sse, "agent_start", Duration::from_secs(3));
    assert!(seen.contains("agent_start"), "{seen}");

    // The bounded state wait expires because the unit is mid-turn. That is a
    // `busy` marker, not an error response.
    let (status, body) = request(server.addr, "GET", &format!("/sessions/{id}"), None);
    assert_eq!(status, 200, "{body}");
    let state: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(state["type"], json!("state"), "{state}");
    assert_eq!(state["busy"], json!(true), "{state}");
    assert!(
        state.get("error").is_none(),
        "must not be an error: {state}"
    );

    // Release the turn; the unit finishes and a later poll returns full state.
    open_gate(&release);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut resolved = None;
    while std::time::Instant::now() < deadline {
        let (status, body) = request(server.addr, "GET", &format!("/sessions/{id}"), None);
        assert_eq!(status, 200, "{body}");
        let state: Value = serde_json::from_str(&body).unwrap();
        if state["busy"] != json!(true) {
            resolved = Some(state);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let state = resolved.expect("unit never returned to a full state response");
    assert_eq!(state["type"], json!("state"), "{state}");
    assert!(
        state.get("system").is_some(),
        "full state carries the resolved context: {state}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether the unit reports `inFlight: true` on `/swarm`.
fn swarm_in_flight(addr: SocketAddr, id: &str) -> bool {
    let (status, body) = request(addr, "GET", "/swarm", None);
    assert_eq!(status, 200, "{body}");
    let payload: Value = serde_json::from_str(&body).unwrap();
    payload["units"]
        .as_array()
        .expect("units")
        .iter()
        .find(|unit| unit["sessionId"] == json!(id))
        .expect("unit in swarm")["inFlight"]
        .as_bool()
        .expect("inFlight bool")
}

/// A unit in a long silent turn reports `inFlight: true` on `/swarm`; an idle
/// unit reports `false` (VED-387).
#[test]
fn swarm_reports_in_flight_during_a_long_turn() {
    let dir = temp_dir("swarm-in-flight");
    let (server, gate) = start_blocking_gateway(&dir);
    let id = create_session(&server, &dir);

    // Idle before any work.
    assert!(!swarm_in_flight(server.addr, &id));

    let mut sse = open_sse(server.addr, &format!("/sessions/{id}/events"));
    let (status, body) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "prompt", "text": "go" })),
    );
    assert_eq!(status, 202, "{body}");
    let seen = wait_for_sse(&mut sse, "agent_start", Duration::from_secs(3));
    assert!(seen.contains("agent_start"), "{seen}");

    // Mid-turn with no further events: the explicit flag must say working.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !swarm_in_flight(server.addr, &id) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        swarm_in_flight(server.addr, &id),
        "/swarm must report inFlight while the unit is mid-turn"
    );

    // Release and let the run settle; the flag must clear.
    open_gate(&gate);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while swarm_in_flight(server.addr, &id) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !swarm_in_flight(server.addr, &id),
        "/swarm must report idle once the run settles"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The `state` payload carries the same flag (false for an idle unit).
#[test]
fn state_payload_carries_in_flight() {
    let dir = temp_dir("state-in-flight");
    let server = start_gateway(&dir, "hello");
    let id = create_session(&server, &dir);

    let (status, body) = request(server.addr, "GET", &format!("/sessions/{id}"), None);
    assert_eq!(status, 200, "{body}");
    let state: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(state["type"], json!("state"), "{state}");
    assert_eq!(state["inFlight"], json!(false), "{state}");

    let _ = std::fs::remove_dir_all(&dir);
}

// --- Single-unit serve (VED-420) ------------------------------------------

/// The shipped serve hosts one unit: the pi-web routes answer for its session,
/// and a later `POST /sessions` replaces the unit instead of adding one.
#[test]
fn single_unit_serve_answers_the_pi_web_routes() {
    let dir = temp_dir("single-unit");
    let server = start_single_unit_gateway(&dir, "hello", &dir.join("s.jsonl"));

    // `GET /sessions` lists exactly the one unit.
    let (status, body) = request(server.addr, "GET", "/sessions", None);
    assert_eq!(status, 200, "{body}");
    let listed: Value = serde_json::from_str(&body).unwrap();
    let ids = listed["sessions"].as_array().expect("sessions");
    assert_eq!(ids.len(), 1, "serves exactly one unit: {body}");
    let id = ids[0].as_str().expect("session id").to_string();

    // `GET /sessions/:id` resolves that unit.
    let (status, body) = request(server.addr, "GET", &format!("/sessions/{id}"), None);
    assert_eq!(status, 200, "{body}");
    let state: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(state["type"], json!("state"), "{state}");

    // SSE + `POST /sessions/:id/commands` drive that unit (pi-web's live path).
    let mut sse = open_sse(server.addr, &format!("/sessions/{id}/events?format=pi"));
    let (status, body) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/commands"),
        Some(json!({ "type": "prompt", "text": "go" })),
    );
    assert_eq!(status, 202, "{body}");
    let seen = wait_for_sse(&mut sse, "agent_settled", Duration::from_secs(3));
    assert!(
        seen.contains("message_start"),
        "SSE carried pi events: {seen}"
    );

    // `ui_response` is accepted for the one unit.
    let (status, body) = request(
        server.addr,
        "POST",
        &format!("/sessions/{id}/ui_response"),
        Some(json!({ "id": "d1", "value": true })),
    );
    assert_eq!(status, 202, "{body}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A second `POST /sessions` replaces the unit: the process still serves one.
#[test]
fn single_unit_serve_replaces_a_second_session() {
    let dir = temp_dir("single-unit-replace");
    let server = start_single_unit_gateway(&dir, "hello", &dir.join("first.jsonl"));

    let (status, body) = request(
        server.addr,
        "POST",
        "/sessions",
        Some(json!({ "sessionPath": dir.join("second.jsonl").to_string_lossy() })),
    );
    assert_eq!(status, 201, "{body}");
    let second = serde_json::from_str::<Value>(&body).unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = request(server.addr, "GET", "/sessions", None);
    assert_eq!(status, 200, "{body}");
    let listed: Value = serde_json::from_str(&body).unwrap();
    let ids = listed["sessions"].as_array().expect("sessions");
    assert_eq!(ids.len(), 1, "still exactly one unit: {body}");
    assert_eq!(ids[0], json!(second), "the newest session won: {body}");

    // The replaced unit's session file survives on disk.
    assert!(dir.join("first.jsonl").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
