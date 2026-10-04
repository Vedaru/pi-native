use super::*;
use std::io::Write;
use std::net::TcpListener;

fn serve_once(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((mut socket, _)) = listener.accept() {
            let mut request = [0u8; 4096];
            let _ = std::io::Read::read(&mut socket, &mut request);
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

const COMPLETIONS_SSE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":60}}}\n\n",
    "data: [DONE]\n\n",
);

#[test]
fn streams_openai_completions_sse_with_usage() {
    let base = serve_once(COMPLETIONS_SSE);
    let params = pi_providers::build_openai_completions_params(
        "deepseek-flash".into(),
        "system",
        &[],
        &[pi_providers::TranscriptMessage::UserText("hi".into())],
        &pi_providers::OpenAiCompletionsBuildOptions {
            cache_retention: pi_providers::CacheRetention::Short,
            session_id: None,
            base_url_is_openai_api: false,
            supports_long_cache_retention: true,
            supports_usage_in_streaming: true,
            supports_store: false,
            max_tokens_field: pi_providers::MaxTokensField::MaxTokens,
            supports_developer_role: false,
            supports_strict_mode: true,
            requires_reasoning_content_on_assistant_messages: true,
            reasoning: true,
            thinking_format: pi_providers::ThinkingFormat::Deepseek,
            max_tokens: Some(384_000),
            off_supported: true,
            reasoning_effort: None,
        },
    );
    let result = stream_openai_completions(&base, "test-key", &params).expect("streams");
    assert_eq!(result.message_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(result.text, "Hello");
    // prompt_tokens includes cached tokens: input = 100 - 60.
    assert_eq!(result.usage.input, 40);
    assert_eq!(result.usage.cache_read, 60);
    assert_eq!(result.usage.output, 5);
    assert_eq!(result.usage.cache_hit_rate(), Some(0.6));
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
