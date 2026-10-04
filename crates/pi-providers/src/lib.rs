//! Provider request builders.
//!
//! Only the OpenAI wire formats are built (Chat Completions and Responses);
//! there is no Anthropic or Google model to validate against.

pub mod convert;
pub mod openai_completions;
pub mod openai_completions_stream;
pub mod openai_responses;
pub mod openai_responses_stream;
pub mod types;

pub use pi_cache::CacheRetention;

pub use convert::{AssistantBlock, ContentPart, TranscriptMessage};
pub use openai_completions::{
    build_openai_completions_params, make_strict_schema, MaxTokensField,
    OpenAiCompletionsBuildOptions, ThinkingFormat,
};
pub use openai_completions_stream::{
    collect_completions, OpenAiCompletionsStream, OpenAiCompletionsStreamEvent,
};
pub use openai_responses::{
    build_openai_responses_params, convert_responses_input, convert_responses_tools,
    OpenAiResponsesBuildOptions, OpenAiResponsesParams,
};
pub use openai_responses_stream::{
    collect_response, OpenAiResponsesStream, OpenAiResponsesStreamEvent,
};
pub use types::{ContentBlock, MessageContent, ToolSpec, Usage};
