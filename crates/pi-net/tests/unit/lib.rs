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

const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_9\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_9\",\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":60}}}}\n\n",
);

#[test]
fn streams_openai_responses_sse_with_usage() {
    let base = serve_once(RESPONSES_SSE);
    let params = pi_providers::build_openai_responses_params(
        "gpt-5".into(),
        "system",
        &[],
        &[pi_providers::TranscriptMessage::UserText("hi".into())],
        &pi_providers::OpenAiResponsesBuildOptions {
            cache_retention: pi_providers::CacheRetention::Short,
            session_id: Some("sess".into()),
            supports_long_cache_retention: true,
            supports_explicit_prompt_cache_mode: false,
            supports_strict_mode: true,
            supports_developer_role: true,
            reasoning: true,
            supports_image_input: true,
            strict: false,
        },
    );
    let result = stream_openai_responses(&base, "test-key", &params).expect("streams");
    assert_eq!(result.message_id.as_deref(), Some("resp_9"));
    assert_eq!(result.text, "Hi");
    assert_eq!(result.usage.input, 40);
    assert_eq!(result.usage.cache_read, 60);
    assert_eq!(result.usage.output, 7);
    assert_eq!(result.usage.cache_hit_rate(), Some(0.6));
}
const GOOGLE_SSE: &str = concat!(
    "event: message\n",
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}],\"role\":\"model\"}}]}\n\n",
    "event: message\n",
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"!\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":5,\"cachedContentTokenCount\":80}}\n\n",
);

#[test]
fn streams_google_sse_with_usage() {
    let base = serve_once(GOOGLE_SSE);
    let params = pi_providers::build_google_params(
        "gemini-2.5-flash".into(),
        "system",
        &[],
        &[pi_providers::TranscriptMessage::UserText("hi".into())],
        &pi_providers::GoogleBuildOptions {
            max_tokens: Some(1024),
            reasoning: true,
            thinking_disabled: true,
        },
    );
    let result = stream_google(&base, "test-key", &params).expect("streams");
    assert_eq!(result.text, "Hi!");
    assert_eq!(result.usage.input, 20);
    assert_eq!(result.usage.cache_read, 80);
    assert_eq!(result.usage.output, 5);
    assert_eq!(result.usage.cache_hit_rate(), Some(0.8));
}
