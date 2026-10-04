//! HTTP + Server-Sent Events transport for native providers.
//!
//! Provider streaming: build the request with `pi-providers`, POST it, and feed
//! the response body through `pi-sse` into a provider stream parser. No SDKs and
//! no Node. Only the OpenAI wire formats are supported.

use std::io::Read;

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
pub fn post_json(
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
    /// Consume one SSE frame.
    fn ingest(&mut self, event_type: &str, data: &str);
    /// Finish and produce the result.
    fn into_result(self) -> StreamResult;
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
    let body =
        serde_json::to_string(params).map_err(|error| NetError::Encode(error.to_string()))?;
    let url = P::endpoint(base_url, params);
    let mut headers = P::headers(api_key);
    headers.push(("accept", "text/event-stream".to_string()));
    let response = post_json(&url, &headers, &body)?;
    let mut protocol = P::default();
    read_sse(response.into_reader(), |event_type, data| {
        protocol.ingest(event_type, data);
    })?;
    Ok(protocol.into_result())
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
    fn ingest(&mut self, _event_type: &str, data: &str) {
        self.events.push(self.stream.handle(data));
    }
    fn into_result(self) -> StreamResult {
        let (text, content) = collect_completions(&self.events);
        StreamResult {
            message_id: self.stream.message_id().map(str::to_string),
            content,
            text,
            usage: self.stream.usage().clone(),
        }
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
    fn ingest(&mut self, event_type: &str, data: &str) {
        self.events.push(self.stream.handle(event_type, data));
    }
    fn into_result(self) -> StreamResult {
        let (text, content) = collect_response(&self.events);
        StreamResult {
            message_id: self.stream.message_id().map(str::to_string),
            content,
            text,
            usage: self.stream.usage().clone(),
        }
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
