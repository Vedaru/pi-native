//! Anthropic Messages request construction with pi's cache-breakpoint rules.
//!
//! Sources mirrored: `packages/ai/src/api/anthropic-messages.ts`
//! (`buildParams`, `convertTools`, the conversation breakpoint at the end of
//! `convertMessages`, and the `thinking` block).

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
        content: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
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

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AnthropicMessage {
    pub role: &'static str,
    pub content: Vec<ContentBlock>,
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

fn eligible_for_cache_marker(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Text { .. } | ContentBlock::Image { .. } | ContentBlock::ToolResult { .. }
    )
}

/// Mark the last user/system message's final eligible block.
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
    let Some(last_block) = last_message.content.last_mut() else {
        return;
    };
    if !eligible_for_cache_marker(last_block) {
        return;
    }
    match last_block {
        ContentBlock::Text {
            cache_control: slot,
            ..
        }
        | ContentBlock::Image {
            cache_control: slot,
            ..
        }
        | ContentBlock::ToolResult {
            cache_control: slot,
            ..
        } => {
            *slot = Some(cache_control.clone());
        }
        _ => {}
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
mod tests {
    use super::*;
    use serde_json::json;

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
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                cache_control: None,
            }],
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
    fn last_user_text_block_is_marked() {
        let mut messages = vec![
            AnthropicMessage {
                role: "assistant",
                content: vec![ContentBlock::Text {
                    text: "a".into(),
                    cache_control: None,
                }],
            },
            user_text("b"),
        ];
        let marker = Some(CacheControlEphemeral {
            r#type: "ephemeral",
            ttl: None,
        });
        apply_conversation_cache_breakpoint(&mut messages, &marker);
        match &messages[1].content[0] {
            ContentBlock::Text { cache_control, .. } => assert!(cache_control.is_some()),
            _ => panic!("expected text"),
        }
        match &messages[0].content[0] {
            ContentBlock::Text { cache_control, .. } => assert!(cache_control.is_none()),
            _ => panic!("expected text"),
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
                content: vec![ContentBlock::Text {
                    text: "a".into(),
                    cache_control: None,
                }],
            }],
            &options(),
        );
        match &params.messages[0].content[0] {
            ContentBlock::Text { cache_control, .. } => assert!(cache_control.is_none()),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn tool_result_block_is_marked() {
        let mut messages = vec![AnthropicMessage {
            role: "user",
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: json!("ok"),
                is_error: None,
                cache_control: None,
            }],
        }];
        let marker = Some(CacheControlEphemeral {
            r#type: "ephemeral",
            ttl: None,
        });
        apply_conversation_cache_breakpoint(&mut messages, &marker);
        match &messages[0].content[0] {
            ContentBlock::ToolResult { cache_control, .. } => assert!(cache_control.is_some()),
            _ => panic!("expected tool_result"),
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
        match &params.messages[0].content[0] {
            ContentBlock::Text { cache_control, .. } => assert!(cache_control.is_none()),
            _ => panic!("expected text"),
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

    /// End-to-end parity against a request captured from pi itself (VED-313).
    /// Regenerate the fixture with `node harness/capture-anthropic.mjs
    /// harness/scenarios/anthropic-basic.json`.
    #[test]
    fn matches_captured_pi_request() {
        let expected: Value = serde_json::from_str(include_str!(
            "../../../harness/fixtures/anthropic-claude-sonnet-4-5.json"
        ))
        .expect("fixture parses");

        let tools = vec![
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
        ];

        let opts = AnthropicBuildOptions {
            cache_retention: CacheRetention::Short,
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
        };

        let actual = serde_json::to_value(build_anthropic_params(
            "claude-sonnet-4-5".into(),
            "You are pi, a coding agent. Be concise.",
            &tools,
            vec![user_text("hello")],
            &opts,
        ))
        .expect("params serialize");

        assert_eq!(
            actual, expected,
            "native request differs from captured pi request"
        );
    }
}
