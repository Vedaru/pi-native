//! HTTP + Server-Sent Events transport for native providers.
//!
//! Provider streaming: build the request with `pi-providers`, POST it, and feed
//! the response body through `pi-sse` into a provider stream parser. No SDKs and
//! no Node. Only the OpenAI wire formats are supported.

use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pi_providers::{
    collect_completions, collect_response, ContentBlock, OpenAiCompletionsStream,
    OpenAiCompletionsStreamEvent, OpenAiResponsesParams, OpenAiResponsesStream,
    OpenAiResponsesStreamEvent, Usage,
};
use pi_sse::SseParser;

#[derive(Debug)]
pub enum NetError {
    Transport(String),
    Status(u16),
    Encode(String),
}

impl std::fmt::Display for NetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetError::Transport(message) => write!(f, "transport: {message}"),
            NetError::Status(code) => write!(f, "http status {code}"),
            NetError::Encode(message) => write!(f, "encode: {message}"),
        }
    }
}

impl std::error::Error for NetError {}

/// An HTTP response whose body is a streaming reader.
pub struct HttpResponse {
    pub status: u16,
    body: Box<dyn Read + Send>,
}

impl HttpResponse {
    pub fn into_reader(self) -> Box<dyn Read + Send> {
        self.body
    }
}

/// POST a JSON body and return the streaming response.
///
/// A swarm of units hits the provider concurrently, so a 429 or 5xx is normal
/// rather than exceptional. The request is retried with jittered backoff: at
/// most [`MAX_ATTEMPTS`] tries for a retryable status or a transport error, and
/// never for a 4xx (the API answered, and the same question gets the same
/// answer). Provider completions are stateless reads, so repeating one is safe.
pub fn post_json(
    url: &str,
    headers: &[(&str, String)],
    body_json: &str,
) -> Result<HttpResponse, NetError> {
    let mut attempt = 0u32;
    loop {
        match post_json_once(url, headers, body_json) {
            Ok(response) => return Ok(response),
            Err(error) => {
                if attempt + 1 >= MAX_ATTEMPTS || !is_retryable(&error) {
                    return Err(error);
                }
                std::thread::sleep(backoff(attempt));
                attempt += 1;
            }
        }
    }
}

/// Total tries: one attempt plus two retries.
pub const MAX_ATTEMPTS: u32 = 3;
/// First backoff, doubled per attempt and then jittered.
pub const BACKOFF_BASE: Duration = Duration::from_millis(250);
/// Ceiling on a single backoff sleep.
pub const BACKOFF_MAX: Duration = Duration::from_secs(4);

/// Whether a failure is worth another attempt.
pub fn is_retryable(error: &NetError) -> bool {
    match error {
        NetError::Status(code) => *code == 429 || (500..600).contains(code),
        // A connection reset or DNS failure is worth a bounded retry too.
        NetError::Transport(_) => true,
        NetError::Encode(_) => false,
    }
}

/// The sleep before attempt `attempt` (0-based), doubled and jittered so that a
/// fleet hitting one rate limit does not synchronise into a second stampede.
/// Jitter comes from the clock, so it costs no dependency.
pub fn backoff(attempt: u32) -> Duration {
    let doubled = BACKOFF_BASE
        .checked_mul(1u32 << attempt.min(8))
        .unwrap_or(BACKOFF_MAX)
        .min(BACKOFF_MAX);
    let jitter_ceiling = (doubled.as_millis() as u64 / 2).max(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = Duration::from_millis(nanos % jitter_ceiling);
    (doubled + jitter).min(BACKOFF_MAX)
}

fn post_json_once(
    url: &str,
    headers: &[(&str, String)],
    body_json: &str,
) -> Result<HttpResponse, NetError> {
    let mut request = ureq::post(url).header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    match request.send(body_json) {
        Ok(response) => {
            let status = response.status().as_u16();
            Ok(HttpResponse {
                status,
                body: Box::new(response.into_body().into_reader()),
            })
        }
        Err(error) => Err(map_ureq_error(error)),
    }
}

fn map_ureq_error(error: ureq::Error) -> NetError {
    let message = error.to_string();
    if let Some(code) = message
        .split_whitespace()
        .find_map(|token| token.parse::<u16>().ok())
    {
        if (100..600).contains(&code) {
            return NetError::Status(code);
        }
    }
    NetError::Transport(message)
}

/// Read SSE frames from `reader`, invoking `on_event(event_type, data)` for each.
/// Stops at the OpenAI-style `[DONE]` sentinel.
pub fn read_sse(
    mut reader: Box<dyn Read + Send>,
    mut on_event: impl FnMut(&str, &str),
) -> Result<(), NetError> {
    let mut parser = SseParser::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| NetError::Transport(error.to_string()))?;
        if read == 0 {
            break;
        }
        for event in parser.push(&buffer[..read]) {
            if event.data == "[DONE]" {
                return Ok(());
            }
            on_event(&event.event, &event.data);
        }
    }
    for event in parser.finish() {
        if event.data == "[DONE]" {
            return Ok(());
        }
        on_event(&event.event, &event.data);
    }
    Ok(())
}

/// The result of consuming a provider stream.
#[derive(Debug, Clone)]
pub struct StreamResult {
    pub message_id: Option<String>,
    pub content: Vec<ContentBlock>,
    pub text: String,
    pub usage: Usage,
}

/// An incremental piece of a provider stream, forwarded as it arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    Text(String),
    Thinking(String),
    ToolCall {
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
}

/// A provider SSE protocol: where to POST, how to authenticate, and how to turn
/// events into a result. Implementations are stateful parsers.
pub trait SseProtocol: Default {
    /// The request params type this protocol serializes.
    type Params: serde::Serialize;
    /// Full request URL for a model base URL and params (some providers put the
    /// model in the path).
    fn endpoint(base_url: &str, params: &Self::Params) -> String;
    /// Auth and version headers; the transport adds `content-type` and `accept`.
    fn headers(api_key: &str) -> Vec<(&'static str, String)>;
    /// Consume one SSE frame, returning any incremental delta it carried.
    fn ingest(&mut self, event_type: &str, data: &str) -> Vec<StreamDelta>;
    /// Finish and produce the result, or an error if the stream failed.
    fn into_result(self) -> Result<StreamResult, NetError>;
}

/// Generic provider streaming.
///
/// Serialize the params, POST to the protocol endpoint, feed SSE frames into
/// the protocol, and return its result. There is exactly one transport loop for
/// every provider; providers contribute only their endpoint, headers, and event
/// handling.
pub fn stream_sse<P>(
    base_url: &str,
    api_key: &str,
    params: &P::Params,
) -> Result<StreamResult, NetError>
where
    P: SseProtocol,
{
    stream_sse_with::<P>(base_url, api_key, params, &mut |_| {})
}

/// Like [`stream_sse`], but forwards each [`StreamDelta`] to `on_delta` as it
/// arrives (before the final result is assembled).
pub fn stream_sse_with<P>(
    base_url: &str,
    api_key: &str,
    params: &P::Params,
    on_delta: &mut dyn FnMut(StreamDelta),
) -> Result<StreamResult, NetError>
where
    P: SseProtocol,
{
    let body =
        serde_json::to_string(params).map_err(|error| NetError::Encode(error.to_string()))?;
    let url = P::endpoint(base_url, params);
    let mut headers = P::headers(api_key);
    headers.push(("accept", "text/event-stream".to_string()));
    let response = post_json(&url, &headers, &body)?;
    let mut protocol = P::default();
    read_sse(response.into_reader(), |event_type, data| {
        for delta in protocol.ingest(event_type, data) {
            on_delta(delta);
        }
    })?;
    protocol.into_result()
}

fn completion_delta(event: &OpenAiCompletionsStreamEvent) -> Vec<StreamDelta> {
    match event {
        OpenAiCompletionsStreamEvent::TextDelta { delta } => vec![StreamDelta::Text(delta.clone())],
        OpenAiCompletionsStreamEvent::ReasoningDelta { delta } => {
            vec![StreamDelta::Thinking(delta.clone())]
        }
        OpenAiCompletionsStreamEvent::ToolCallDeltas { deltas } => deltas
            .iter()
            .map(|delta| StreamDelta::ToolCall {
                id: delta.id.clone(),
                name: delta.name.clone(),
                arguments: delta.arguments.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn response_delta(event: &OpenAiResponsesStreamEvent) -> Vec<StreamDelta> {
    match event {
        OpenAiResponsesStreamEvent::TextDelta { delta } => vec![StreamDelta::Text(delta.clone())],
        OpenAiResponsesStreamEvent::ToolCallStart { call_id, name, .. } => {
            vec![StreamDelta::ToolCall {
                id: Some(call_id.clone()),
                name: Some(name.clone()),
                arguments: None,
            }]
        }
        OpenAiResponsesStreamEvent::ToolCallArgsDelta { call_id, delta } => {
            vec![StreamDelta::ToolCall {
                id: Some(call_id.clone()),
                name: None,
                arguments: Some(delta.clone()),
            }]
        }
        _ => Vec::new(),
    }
}

/// OpenAI-compatible Chat Completions protocol (DeepSeek, Xiaomi, OpenAI).
#[derive(Default)]
pub struct OpenAiCompletionsProtocol {
    stream: OpenAiCompletionsStream,
    events: Vec<OpenAiCompletionsStreamEvent>,
}

impl SseProtocol for OpenAiCompletionsProtocol {
    type Params = serde_json::Value;
    fn endpoint(base_url: &str, _params: &serde_json::Value) -> String {
        format!("{}/chat/completions", base_url.trim_end_matches('/'))
    }
    fn headers(api_key: &str) -> Vec<(&'static str, String)> {
        vec![("authorization", format!("Bearer {api_key}"))]
    }
    fn ingest(&mut self, _event_type: &str, data: &str) -> Vec<StreamDelta> {
        let event = self.stream.handle(data);
        let delta = completion_delta(&event);
        self.events.push(event);
        delta
    }
    fn into_result(self) -> Result<StreamResult, NetError> {
        let (text, content) = collect_completions(&self.events);
        Ok(StreamResult {
            message_id: self.stream.message_id().map(str::to_string),
            content,
            text,
            usage: self.stream.usage().clone(),
        })
    }
}

/// OpenAI Responses protocol.
#[derive(Default)]
pub struct OpenAiResponsesProtocol {
    stream: OpenAiResponsesStream,
    events: Vec<OpenAiResponsesStreamEvent>,
}

impl SseProtocol for OpenAiResponsesProtocol {
    type Params = OpenAiResponsesParams;
    fn endpoint(base_url: &str, _params: &OpenAiResponsesParams) -> String {
        format!("{}/responses", base_url.trim_end_matches('/'))
    }
    fn headers(api_key: &str) -> Vec<(&'static str, String)> {
        vec![("authorization", format!("Bearer {api_key}"))]
    }
    fn ingest(&mut self, event_type: &str, data: &str) -> Vec<StreamDelta> {
        let event = self.stream.handle(event_type, data);
        let delta = response_delta(&event);
        self.events.push(event);
        delta
    }
    fn into_result(self) -> Result<StreamResult, NetError> {
        if let Some(failure) = self.events.iter().find_map(|event| match event {
            OpenAiResponsesStreamEvent::Failed { message } => Some(message.clone()),
            _ => None,
        }) {
            return Err(NetError::Transport(failure));
        }
        let (text, content) = collect_response(&self.events);
        Ok(StreamResult {
            message_id: self.stream.message_id().map(str::to_string),
            content,
            text,
            usage: self.stream.usage().clone(),
        })
    }
}

/// Stream an OpenAI-compatible Chat Completions request.
pub fn stream_openai_completions(
    base_url: &str,
    api_key: &str,
    params: &serde_json::Value,
) -> Result<StreamResult, NetError> {
    stream_sse::<OpenAiCompletionsProtocol>(base_url, api_key, params)
}

/// Stream an OpenAI Responses request. `base_url` includes the version path.
pub fn stream_openai_responses(
    base_url: &str,
    api_key: &str,
    params: &OpenAiResponsesParams,
) -> Result<StreamResult, NetError> {
    stream_sse::<OpenAiResponsesProtocol>(base_url, api_key, params)
}

#[cfg(test)]
#[path = "../tests/unit/lib.rs"]
mod tests;
