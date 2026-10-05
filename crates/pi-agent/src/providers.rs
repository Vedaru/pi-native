//! Providers as one generic mechanism.
//!
//! A provider is a protocol plus a way to build that protocol's params from a
//! `CompletionRequest`. The loop, transport, and result mapping are shared, so
//! adding a provider is data (a builder), not a new adapter. All wire logic
//! lives in `pi-providers`/`pi-net`; nothing here re-implements it.

use std::sync::{Arc, Mutex};

use crate::{AgentError, AssistantTurn, CompletionRequest, ModelProvider, ToolCall};
use pi_net::{
    stream_sse, stream_sse_with, OpenAiCompletionsProtocol, OpenAiResponsesProtocol, SseProtocol,
    StreamDelta, StreamResult,
};
use pi_providers::{
    build_openai_completions_params, build_openai_responses_params, CacheRetention, ContentBlock,
    MaxTokensField, OpenAiCompletionsBuildOptions, OpenAiResponsesBuildOptions, ThinkingFormat,
};

/// Maps a streamed provider result to an `AssistantTurn`.
pub fn turn_from_stream(result: StreamResult) -> AssistantTurn {
    let tool_calls: Vec<ToolCall> = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: input.clone(),
            }),
            _ => None,
        })
        .collect();
    let stop_reason = if tool_calls.is_empty() {
        Some("end_turn".to_string())
    } else {
        Some("tool_use".to_string())
    };
    AssistantTurn {
        text: result.text,
        tool_calls,
        usage: Some(result.usage),
        stop_reason,
    }
}

/// Builds a protocol's params from a completion request.
type ParamsBuilder<P> =
    Box<dyn for<'a> Fn(&CompletionRequest<'a>) -> <P as SseProtocol>::Params + Send + Sync>;

/// A provider for any SSE protocol. `build` produces the protocol's params from
/// a completion request; everything else is shared.
pub struct HttpProvider<P: SseProtocol> {
    base_url: String,
    api_key: String,
    build: ParamsBuilder<P>,
    /// Reasoning effort applied to every request. `set_thinking_level` writes
    /// it; the params builder reads it, so a session's level reaches the wire.
    thinking_effort: Arc<Mutex<Option<String>>>,
}

impl<P: SseProtocol> HttpProvider<P> {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        build: impl for<'a> Fn(&CompletionRequest<'a>) -> P::Params + Send + Sync + 'static,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            build: Box::new(build),
            thinking_effort: Arc::new(Mutex::new(None)),
        }
    }
}

/// Map pi's thinking levels onto a provider reasoning effort.
///
/// `off` disables reasoning (no effort, so `resolve_thinking` emits
/// `disabled`); `minimal`/`low` collapse to `low` and `xhigh`/`max` to `high`,
/// the values OpenAI-compatible APIs accept. Any other level passes through.
fn level_effort(level: &str) -> Option<String> {
    match level {
        "off" => None,
        "minimal" | "low" => Some("low".to_string()),
        "medium" => Some("medium".to_string()),
        "high" | "xhigh" | "max" => Some("high".to_string()),
        other => Some(other.to_string()),
    }
}

impl<P: SseProtocol + 'static> ModelProvider for HttpProvider<P> {
    fn complete(&self, request: &CompletionRequest) -> Result<AssistantTurn, AgentError> {
        let params = (self.build)(request);
        let result = stream_sse::<P>(&self.base_url, &self.api_key, &params)
            .map_err(|error| AgentError::Provider(error.to_string()))?;
        Ok(turn_from_stream(result))
    }

    fn stream(
        &self,
        request: &CompletionRequest,
        on_delta: &mut dyn FnMut(StreamDelta),
    ) -> Result<AssistantTurn, AgentError> {
        let params = (self.build)(request);
        let result = stream_sse_with::<P>(&self.base_url, &self.api_key, &params, on_delta)
            .map_err(|error| AgentError::Provider(error.to_string()))?;
        Ok(turn_from_stream(result))
    }

    fn set_thinking_level(&self, level: &str) {
        if let Ok(mut effort) = self.thinking_effort.lock() {
            *effort = level_effort(level);
        }
    }
}

/// OpenAI Responses provider.
pub fn openai_responses_provider(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    model: impl Into<String>,
) -> HttpProvider<OpenAiResponsesProtocol> {
    let model = model.into();
    HttpProvider::new(base_url, api_key, move |request| {
        build_openai_responses_params(
            model.clone(),
            request.system,
            request.tools,
            request.messages,
            &OpenAiResponsesBuildOptions {
                cache_retention: CacheRetention::Short,
                session_id: None,
                supports_long_cache_retention: true,
                supports_explicit_prompt_cache_mode: false,
                supports_strict_mode: true,
                supports_developer_role: true,
                reasoning: true,
                supports_image_input: true,
                strict: false,
            },
        )
    })
}

/// OpenAI-compatible Chat Completions provider.
///
/// Everything vendor-specific (base URL, model id, output cap, thinking format)
/// is supplied by the caller; nothing is assumed here.
pub fn openai_completions_provider(
    base_url: impl Into<String>,
    api_key: impl Into<String>,
    model: impl Into<String>,
    max_tokens: Option<i64>,
    thinking_format: ThinkingFormat,
    reasoning_effort: Option<String>,
) -> HttpProvider<OpenAiCompletionsProtocol> {
    let model = model.into();
    let requires_reasoning = matches!(thinking_format, ThinkingFormat::Deepseek);
    let effort = Arc::new(Mutex::new(reasoning_effort));
    let effort_for_build = Arc::clone(&effort);
    let mut provider = HttpProvider::new(base_url, api_key, move |request| {
        // Read the current level each request, so a mid-session change applies
        // from the next turn without rebuilding the provider.
        let reasoning_effort = effort_for_build
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        build_openai_completions_params(
            model.clone(),
            request.system,
            request.tools,
            request.messages,
            &OpenAiCompletionsBuildOptions {
                cache_retention: CacheRetention::Short,
                session_id: None,
                base_url_is_openai_api: false,
                supports_long_cache_retention: true,
                supports_usage_in_streaming: true,
                supports_store: false,
                max_tokens_field: MaxTokensField::MaxTokens,
                supports_developer_role: false,
                supports_strict_mode: true,
                requires_reasoning_content_on_assistant_messages: requires_reasoning,
                reasoning: true,
                thinking_format,
                max_tokens,
                off_supported: true,
                reasoning_effort,
            },
        )
    });
    provider.thinking_effort = effort;
    provider
}

#[cfg(test)]
#[path = "../tests/unit/providers.rs"]
mod tests;
