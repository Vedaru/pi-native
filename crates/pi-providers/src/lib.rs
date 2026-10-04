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

pub use anthropic::{
    apply_conversation_cache_breakpoint, build_anthropic_params, build_system, convert_tools,
    resolve_thinking, AnthropicBuildOptions, AnthropicMessage, AnthropicParams,
    AnthropicSystemBlock, AnthropicTool, ContentBlock, ThinkingOptions, ToolSpec,
};
