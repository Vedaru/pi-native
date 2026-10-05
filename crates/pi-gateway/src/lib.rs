//! Minimal HTTP + SSE gateway over [`pi_host::Host`].
//!
//! This is the transport the web UI attaches to. It is intentionally small and
//! dependency-free (a blocking `std::net` server), and its routes mirror the
//! subset of pi-web's `/api/agent/*` surface that the browser needs:
//!
//! | Method | Path | Meaning |
//! | --- | --- | --- |
//! | `GET` | `/sessions` | List running session ids |
//! | `GET` | `/swarm` | Status snapshot of every unit |
//! | `POST` | `/sessions` | Open/create a session (`{"sessionPath"?: "…", "cwd"?: "…"}`) |
//! | `GET` | `/sessions/:id` | Resolve state (subscribe → `get_state` → `state`) |
//! | `GET` | `/sessions/:id/commands` | Extension slash commands |
//! | `POST` | `/sessions/:id/title` | Generate a session title from the transcript |
//! | `GET` | `/sessions/:id/events` | SSE: replay + live (`?format=pi` for pi's shapes) |
//! | `POST` | `/sessions/:id/commands` | Send a command (202 Accepted) |
//! | `POST` | `/sessions/:id/ui_response` | Answer a `ui_request` |
//!
//! Events stream as `data: <json>\n\n`; the native event envelope is used
//! unless `?format=pi` asks for pi's canonical stream through
//! [`pi_rpc::PiEventAdapter`].

use pi_host::{Host, HostError, RecvError, Subscription};
use pi_rpc::{Event, PiEventAdapter};
use pi_triggers::{Episode, RunStore, Runner};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// How long an idle SSE connection waits before emitting a keep-alive comment.
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);
/// How long a state request waits for the unit to answer.
const STATE_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest request body the gateway will buffer; larger requests get a 413.
const MAX_BODY: usize = 1 << 20;

/// Shared, thread-safe host plus the routing logic.
pub struct Gateway {
    host: Mutex<Host>,
    triggers: Option<Mutex<TriggerRuntime>>,
}

struct TriggerRuntime {
    runner: Runner,
    store: Box<dyn RunStore>,
}

impl Gateway {
    pub fn new(host: Host) -> Self {
        Self {
            host: Mutex::new(host),
            triggers: None,
        }
    }

    /// Attach a trigger runner and its durable store. Call
    /// [`Gateway::spawn_trigger_loop`] to drive it.
    pub fn with_triggers(host: Host, runner: Runner, store: Box<dyn RunStore>) -> Self {
        Self {
            host: Mutex::new(host),
            triggers: Some(Mutex::new(TriggerRuntime { runner, store })),
        }
    }

    /// Fire every due trigger at `now` (unix seconds); returns the episodes.
    pub fn tick_triggers(&self, now: i64) -> Vec<Episode> {
        let Some(triggers) = &self.triggers else {
            return Vec::new();
        };
        let mut runtime = triggers.lock().unwrap_or_else(|error| error.into_inner());
        let mut host = self.host.lock().unwrap_or_else(|error| error.into_inner());
        let TriggerRuntime { runner, store } = &mut *runtime;
        runner.tick(&mut host, store.as_mut(), now)
    }

    /// Start a background thread that ticks triggers every `interval`.
    pub fn spawn_trigger_loop(self: &Arc<Self>, interval: Duration) {
        let gateway = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(interval);
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let _ = gateway.tick_triggers(now);
        });
    }

    /// Periodically suspend units idle for `idle`, releasing their agents.
    pub fn spawn_idle_reaper(self: &Arc<Self>, interval: Duration, idle: Duration) {
        let gateway = self.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(interval);
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let _ = gateway.host().reap_idle(idle, now);
        });
    }

    fn host(&self) -> MutexGuard<'_, Host> {
        self.host.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// A running gateway server. Dropping it does not stop the accept thread, but
/// tests use ephemeral ports and short-lived processes.
pub struct GatewayServer {
    pub addr: SocketAddr,
}

/// Bind `127.0.0.1:0` and start serving on a background thread.
pub fn bind(host: Host) -> std::io::Result<GatewayServer> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let addr = listener.local_addr()?;
    let gateway = Arc::new(Gateway::new(host));
    std::thread::spawn(move || serve(listener, gateway));
    Ok(GatewayServer { addr })
}

/// Accept loop. Each connection is handled on its own thread so a long SSE
/// stream never blocks other requests.
pub fn serve(listener: TcpListener, gateway: Arc<Gateway>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        let gateway = gateway.clone();
        std::thread::spawn(move || {
            let _ = handle_connection(stream, gateway);
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

/// Why `read_request` stopped reading.
enum ReadOutcome {
    Request(Request),
    /// The peer closed before sending a full request.
    Closed,
    /// `Content-Length` (or the header block) exceeded [`MAX_BODY`].
    TooLarge,
}

impl Request {
    fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<ReadOutcome> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(ReadOutcome::Closed);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 64 * 1024 {
            return Ok(ReadOutcome::Closed);
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    let mut content_length = 0usize;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    if content_length > MAX_BODY {
        return Ok(ReadOutcome::TooLarge);
    }
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(ReadOutcome::Request(Request {
        method,
        path,
        query,
        body,
    }))
}

fn write_headers(stream: &mut TcpStream, status: u16, content_type: &str, length: usize) {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(head.as_bytes());
}

fn write_json(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = serde_json::to_vec(value).unwrap_or_default();
    write_headers(stream, status, "application/json", body.len());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

fn status_for(error: &HostError) -> u16 {
    match error {
        HostError::UnknownSession(_) => 404,
        _ => 500,
    }
}

fn handle_connection(mut stream: TcpStream, gateway: Arc<Gateway>) -> std::io::Result<()> {
    let request = match read_request(&mut stream)? {
        ReadOutcome::Request(request) => request,
        ReadOutcome::Closed => return Ok(()),
        ReadOutcome::TooLarge => {
            write_json(
                &mut stream,
                413,
                &json!({ "error": "request body too large" }),
            );
            return Ok(());
        }
    };
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["sessions"]) => {
            let ids = gateway.host().session_ids();
            write_json(&mut stream, 200, &json!({ "sessions": ids }));
        }
        ("GET", ["swarm"]) => {
            let host = gateway.host();
            let units = host.swarm();
            let cwd = host.cwd().to_string();
            drop(host);
            write_json(&mut stream, 200, &json!({ "units": units, "cwd": cwd }));
        }
        ("POST", ["sessions"]) => create_session(&mut stream, &gateway, &request),
        ("GET", ["sessions", id]) => session_state(&mut stream, &gateway, id),
        ("GET", ["sessions", id, "commands"]) => session_commands(&mut stream, &gateway, id),
        ("POST", ["sessions", id, "title"]) => session_title(&mut stream, &gateway, id),
        ("GET", ["sessions", id, "events"]) => {
            stream_events(stream, &gateway, id, request.query.contains("format=pi"))?;
        }
        ("POST", ["sessions", id, "commands"]) => send_command(&mut stream, &gateway, id, &request),
        ("POST", ["sessions", id, "ui_response"]) => {
            ui_response(&mut stream, &gateway, id, &request)
        }
        _ => {
            write_json(&mut stream, 404, &json!({ "error": "not found" }));
        }
    }
    Ok(())
}

fn create_session(stream: &mut TcpStream, gateway: &Gateway, request: &Request) {
    let body = request.json().unwrap_or(Value::Null);
    let path = match body.get("sessionPath").and_then(Value::as_str) {
        Some(path) => PathBuf::from(path),
        None => {
            let cwd = body
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| gateway.host().cwd().to_string());
            let unit = body.get("unit").and_then(Value::as_str);
            match pi_agent::new_session_path_with_unit(std::path::Path::new(&cwd), unit) {
                Ok(path) => path,
                Err(error) => {
                    write_json(stream, 500, &json!({ "error": error.to_string() }));
                    return;
                }
            }
        }
    };
    let opened = gateway.host().open(path.clone());
    match opened {
        Ok(id) => write_json(
            stream,
            201,
            &json!({ "sessionId": id, "sessionPath": path.to_string_lossy() }),
        ),
        Err(error) => write_json(
            stream,
            status_for(&error),
            &json!({ "error": error.to_string() }),
        ),
    }
}

/// Subscribe to a unit, or write the error and return `None`. Binding the
/// result drops the host lock before the response is written to the socket.
fn subscribe_or_error(stream: &mut TcpStream, gateway: &Gateway, id: &str) -> Option<Subscription> {
    let subscribed = gateway.host().subscribe(id);
    match subscribed {
        Ok(subscription) => Some(subscription),
        Err(error) => {
            write_json(
                stream,
                status_for(&error),
                &json!({ "error": error.to_string() }),
            );
            None
        }
    }
}

/// Send a command, or write the error and return `false`.
fn send_or_error(stream: &mut TcpStream, gateway: &Gateway, id: &str, command: Value) -> bool {
    let sent = gateway.host().send(id, command);
    match sent {
        Ok(()) => true,
        Err(error) => {
            write_json(
                stream,
                status_for(&error),
                &json!({ "error": error.to_string() }),
            );
            false
        }
    }
}

fn session_state(stream: &mut TcpStream, gateway: &Gateway, id: &str) {
    let Some(subscription) = subscribe_or_error(stream, gateway, id) else {
        return;
    };
    if !send_or_error(stream, gateway, id, json!({ "type": "get_state" })) {
        return;
    }
    write_json(stream, 200, &await_state(&subscription));
}

/// Wait for the `state` event emitted in response to `get_state`.
fn await_state(subscription: &Subscription) -> Value {
    await_response(subscription, "state", |event| {
        event.get("type").and_then(Value::as_str) == Some("state")
    })
}

fn session_commands(stream: &mut TcpStream, gateway: &Gateway, id: &str) {
    let Some(subscription) = subscribe_or_error(stream, gateway, id) else {
        return;
    };
    if !send_or_error(stream, gateway, id, json!({ "type": "get_commands" })) {
        return;
    }
    let data = await_response(&subscription, "get_commands", |event| {
        event.get("type").and_then(Value::as_str) == Some("response")
            && event.get("command").and_then(Value::as_str) == Some("get_commands")
    });
    write_json(stream, 200, &data);
}

/// Generate a session title using the unit's own provider.
fn session_title(stream: &mut TcpStream, gateway: &Gateway, id: &str) {
    let Some(subscription) = subscribe_or_error(stream, gateway, id) else {
        return;
    };
    if !send_or_error(stream, gateway, id, json!({ "type": "generate_title" })) {
        return;
    }
    let data = await_response(&subscription, "generate_title", |event| {
        event.get("type").and_then(Value::as_str) == Some("response")
            && event.get("command").and_then(Value::as_str) == Some("generate_title")
    });
    write_json(stream, 200, &data);
}

/// Wait for the first event matching `predicate`, returning its data payload.
fn await_response(
    subscription: &Subscription,
    label: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + STATE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return json!({ "error": format!("timed out waiting for {label}") });
        }
        match subscription.recv_timeout(remaining) {
            Ok(event) if predicate(&event) => {
                // A `state` event is the payload; a `response` carries it in `data`.
                if event.get("type").and_then(Value::as_str) == Some("response") {
                    return event.get("data").cloned().unwrap_or(Value::Null);
                }
                return event;
            }
            Ok(_) => continue,
            Err(RecvError::Timeout) => {
                return json!({ "error": format!("timed out waiting for {label}") })
            }
            Err(RecvError::Disconnected) => {
                return json!({ "error": format!("unit closed before {label} arrived") })
            }
        }
    }
}

fn send_command(stream: &mut TcpStream, gateway: &Gateway, id: &str, request: &Request) {
    let command = request.json().unwrap_or(Value::Null);
    if !command.is_object() {
        write_json(
            stream,
            400,
            &json!({ "error": "command must be a JSON object" }),
        );
        return;
    }
    if send_or_error(stream, gateway, id, command) {
        write_json(stream, 202, &json!({ "accepted": true }));
    }
}

fn ui_response(stream: &mut TcpStream, gateway: &Gateway, id: &str, request: &Request) {
    let body = request.json().unwrap_or(Value::Null);
    let Some(request_id) = body.get("id").and_then(Value::as_str) else {
        write_json(stream, 400, &json!({ "error": "id is required" }));
        return;
    };
    let value = body.get("value").cloned().unwrap_or(Value::Bool(false));
    let command = json!({ "type": "ui_response", "id": request_id, "value": value });
    if send_or_error(stream, gateway, id, command) {
        write_json(stream, 202, &json!({ "accepted": true }));
    }
}

fn stream_events(
    mut stream: TcpStream,
    gateway: &Gateway,
    id: &str,
    pi_format: bool,
) -> std::io::Result<()> {
    let Some(subscription) = subscribe_or_error(&mut stream, gateway, id) else {
        return Ok(());
    };
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.flush()?;

    let mut adapter = PiEventAdapter::new();
    // Snapshot first, then live events; the host registers the subscriber and
    // snapshots the replay atomically, so there is no gap or duplicate.
    for event in subscription.replay.clone() {
        let payloads = if pi_format {
            translate(&mut adapter, &event)
        } else {
            vec![event]
        };
        for payload in payloads {
            let line = format!(
                "data: {}\n\n",
                serde_json::to_string(&payload).unwrap_or_default()
            );
            if stream.write_all(line.as_bytes()).is_err() {
                return Ok(());
            }
        }
    }
    if stream.flush().is_err() {
        return Ok(());
    }
    loop {
        match subscription.recv_timeout(SSE_HEARTBEAT) {
            Ok(event) => {
                let payloads = if pi_format {
                    translate(&mut adapter, &event)
                } else {
                    vec![event]
                };
                for payload in payloads {
                    let line = format!(
                        "data: {}\n\n",
                        serde_json::to_string(&payload).unwrap_or_default()
                    );
                    if stream.write_all(line.as_bytes()).is_err() {
                        return Ok(());
                    }
                }
                if stream.flush().is_err() {
                    return Ok(());
                }
            }
            Err(RecvError::Timeout) => {
                if stream.write_all(b": keepalive\n\n").is_err() {
                    return Ok(());
                }
                let _ = stream.flush();
            }
            // The unit was suspended or its subscriber lagged and was dropped:
            // close the stream instead of heartbeating a dead unit forever.
            Err(RecvError::Disconnected) => return Ok(()),
        }
    }
}

/// Fold a native event into pi's shapes; pass unknown values through unchanged.
fn translate(adapter: &mut PiEventAdapter, value: &Value) -> Vec<Value> {
    match serde_json::from_value::<Event>(value.clone()) {
        Ok(event) => adapter.translate(&event),
        Err(_) => vec![value.clone()],
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
