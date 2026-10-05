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
//! | `GET` | `/units/:id/messages?after=<seq>` | Direct messages enqueued to a unit |
//! | `POST` | `/units/:id/messages` | Send a direct unit message |
//! | `POST` | `/units/:id/messages/:msgid/ack` | Acknowledge a direct message |
//! | `POST` | `/units/:id/ownership` | Transfer a unit's ownership |
//! | `POST` | `/sessions` | Open/create a session (`{"sessionPath"?: "…", "cwd"?: "…"}`) |
//! | `GET` | `/sessions/:id` | Resolve state (subscribe → `get_state` → `state`) |
//! | `DELETE` | `/sessions/:id` | Forget a unit (keeps the session file on disk) |
//! | `GET` | `/sessions/:id/commands` | Extension slash commands |
//! | `POST` | `/sessions/:id/title` | Generate a session title from the transcript |
//! | `POST` | `/sessions/:id/reset` | Clear a unit's context in place (no re-run) |
//! | `GET` | `/sessions/:id/events` | SSE: replay + live (`?format=pi` for pi's shapes) |
//! | `POST` | `/sessions/:id/commands` | Send a command (202 Accepted) |
//! | `POST` | `/sessions/:id/ui_response` | Answer a `ui_request` |
//!
//! Events stream as `data: <json>\n\n`; the native event envelope is used
//! unless `?format=pi` asks for pi's canonical stream through
//! [`pi_rpc::PiEventAdapter`].

use pi_host::{Host, HostError, MessageKind, RecvError, Subscription};
use pi_rpc::{Event, PiEventAdapter};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
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
    /// When set, every request must present a matching bearer token. `None`
    /// means the gateway is only safe on a loopback bind (the CLI enforces
    /// this); it is never safe to expose an unauthenticated gateway publicly.
    token: Option<String>,
    /// Hosts accepted in the `Host` header. Requests whose `Host` names
    /// anything else are rejected to blunt DNS-rebinding attacks. Loopback
    /// names are always accepted.
    allowed_hosts: Vec<String>,
}

impl Gateway {
    pub fn new(host: Host) -> Self {
        Self {
            host: Mutex::new(host),
            token: None,
            allowed_hosts: Vec::new(),
        }
    }

    /// Require `token` on every request (as `Authorization: Bearer <token>` or
    /// `X-Pi-Token: <token>`). An empty token is ignored, so a blank
    /// `--gateway-token`/env var cannot silently disable the check.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.filter(|token| !token.is_empty());
        self
    }

    /// Trust `hosts` in the `Host` header in addition to loopback names.
    pub fn with_allowed_hosts<I, S>(mut self, hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_hosts = hosts.into_iter().map(Into::into).collect();
        self
    }

    /// Enforce authentication and the `Host` allowlist for `request`; on
    /// failure, write the error response and return `false`.
    fn authorize(&self, stream: &mut TcpStream, request: &Request) -> bool {
        if !self.host_allowed(request) {
            write_json(
                stream,
                403,
                &json!({ "error": "host not allowed; set the correct address" }),
            );
            return false;
        }
        if let Some(expected) = &self.token {
            let presented = request.bearer_token();
            let ok = presented
                .map(|presented| constant_time_eq(presented.as_bytes(), expected.as_bytes()))
                .unwrap_or(false);
            if !ok {
                write_json(stream, 401, &json!({ "error": "unauthorized" }));
                return false;
            }
        }
        true
    }

    /// Whether the request's `Host` header names an accepted host. A missing
    /// `Host` header is rejected (HTTP/1.1 requires one).
    fn host_allowed(&self, request: &Request) -> bool {
        let Some(host) = request.header("host") else {
            return false;
        };
        let host = host_hostname(host);
        is_loopback_host(host)
            || self
                .allowed_hosts
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(host))
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

/// Whether `addr` is safe to serve without a token: every address it resolves
/// to is loopback. Returns a resolved socket address on success, or a
/// human-readable reason the bind must be refused.
pub fn check_bind_security(addr: &str, has_token: bool) -> Result<SocketAddr, String> {
    use std::net::ToSocketAddrs;

    // Accept both `ip:port` literals and `host:port` names.
    let mut resolved = addr
        .to_socket_addrs()
        .map_err(|error| format!("invalid --gateway-addr {addr:?}: {error}"))?;
    let parsed = resolved
        .next()
        .ok_or_else(|| format!("--gateway-addr {addr:?} did not resolve to any address"))?;
    // Fail closed when no token is set: every resolved address must be
    // loopback, so `localhost` resolving to a non-loopback IP is still refused.
    if !has_token {
        let mut all = vec![parsed];
        all.extend(resolved);
        if all.iter().any(|addr| !addr.ip().is_loopback()) {
            return Err(format!(
                "refusing to bind non-loopback address {addr}: set --gateway-token or \
                 PIPELETS_GATEWAY_TOKEN (or bind a loopback address)"
            ));
        }
    }
    Ok(parsed)
}

/// Constant-time byte comparison, so token checks do not leak length/prefix
/// information through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

/// Strip an optional `:port` from a `Host` header value, handling bracketed
/// IPv6 literals (`[::1]:30142`).
fn host_hostname(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    host.split(':').next().unwrap_or(host)
}

/// Whether a bare hostname (no port) is a loopback name.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
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
    /// The `Last-Event-ID` header, parsed as a sequence id, for SSE resume.
    last_event_id: Option<u64>,
    /// The raw request headers, used for bearer-token authorization.
    headers: Vec<(String, String)>,
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

    /// The first value for `name`, case-insensitively.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The bearer token from `Authorization: Bearer …` or `X-Pi-Token`.
    fn bearer_token(&self) -> Option<String> {
        if let Some(value) = self.header("authorization") {
            let value = value.trim();
            if let Some(token) = value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
            {
                return Some(token.trim().to_string());
            }
        }
        self.header("x-pi-token")
            .map(|value| value.trim().to_string())
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
    let mut last_event_id = None;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            } else if key.eq_ignore_ascii_case("last-event-id") {
                last_event_id = value.trim().parse().ok();
            }
            headers.push((key, value));
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
        last_event_id,
        headers,
        body,
    }))
}

fn write_headers(stream: &mut TcpStream, status: u16, content_type: &str, length: usize) {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
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
        HostError::AtCapacity(_) => 429,
        // A full mailbox is a *sender-visible* backpressure signal (VED-379),
        // never a silent drop: the caller must retry after the recipient acks.
        HostError::MailboxFull { .. } => 429,
        // An oversized body is a bad request, not a server fault.
        HostError::BodyTooLarge { .. } => 413,
        // A transfer by a non-owner is a conflict with the recorded owner.
        HostError::NotOwner { .. } => 409,
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
    if !gateway.authorize(&mut stream, &request) {
        return Ok(());
    }
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
        ("GET", ["units", id, "messages"]) => poll_messages(&mut stream, &gateway, id, &request),
        ("POST", ["units", id, "messages"]) => send_message(&mut stream, &gateway, id, &request),
        ("POST", ["units", id, "messages", msgid, "ack"]) => {
            ack_message(&mut stream, &gateway, id, msgid)
        }
        ("POST", ["units", id, "ownership"]) => {
            transfer_ownership(&mut stream, &gateway, id, &request)
        }
        ("POST", ["sessions"]) => create_session(&mut stream, &gateway, &request),
        ("GET", ["sessions", id]) => session_state(&mut stream, &gateway, id),
        ("DELETE", ["sessions", id]) => remove_session(&mut stream, &gateway, id),
        ("GET", ["sessions", id, "commands"]) => session_commands(&mut stream, &gateway, id),
        ("POST", ["sessions", id, "title"]) => session_title(&mut stream, &gateway, id),
        ("POST", ["sessions", id, "reset"]) => reset_session(&mut stream, &gateway, id),
        ("GET", ["sessions", id, "events"]) => {
            stream_events(
                stream,
                &gateway,
                id,
                request.query.contains("format=pi"),
                request.last_event_id,
            )?;
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

// Direct unit-to-unit messaging routes (VED-379).

/// `GET /units/:id/messages?after=<seq>` — messages enqueued to `id`.
fn poll_messages(stream: &mut TcpStream, gateway: &Gateway, id: &str, request: &Request) {
    let after = query_u64(&request.query, "after").unwrap_or(0);
    match gateway.host().poll_messages(id, after) {
        Ok((messages, next_seq)) => write_json(
            stream,
            200,
            &json!({ "messages": messages, "nextSeq": next_seq }),
        ),
        Err(error) => write_json(
            stream,
            status_for(&error),
            &json!({ "error": error.to_string() }),
        ),
    }
}

/// `POST /units/:id/messages` — send a direct message to `id`.
///
/// Body: `{from, kind, body, issue?, corrId?}`. Returns `202 Accepted` with the
/// accepted envelope, `429` when the mailbox is full, `413` for an oversized
/// body, and `404` for an unknown recipient.
fn send_message(stream: &mut TcpStream, gateway: &Gateway, id: &str, request: &Request) {
    let body = request.json().unwrap_or(Value::Null);
    let Some(from) = body.get("from").and_then(Value::as_str) else {
        write_json(stream, 400, &json!({ "error": "from is required" }));
        return;
    };
    let kind = match body.get("kind").and_then(Value::as_str) {
        Some(kind) => match parse_kind(kind) {
            Some(kind) => kind,
            None => {
                write_json(stream, 400, &json!({ "error": "unknown kind" }));
                return;
            }
        },
        None => MessageKind::Request,
    };
    let Some(text) = body.get("body").and_then(Value::as_str) else {
        write_json(stream, 400, &json!({ "error": "body is required" }));
        return;
    };
    let issue = body.get("issue").and_then(Value::as_str);
    let corr_id = body.get("corrId").and_then(Value::as_str);
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match gateway
        .host()
        .send_message(from, id, kind, issue, corr_id, text, now)
    {
        Ok(envelope) => write_json(stream, 202, &json!({ "message": envelope })),
        Err(error) => write_json(
            stream,
            status_for(&error),
            &json!({ "error": error.to_string() }),
        ),
    }
}

/// These routes live on the gateway so the mailbox is a host-level resource,
/// not a model turn: sending a message must never enter the recipient's
/// transcript (VED-379 AC10). `parse_kind` and `query_u64` are the small
/// helpers the routes share.
fn parse_kind(kind: &str) -> Option<MessageKind> {
    match kind {
        "request" => Some(MessageKind::Request),
        "reply" => Some(MessageKind::Reply),
        "handoff" => Some(MessageKind::Handoff),
        "ownership" => Some(MessageKind::Ownership),
        "ack" => Some(MessageKind::Ack),
        _ => None,
    }
}

fn query_u64(query: &str, key: &str) -> Option<u64> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            value.parse().ok()
        } else {
            None
        }
    })
}

/// `POST /units/:id/messages/:msgid/ack` — acknowledge a message. Marks it
/// `acked` (freeing mailbox depth) and delivers an `ack` envelope to the
/// original sender so the sender can observe delivery.
fn ack_message(stream: &mut TcpStream, gateway: &Gateway, id: &str, msgid: &str) {
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match gateway.host().ack_message(id, msgid, now) {
        Ok(ack) => write_json(stream, 200, &json!({ "ack": ack })),
        Err(error) => write_json(
            stream,
            status_for(&error),
            &json!({ "error": error.to_string() }),
        ),
    }
}

/// `POST /units/:id/ownership` — transfer ownership of `id`.
///
/// Body: `{to, by, issue?}`. Records `ownerBefore`/`ownerAfter` and mirrors the
/// new owner into the session header. A transfer by a non-owner fails `409`;
/// an unknown target fails `404`.
fn transfer_ownership(stream: &mut TcpStream, gateway: &Gateway, id: &str, request: &Request) {
    let body = request.json().unwrap_or(Value::Null);
    let Some(to) = body.get("to").and_then(Value::as_str) else {
        write_json(stream, 400, &json!({ "error": "to is required" }));
        return;
    };
    let Some(by) = body.get("by").and_then(Value::as_str) else {
        write_json(stream, 400, &json!({ "error": "by is required" }));
        return;
    };
    let issue = body.get("issue").and_then(Value::as_str);
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    match gateway.host().transfer_ownership(id, to, by, issue, now) {
        Ok(envelope) => write_json(stream, 200, &json!({ "message": envelope })),
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
    let mut state = await_state(&subscription);
    // Carry the live in-flight signal on the state payload too, so a client
    // does not have to cross-reference `/swarm` (and does not mistake a
    // mid-turn unit for an idle one). VED-387.
    if let Ok(in_flight) = gateway.host().in_flight(id) {
        if let Some(object) = state.as_object_mut() {
            object.insert("inFlight".to_string(), json!(in_flight));
        }
    }
    write_json(stream, 200, &state);
}

/// Forget a unit entirely. The session file on disk is untouched, so the same
/// path can be reopened later with `POST /sessions`.
fn remove_session(stream: &mut TcpStream, gateway: &Gateway, id: &str) {
    if gateway.host().remove(id) {
        write_json(stream, 200, &json!({ "removed": true }));
    } else {
        write_json(
            stream,
            404,
            &json!({ "error": format!("unknown session: {id}") }),
        );
    }
}

/// Why an [`await_response`] wait did not produce the awaited event.
enum AwaitError {
    /// The bounded wait elapsed without the event. The unit is still alive.
    TimedOut,
    /// The unit was suspended or dropped this subscriber.
    Disconnected,
}

/// Wait for the `state` event emitted in response to `get_state`.
///
/// A unit that is mid-turn processes commands serially, so it cannot service
/// `get_state` until the current turn ends and the bounded wait expires. That
/// is `busy`, not broken: report it as a normal state response with
/// `busy: true` so a caller (or the conductor) can tell "busy" from "wedged"
/// instead of reading a false timeout error (VED-386).
fn await_state(subscription: &Subscription) -> Value {
    match await_response(subscription, |event| {
        event.get("type").and_then(Value::as_str) == Some("state")
    }) {
        Ok(event) => event,
        Err(AwaitError::TimedOut) => json!({ "type": "state", "busy": true }),
        Err(AwaitError::Disconnected) => {
            json!({ "error": "unit closed before state arrived" })
        }
    }
}

fn session_commands(stream: &mut TcpStream, gateway: &Gateway, id: &str) {
    let Some(subscription) = subscribe_or_error(stream, gateway, id) else {
        return;
    };
    if !send_or_error(stream, gateway, id, json!({ "type": "get_commands" })) {
        return;
    }
    let data = await_response(&subscription, |event| {
        event.get("type").and_then(Value::as_str) == Some("response")
            && event.get("command").and_then(Value::as_str) == Some("get_commands")
    })
    .unwrap_or_else(|error| await_error(error, "get_commands"));
    write_json(stream, 200, &data);
}

/// Clear a unit's context in place without re-running the task, so the next
/// prompt starts fresh. The session file and id are kept, so the unit stays
/// addressable. Used by the swarm conductor for fresh-context-per-card.
fn reset_session(stream: &mut TcpStream, gateway: &Gateway, id: &str) {
    let Some(subscription) = subscribe_or_error(stream, gateway, id) else {
        return;
    };
    if !send_or_error(
        stream,
        gateway,
        id,
        json!({ "type": "reset", "rerun": false }),
    ) {
        return;
    }
    let data = await_response(&subscription, |event| {
        event.get("type").and_then(Value::as_str) == Some("response")
            && event.get("command").and_then(Value::as_str) == Some("reset")
    })
    .unwrap_or_else(|error| await_error(error, "reset"));
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
    let data = await_response(&subscription, |event| {
        event.get("type").and_then(Value::as_str) == Some("response")
            && event.get("command").and_then(Value::as_str) == Some("generate_title")
    })
    .unwrap_or_else(|error| await_error(error, "generate_title"));
    write_json(stream, 200, &data);
}

/// The error body for a wait that did not complete.
fn await_error(error: AwaitError, label: &str) -> Value {
    match error {
        AwaitError::TimedOut => json!({ "error": format!("timed out waiting for {label}") }),
        AwaitError::Disconnected => {
            json!({ "error": format!("unit closed before {label} arrived") })
        }
    }
}

/// Wait for the first event matching `predicate`, returning its data payload.
fn await_response(
    subscription: &Subscription,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value, AwaitError> {
    let deadline = std::time::Instant::now() + STATE_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(AwaitError::TimedOut);
        }
        match subscription.recv_timeout(remaining) {
            Ok(event) if predicate(&event) => {
                // A `state` event is the payload; a `response` carries it in `data`.
                if event.get("type").and_then(Value::as_str) == Some("response") {
                    return Ok(event.get("data").cloned().unwrap_or(Value::Null));
                }
                return Ok(event);
            }
            Ok(_) => continue,
            Err(RecvError::Timeout) => return Err(AwaitError::TimedOut),
            Err(RecvError::Disconnected) => return Err(AwaitError::Disconnected),
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
    last_event_id: Option<u64>,
) -> std::io::Result<()> {
    let Some(subscription) = subscribe_or_error(&mut stream, gateway, id) else {
        return Ok(());
    };
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.flush()?;

    let mut adapter = PiEventAdapter::new();
    let oldest = subscription.oldest_replay_seq();
    // When the client asked to resume from an id older than anything still
    // buffered, the events in between are gone. Tell it to resync rather than
    // silently dropping them, then continue with what remains.
    if let Some(last) = last_event_id {
        if last + 1 < oldest {
            let gap = json!({
                "type": "resync",
                "reason": "replay_buffer_rolled_over",
                "lastEventId": last,
                "oldestReplayId": oldest,
                "nextEventId": subscription.next_seq,
            });
            let frame = format!(
                "event: resync\nid: {}\ndata: {}\n\n",
                oldest.saturating_sub(1),
                serde_json::to_string(&gap).unwrap_or_default()
            );
            if stream.write_all(frame.as_bytes()).is_err() {
                return Ok(());
            }
            // Ask the unit for a fresh snapshot so the client can rebuild state.
            let _ = gateway.host().send(id, json!({ "type": "get_state" }));
        }
    }
    // Snapshot first, then live events; the host registers the subscriber and
    // snapshots the replay atomically, so there is no gap or duplicate. Skip
    // anything the client already saw (seq <= Last-Event-ID).
    let resume_from = last_event_id.unwrap_or(0);
    for (event, seq) in subscription
        .replay
        .clone()
        .into_iter()
        .zip(subscription.replay_ids.iter().copied())
    {
        if seq <= resume_from {
            continue;
        }
        let payloads = if pi_format {
            translate(&mut adapter, &event)
        } else {
            vec![event]
        };
        for payload in payloads {
            if stream
                .write_all(sse_frame(seq, &payload).as_bytes())
                .is_err()
            {
                return Ok(());
            }
        }
    }
    if stream.flush().is_err() {
        return Ok(());
    }
    loop {
        match subscription.recv_sequenced_timeout(SSE_HEARTBEAT) {
            Ok((seq, event)) => {
                let payloads = if pi_format {
                    translate(&mut adapter, &event)
                } else {
                    vec![event]
                };
                for payload in payloads {
                    if stream
                        .write_all(sse_frame(seq, &payload).as_bytes())
                        .is_err()
                    {
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

/// One SSE frame with its `id:` line, so a client's `Last-Event-ID` retry can
/// resume from the right place.
fn sse_frame(seq: u64, payload: &Value) -> String {
    format!(
        "id: {seq}\ndata: {}\n\n",
        serde_json::to_string(payload).unwrap_or_default()
    )
}

/// Fold a native event into pi's shapes; pass unknown values through unchanged.
fn translate(adapter: &mut PiEventAdapter, value: &Value) -> Vec<Value> {
    match serde_json::from_value::<Event>(value.clone()) {
        Ok(event) => adapter.translate(&event),
        Err(_) => vec![value.clone()],
    }
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
