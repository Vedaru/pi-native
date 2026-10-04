//! Anthropic provider over `pi-net`.
//!
//! Bridges the agent loop to the native streaming transport: convert the
//! transcript to an Anthropic request, stream it, and map the result back to an
//! `AssistantTurn`.

use crate::{AgentError, AssistantTurn, CompletionRequest, ModelProvider, ToolCall};
use pi_net::{stream_anthropic, StreamResult};
use pi_providers::{
    build_anthropic_params, convert_messages, AnthropicBuildOptions, ContentBlock, ThinkingOptions,
};

/// A live Anthropic Messages provider.
pub struct AnthropicProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: i64,
    pub reasoning: bool,
    pub thinking_enabled: Option<bool>,
}

impl AnthropicProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_tokens: 4096,
            reasoning: true,
            thinking_enabled: Some(false),
        }
    }

    fn options(&self) -> AnthropicBuildOptions {
        AnthropicBuildOptions {
            cache_retention: pi_providers::CacheRetention::Short,
            supports_long_cache_retention: true,
            supports_cache_control_on_tools: true,
            supports_eager_tool_input_streaming: true,
            strict_tools: false,
            max_tokens: Some(self.max_tokens),
            default_max_tokens: self.max_tokens,
            temperature: None,
            reasoning: self.reasoning,
            force_adaptive_thinking: false,
            thinking: ThinkingOptions {
                enabled: self.thinking_enabled,
                ..Default::default()
            },
        }
    }
}

impl ModelProvider for AnthropicProvider {
    fn complete(&self, request: &CompletionRequest) -> Result<AssistantTurn, AgentError> {
        let messages = convert_messages(&request.messages);
        let params = build_anthropic_params(
            self.model.clone(),
            &request.system,
            &request.tools,
            messages,
            &self.options(),
        );
        let result = stream_anthropic(&self.base_url, &self.api_key, &params)
            .map_err(|error| AgentError::Provider(error.to_string()))?;
        Ok(turn_from_stream(result))
    }
}

/// Map a streamed Anthropic result to an `AssistantTurn`.
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

#[cfg(test)]
#[path = "../tests/unit/anthropic.rs"]
mod tests;
