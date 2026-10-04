//! Provider request builders.
//!
//! The cache marker placement here mirrors pi exactly:
//! - Anthropic `cache_control` on the system block, the **last** tool, and the
//!   last user/system message's final eligible content block.
//!
//! Sources mirrored: `packages/ai/src/api/anthropic-messages.ts`
//! (`buildParams`, `convertTools`, the conversation breakpoint at the end of
//! `convertMessages`).

pub mod anthropic;
pub mod convert;
pub mod openai_completions;
pub mod openai_responses;

pub use anthropic::{
    apply_conversation_cache_breakpoint, build_anthropic_params, build_system, convert_tools,
    resolve_thinking, AnthropicBuildOptions, AnthropicMessage, AnthropicParams,
    AnthropicSystemBlock, AnthropicTool, ContentBlock, MessageContent, ThinkingOptions, ToolSpec,
};
pub use convert::{convert_messages, AssistantBlock, ContentPart, TranscriptMessage};
pub use openai_completions::{
    build_openai_completions_params, MaxTokensField, OpenAiCompletionsBuildOptions, ThinkingFormat,
};
pub use openai_responses::{
    build_openai_responses_params, convert_responses_input, convert_responses_tools,
    OpenAiResponsesBuildOptions, OpenAiResponsesParams,
};
