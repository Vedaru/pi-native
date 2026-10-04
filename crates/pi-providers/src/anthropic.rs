//! Anthropic Messages request construction with pi's cache-breakpoint rules.
//!
//! Sources mirrored: `packages/ai/src/api/anthropic-messages.ts`
//! (`buildParams`, `convertMessages`, `convertContentBlocks`, `convertTools`,
//! and the `thinking` block).

use pi_cache::{get_cache_control, CacheControlEphemeral, CacheRetention};
use serde::Serialize;
use serde_json::{json, Value};

/// A system prompt block. Pi always uses a single `text` block.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnthropicSystemBlock {
    pub r#type: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
}

/// A tool definition as sent to Anthropic.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eager_input_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
}

/// A message content block. The `type` tag matches the Anthropic wire format.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Image {
        source: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: MessageContent,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
}

/// Message content: pi emits a bare string for text-only content and a block
/// array otherwise (`convertContentBlocks`).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnthropicMessage {
    pub role: &'static str,
    pub content: MessageContent,
}

/// A tool spec as supplied by the tool registry.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// Full JSON schema (object with `type`, `properties`, `required`).
    pub input_schema: Value,
}

/// Assembled Anthropic Messages params.
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicParams {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: i64,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<AnthropicSystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
}

/// Extended-thinking selection, mirroring pi's option surface.
#[derive(Debug, Clone, Default)]
pub struct ThinkingOptions {
    /// `Some(false)` emits `{"type":"disabled"}` on reasoning models.
    pub enabled: Option<bool>,
    pub display: Option<String>,
    pub effort: Option<String>,
    pub budget_tokens: Option<i64>,
}

/// Options controlling request assembly.
#[derive(Debug, Clone)]
pub struct AnthropicBuildOptions {
    pub cache_retention: CacheRetention,
    pub supports_long_cache_retention: bool,
    pub supports_cache_control_on_tools: bool,
    pub supports_eager_tool_input_streaming: bool,
    pub strict_tools: bool,
    pub max_tokens: Option<i64>,
    pub default_max_tokens: i64,
    pub temperature: Option<f64>,
    pub reasoning: bool,
    pub force_adaptive_thinking: bool,
    pub thinking: ThinkingOptions,
}

/// Build the non-OAuth system prompt block. An empty prompt is omitted.
pub fn build_system(
    text: &str,
    cache_control: Option<CacheControlEphemeral>,
) -> Option<Vec<AnthropicSystemBlock>> {
    if text.is_empty() {
        return None;
    }
    Some(vec![AnthropicSystemBlock {
        r#type: "text",
        text: text.to_string(),
        cache_control,
    }])
}

/// Convert tool specs, placing the cache marker on the **last** tool only.
pub fn convert_tools(
    tools: &[ToolSpec],
    eager_input_streaming: bool,
    strict: bool,
    cache_control: Option<CacheControlEphemeral>,
) -> Vec<AnthropicTool> {
    let last = tools.len().saturating_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| AnthropicTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            eager_input_streaming: eager_input_streaming.then_some(true),
            strict: strict.then_some(true),
            input_schema: tool.input_schema.clone(),
            cache_control: if index == last {
                cache_control.clone()
            } else {
                None
            },
        })
        .collect()
}

/// Resolve the `thinking` block. Only reasoning models get one, matching pi.
pub fn resolve_thinking(
    reasoning: bool,
    force_adaptive: bool,
    options: &ThinkingOptions,
) -> Option<Value> {
    if !reasoning {
        return None;
    }
    match options.enabled {
        Some(true) => {
            let display = options
                .display
                .clone()
                .unwrap_or_else(|| "summarized".to_string());
            if force_adaptive {
                Some(json!({ "type": "adaptive", "display": display }))
            } else {
                Some(json!({
                    "type": "enabled",
                    "budget_tokens": options.budget_tokens.unwrap_or(1024),
                    "display": display,
                }))
            }
        }
        // `thinkingLevelMap.off !== null` holds for the models we target.
        Some(false) => Some(json!({ "type": "disabled" })),
        None => None,
    }
}

fn cache_slot(block: &mut ContentBlock) -> Option<&mut Option<CacheControlEphemeral>> {
    match block {
        ContentBlock::Text { cache_control, .. }
        | ContentBlock::Image { cache_control, .. }
        | ContentBlock::ToolResult { cache_control, .. } => Some(cache_control),
        _ => None,
    }
}

/// Mark the last user/system message's final eligible block, converting a bare
/// string to a marked block array like pi's `convertMessages` does.
pub fn apply_conversation_cache_breakpoint(
    messages: &mut [AnthropicMessage],
    cache_control: &Option<CacheControlEphemeral>,
) {
    let Some(cache_control) = cache_control else {
        return;
    };
    let Some(last_message) = messages.last_mut() else {
        return;
    };
    if last_message.role != "user" && last_message.role != "system" {
        return;
    }
    match &mut last_message.content {
        MessageContent::Text(text) => {
            last_message.content = MessageContent::Blocks(vec![ContentBlock::Text {
                text: std::mem::take(text),
                cache_control: Some(cache_control.clone()),
            }]);
        }
        MessageContent::Blocks(blocks) => {
            if let Some(block) = blocks.last_mut() {
                if let Some(slot) = cache_slot(block) {
                    *slot = Some(cache_control.clone());
                }
            }
        }
    }
}

/// Assemble Anthropic Messages params with pi's cache semantics.
pub fn build_anthropic_params(
    model: String,
    system_text: &str,
    tools: &[ToolSpec],
    mut messages: Vec<AnthropicMessage>,
    options: &AnthropicBuildOptions,
) -> AnthropicParams {
    let cache = get_cache_control(
        options.cache_retention,
        options.supports_long_cache_retention,
    );
    let cache_control = cache.cache_control;

    apply_conversation_cache_breakpoint(&mut messages, &cache_control);

    let tool_cache_control = if options.supports_cache_control_on_tools {
        cache_control.clone()
    } else {
        None
    };
    let tools = if tools.is_empty() {
        None
    } else {
        Some(convert_tools(
            tools,
            options.supports_eager_tool_input_streaming,
            options.strict_tools,
            tool_cache_control,
        ))
    };

    AnthropicParams {
        model,
        messages,
        max_tokens: options.max_tokens.unwrap_or(options.default_max_tokens),
        stream: true,
        system: build_system(system_text, cache_control),
        temperature: options.temperature,
        tools,
        thinking: resolve_thinking(
            options.reasoning,
            options.force_adaptive_thinking,
            &options.thinking,
        ),
    }
}

#[cfg(test)]
pub(crate) fn block_has_cache_marker(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text { cache_control, .. }
        | ContentBlock::Image { cache_control, .. }
        | ContentBlock::ToolResult { cache_control, .. } => cache_control.is_some(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{convert_messages, AssistantBlock, ContentPart, TranscriptMessage};

    fn options() -> AnthropicBuildOptions {
        AnthropicBuildOptions {
            cache_retention: CacheRetention::Short,
            supports_long_cache_retention: true,
            supports_cache_control_on_tools: true,
            supports_eager_tool_input_streaming: true,
            strict_tools: false,
            max_tokens: None,
            default_max_tokens: 4096,
            temperature: None,
            reasoning: true,
            force_adaptive_thinking: false,
            thinking: ThinkingOptions {
                enabled: Some(false),
                ..Default::default()
            },
        }
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: format!("{name} tool"),
            input_schema: json!({"type": "object", "properties": {}, "required": []}),
        }
    }

    fn user_text(text: &str) -> AnthropicMessage {
        AnthropicMessage {
            role: "user",
            content: MessageContent::Text(text.to_string()),
        }
    }

    fn user_blocks(blocks: Vec<ContentBlock>) -> AnthropicMessage {
        AnthropicMessage {
            role: "user",
            content: MessageContent::Blocks(blocks),
        }
    }

    #[test]
    fn system_block_carries_marker() {
        let params = build_anthropic_params(
            "claude".into(),
            "system",
            &[],
            vec![user_text("hi")],
            &options(),
        );
        let system = params.system.unwrap();
        assert_eq!(system.len(), 1);
        assert!(system[0].cache_control.is_some());
    }

    #[test]
    fn empty_system_is_omitted() {
        let params = build_anthropic_params("claude".into(), "", &[], vec![], &options());
        assert!(params.system.is_none());
    }

    #[test]
    fn only_last_tool_is_marked() {
        let params = build_anthropic_params(
            "claude".into(),
            "s",
            &[tool("read"), tool("bash"), tool("edit")],
            vec![],
            &options(),
        );
        let tools = params.tools.unwrap();
        assert!(tools[0].cache_control.is_none());
        assert!(tools[1].cache_control.is_none());
        assert!(tools[2].cache_control.is_some());
    }

    #[test]
    fn tool_markers_suppressed_when_unsupported() {
        let mut opts = options();
        opts.supports_cache_control_on_tools = false;
        let params = build_anthropic_params("claude".into(), "s", &[tool("read")], vec![], &opts);
        assert!(params.tools.unwrap()[0].cache_control.is_none());
    }

    #[test]
    fn bare_string_is_converted_to_marked_block() {
        let mut messages = vec![user_text("hi")];
        let marker = Some(CacheControlEphemeral {
            r#type: "ephemeral",
            ttl: None,
        });
        apply_conversation_cache_breakpoint(&mut messages, &marker);
        match &messages[0].content {
            MessageContent::Blocks(blocks) => match &blocks[0] {
                ContentBlock::Text { cache_control, .. } => assert!(cache_control.is_some()),
                _ => panic!("expected text"),
            },
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn trailing_assistant_message_is_not_marked() {
        let params = build_anthropic_params(
            "claude".into(),
            "s",
            &[],
            vec![AnthropicMessage {
                role: "assistant",
                content: MessageContent::Blocks(vec![ContentBlock::Text {
                    text: "a".into(),
                    cache_control: None,
                }]),
            }],
            &options(),
        );
        match &params.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert!(!block_has_cache_marker(&blocks[0]));
            }
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn tool_result_block_is_marked() {
        let mut messages = vec![user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: MessageContent::Text("ok".into()),
            is_error: false,
            cache_control: None,
        }])];
        let marker = Some(CacheControlEphemeral {
            r#type: "ephemeral",
            ttl: None,
        });
        apply_conversation_cache_breakpoint(&mut messages, &marker);
        match &messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert!(block_has_cache_marker(&blocks[0]));
            }
            _ => panic!("expected blocks"),
        }
    }

    #[test]
    fn none_retention_emits_no_markers_anywhere() {
        let mut opts = options();
        opts.cache_retention = CacheRetention::None;
        let params = build_anthropic_params(
            "claude".into(),
            "s",
            &[tool("read")],
            vec![user_text("hi")],
            &opts,
        );
        assert!(params.system.unwrap()[0].cache_control.is_none());
        assert!(params.tools.unwrap()[0].cache_control.is_none());
        match &params.messages[0].content {
            MessageContent::Text(_) => {}
            MessageContent::Blocks(blocks) => assert!(!block_has_cache_marker(&blocks[0])),
        }
    }

    #[test]
    fn thinking_disabled_on_reasoning_model() {
        let value = resolve_thinking(
            true,
            false,
            &ThinkingOptions {
                enabled: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(value, Some(json!({"type": "disabled"})));
    }

    #[test]
    fn no_thinking_on_non_reasoning_model() {
        let value = resolve_thinking(
            false,
            false,
            &ThinkingOptions {
                enabled: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(value, None);
    }

    #[test]
    fn budget_thinking_shape() {
        let value = resolve_thinking(
            true,
            false,
            &ThinkingOptions {
                enabled: Some(true),
                budget_tokens: Some(2048),
                ..Default::default()
            },
        );
        assert_eq!(
            value,
            Some(json!({"type": "enabled", "budget_tokens": 2048, "display": "summarized"}))
        );
    }

    fn parity_options(cache_retention: CacheRetention) -> AnthropicBuildOptions {
        AnthropicBuildOptions {
            cache_retention,
            supports_long_cache_retention: true,
            supports_cache_control_on_tools: true,
            supports_eager_tool_input_streaming: true,
            strict_tools: false,
            max_tokens: Some(64_000),
            default_max_tokens: 64_000,
            temperature: None,
            reasoning: true,
            force_adaptive_thinking: false,
            thinking: ThinkingOptions {
                enabled: Some(false),
                ..Default::default()
            },
        }
    }

    fn parity_tools() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            },
            ToolSpec {
                name: "bash".into(),
                description: "Run a shell command".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"]
                }),
            },
        ]
    }

    fn assert_parity(fixture: &str, built: AnthropicParams, label: &str) {
        let expected: Value = serde_json::from_str(fixture).expect("fixture parses");
        let actual = serde_json::to_value(built).expect("params serialize");
        assert_eq!(actual, expected, "{label} differs from pi");
    }

    /// End-to-end parity against requests captured from pi itself (VED-313).
    /// Regenerate fixtures with `./scripts/parity-check.sh`.
    #[test]
    fn matches_captured_pi_basic() {
        let built = build_anthropic_params(
            "claude-sonnet-4-5".into(),
            "You are pi, a coding agent. Be concise.",
            &parity_tools(),
            vec![user_text("hello")],
            &parity_options(CacheRetention::Short),
        );
        assert_parity(
            include_str!("../../../harness/fixtures/anthropic-basic.json"),
            built,
            "basic",
        );
    }

    #[test]
    fn matches_captured_pi_long_retention() {
        let built = build_anthropic_params(
            "claude-sonnet-4-5".into(),
            "You are pi, a coding agent. Be concise.",
            &parity_tools(),
            vec![user_text("hello")],
            &parity_options(CacheRetention::Long),
        );
        assert_parity(
            include_str!("../../../harness/fixtures/anthropic-long-retention.json"),
            built,
            "long-retention",
        );
    }

    #[test]
    fn matches_captured_pi_no_tools() {
        let built = build_anthropic_params(
            "claude-sonnet-4-5".into(),
            "You are pi.",
            &[],
            vec![user_text("hi")],
            &parity_options(CacheRetention::Short),
        );
        assert_parity(
            include_str!("../../../harness/fixtures/anthropic-no-tools.json"),
            built,
            "no-tools",
        );
    }

    #[test]
    fn matches_captured_pi_tool_use() {
        let transcript = vec![
            TranscriptMessage::UserText("read package.json".into()),
            TranscriptMessage::Assistant(vec![AssistantBlock::ToolCall {
                id: "toolu_1".into(),
                name: "read".into(),
                arguments: json!({"path": "package.json"}),
            }]),
            TranscriptMessage::ToolResult {
                tool_call_id: "toolu_1".into(),
                content: vec![ContentPart::Text {
                    text: "{\"name\":\"x\"}".into(),
                }],
                is_error: false,
            },
            TranscriptMessage::UserText("summarize it".into()),
        ];
        let built = build_anthropic_params(
            "claude-sonnet-4-5".into(),
            "You are pi, a coding agent. Be concise.",
            &parity_tools(),
            convert_messages(&transcript),
            &parity_options(CacheRetention::Short),
        );
        assert_parity(
            include_str!("../../../harness/fixtures/anthropic-tool-use.json"),
            built,
            "tool-use",
        );
    }
}
