//! HTTP + Server-Sent Events transport for native providers.
//!
//! Provider streaming: build the request with `pi-providers`, POST it, and feed
//! the response body through `pi-sse` into a provider stream parser. No SDKs and
//! no Node.

use std::io::Read;

use pi_providers::{collect_content, AnthropicParams, AnthropicStream, ContentBlock, Usage};
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

/// The result of consuming an Anthropic message stream.
#[derive(Debug, Clone)]
pub struct AnthropicResult {
    pub message_id: Option<String>,
    pub content: Vec<ContentBlock>,
    pub text: String,
    pub usage: Usage,
}

/// Build, POST, and consume an Anthropic Messages streaming request.
///
/// `base_url` is what pi calls `model.baseUrl` (e.g. `https://api.anthropic.com`);
/// the `/v1/messages` path is appended.
pub fn stream_anthropic(
    base_url: &str,
    api_key: &str,
    params: &AnthropicParams,
) -> Result<AnthropicResult, NetError> {
    let body =
        serde_json::to_string(params).map_err(|error| NetError::Encode(error.to_string()))?;
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let headers = [
        ("x-api-key", api_key.to_string()),
        ("anthropic-version", "2023-06-01".to_string()),
        ("accept", "text/event-stream".to_string()),
    ];
    let response = post_json(&url, &headers, &body)?;
    let mut stream = AnthropicStream::new();
    let mut events = Vec::new();
    read_sse(response.into_reader(), |event_type, data| {
        events.push(stream.handle(event_type, data));
    })?;
    let (content, text) = collect_content(&events);
    Ok(AnthropicResult {
        message_id: stream.message_id().map(str::to_string),
        content,
        text,
        usage: stream.usage().clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_providers::{AnthropicBuildOptions, ThinkingOptions};
    use std::io::Write;
    use std::net::TcpListener;

    fn test_params() -> AnthropicParams {
        pi_providers::build_anthropic_params(
            "claude-sonnet-4-5".into(),
            "system",
            &[],
            vec![pi_providers::AnthropicMessage {
                role: "user",
                content: pi_providers::MessageContent::Text("hi".into()),
            }],
            &AnthropicBuildOptions {
                cache_retention: pi_providers::CacheRetention::Short,
                supports_long_cache_retention: true,
                supports_cache_control_on_tools: true,
                supports_eager_tool_input_streaming: true,
                strict_tools: false,
                max_tokens: Some(1024),
                default_max_tokens: 1024,
                temperature: None,
                reasoning: true,
                force_adaptive_thinking: false,
                thinking: ThinkingOptions {
                    enabled: Some(false),
                    ..Default::default()
                },
            },
        )
    }

    const SSE_BODY: &str = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_42\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1,\"cache_read_input_tokens\":90}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    fn serve_once(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut socket, _)) = listener.accept() {
                let mut request = [0u8; 4096];
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
    fn streams_anthropic_sse_with_usage() {
        let base = serve_once(SSE_BODY);
        let result = stream_anthropic(&base, "test-key", &test_params()).expect("streams");
        assert_eq!(result.message_id.as_deref(), Some("msg_42"));
        assert_eq!(result.text, "Hello");
        assert_eq!(result.usage.input, 10);
        assert_eq!(result.usage.output, 5);
        assert_eq!(result.usage.cache_read, 90);
        assert_eq!(result.usage.cache_hit_rate(), Some(0.9));
    }
}
