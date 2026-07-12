//! system-blocks, build_request, reasoning-effort tests for the Anthropic
//! provider. Moved from `tests.rs` to `tests/request.rs` in wave 2 of
//! epic `456f` to bring the test module under the 50 KB / 1,500-line size
//! guard. The split boundary is the `// ─── Empty-segment handling tests`
//! section comment that originally sat at L437 of `tests.rs`: this file
//! picks up everything from that comment onward, up to the
//! `// ─── End-to-end prompt assembly → Anthropic request coverage ──` break
//! that originally sat at L739.
//!
//! Tests covered (13 total, byte-for-byte identical to the originals):
//! - 2 `test_system_blocks_skips_*` / `test_system_blocks_empty_*` tests
//! - 2 `test_serialize_system_blocks_*` tests
//! - 2 `test_build_request_no_system_*` / `*_all_empty_system_content_*` tests
//! - 4 `test_reasoning_effort_*` tests (none/high/budget-clamp/tool-choice skip)
//! - 3 `test_cache_control_*` tests that are really build_request / system
//!   block tests (`test_cache_control_correct_after_empty_block_filtering`,
//!   `test_cache_control_when_trailing_empty_blocks_are_filtered`,
//!   `test_populated_segments_unchanged`)

use super::*;
use super::{test_anthropic_config, test_provider};
use crate::message::Conversation;

// ─── Empty-segment handling tests ─────────────────────────────────────────

#[test]
fn test_system_blocks_skips_empty_and_whitespace_content() {
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::System,
        content: vec![
            ContentBlock::Text {
                text: "base prompt".to_string(),
            },
            ContentBlock::Text {
                text: "".to_string(),
            },
            ContentBlock::Text {
                text: "   \n  ".to_string(),
            },
            ContentBlock::Text {
                text: "dynamic tail".to_string(),
            },
        ],
        metadata: None,
    });
    conv.push(crate::message::Message::user("hello"));

    let blocks = AnthropicProvider::system_blocks(&conv);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].text, "base prompt");
    assert_eq!(blocks[1].text, "dynamic tail");
}

#[test]
fn test_system_blocks_empty_conversation_produces_no_blocks() {
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));

    let blocks = AnthropicProvider::system_blocks(&conv);
    assert!(blocks.is_empty());
}

#[test]
fn test_serialize_system_blocks_returns_none_for_empty() {
    let result = AnthropicProvider::serialize_system_blocks(&[]);
    assert!(result.is_none());
}

#[test]
fn test_serialize_system_blocks_single_no_cache() {
    let blocks = vec![AnthropicSystemBlock {
        text: "hello".to_string(),
        cache_control: None,
    }];
    let result = AnthropicProvider::serialize_system_blocks(&blocks);
    assert_eq!(result, Some(Value::String("hello".to_string())));
}

#[test]
fn test_build_request_no_system_field_when_no_system_message() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));

    let req = provider.build_request(&conv, &[], None);
    assert!(
        req.get("system").is_none(),
        "system field should be absent when there are no system blocks"
    );
}

#[test]
fn test_build_request_with_all_empty_system_content_omits_system() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::System,
        content: vec![
            ContentBlock::Text {
                text: "".to_string(),
            },
            ContentBlock::Text {
                text: "   ".to_string(),
            },
        ],
        metadata: None,
    });
    conv.push(crate::message::Message::user("hello"));

    let req = provider.build_request(&conv, &[], None);
    assert!(
        req.get("system").is_none(),
        "system field should be absent when all system content blocks are empty"
    );
}

// ─── B5: reasoning-effort -> thinking block ─────────────────────────────

#[test]
fn test_reasoning_effort_none_omits_thinking_block() {
    // None must preserve pre-B5 behavior: no `thinking` block at all.
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));
    let req = provider.build_request(&conv, &[], None);
    assert!(
        req.get("thinking").is_none(),
        "thinking block must be absent when reasoning_effort is None"
    );
}

#[test]
fn test_reasoning_effort_high_enables_thinking() {
    use crate::provider::ReasoningEffort;
    let mut config = test_anthropic_config();
    config.reasoning_effort = Some(ReasoningEffort::High);
    let provider = AnthropicProvider::new(config);
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));
    let req = provider.build_request(&conv, &[], None);
    assert_eq!(req["thinking"]["type"], "enabled");
    // High budget (24000) is below max_tokens (64000), so it passes through.
    assert_eq!(req["thinking"]["budget_tokens"], 24_000);
}

#[test]
fn test_reasoning_effort_budget_clamped_below_max_tokens() {
    use crate::provider::{ProviderCapabilities, ReasoningEffort};
    let mut config = test_anthropic_config();
    // Force a tiny output limit so the tier budget must be clamped.
    config.capabilities = ProviderCapabilities {
        streaming: true,
        max_tokens_default: Some(2_000),
    };
    config.reasoning_effort = Some(ReasoningEffort::High);
    let provider = AnthropicProvider::new(config);
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));
    let req = provider.build_request(&conv, &[], None);
    assert_eq!(req["thinking"]["type"], "enabled");
    // Clamped to max_tokens - 1.
    assert_eq!(req["thinking"]["budget_tokens"], 1_999);
}

#[test]
fn test_default_policy_enables_minimax_anthropic_thinking() {
    use crate::provider::{FormatFamily, ProviderCapabilities, default_reasoning_effort_for_model};

    let mut config = test_anthropic_config();
    config.model_id = "MiniMax-M2.5".to_string();
    config.capabilities = ProviderCapabilities {
        streaming: true,
        max_tokens_default: Some(2_000),
    };
    config.reasoning_effort =
        default_reasoning_effort_for_model(true, FormatFamily::Anthropic, &config.model_id);

    let provider = AnthropicProvider::new(config);
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));
    let req = provider.build_request(&conv, &[], None);

    assert_eq!(req["thinking"]["type"], "enabled");
    let budget = req["thinking"]["budget_tokens"]
        .as_u64()
        .expect("budget_tokens is numeric");
    let max_tokens = req["max_tokens"].as_u64().expect("max_tokens is numeric");
    assert!(
        budget < max_tokens,
        "Anthropic-compatible MiniMax thinking budget must stay below max_tokens"
    );
    assert_eq!(budget, 1_999);
}

#[test]
fn test_reasoning_effort_enabled_skips_forced_tool_choice() {
    use crate::provider::ReasoningEffort;
    let mut config = test_anthropic_config();
    config.reasoning_effort = Some(ReasoningEffort::Medium);
    let provider = AnthropicProvider::new(config);
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));
    let tools = vec![json!({
        "name": "do_thing",
        "description": "does a thing",
        "input_schema": {"type": "object", "properties": {}}
    })];
    let req = provider.build_request(&conv, &tools, Some(ToolChoice::Required));
    // With thinking enabled, the forcing tool_choice must NOT be emitted.
    assert!(
        req.get("tool_choice").is_none(),
        "tool_choice must be omitted when thinking is enabled"
    );
    assert_eq!(req["thinking"]["type"], "enabled");
}

#[test]
fn test_cache_control_correct_after_empty_block_filtering() {
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::System,
        content: vec![
            ContentBlock::Text {
                text: "base prompt".to_string(),
            },
            ContentBlock::Text {
                text: "".to_string(),
            },
            ContentBlock::Text {
                text: "tools".to_string(),
            },
            ContentBlock::Text {
                text: "   ".to_string(),
            },
            ContentBlock::Text {
                text: "dynamic tail".to_string(),
            },
        ],
        metadata: Some(crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some("stable_prefix".to_string()),
                }
            })),
        }),
    });
    conv.push(crate::message::Message::user("hello"));

    let blocks = AnthropicProvider::system_blocks(&conv);
    // After filtering: ["base prompt", "tools", "dynamic tail"]
    assert_eq!(blocks.len(), 3);
    // First two should have cache_control, last should not
    assert!(
        blocks[0].cache_control.is_some(),
        "first block should have cache_control"
    );
    assert!(
        blocks[1].cache_control.is_some(),
        "second block should have cache_control"
    );
    assert!(
        blocks[2].cache_control.is_none(),
        "last block should NOT have cache_control"
    );
}

#[test]
fn test_cache_control_when_trailing_empty_blocks_are_filtered() {
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::System,
        content: vec![
            ContentBlock::Text {
                text: "base prompt".to_string(),
            },
            ContentBlock::Text {
                text: "cached segment".to_string(),
            },
            ContentBlock::Text {
                text: "".to_string(),
            },
        ],
        metadata: Some(crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some("stable_prefix".to_string()),
                }
            })),
        }),
    });
    conv.push(crate::message::Message::user("hello"));

    let blocks = AnthropicProvider::system_blocks(&conv);
    // After filtering: ["base prompt", "cached segment"]
    assert_eq!(blocks.len(), 2);
    assert!(
        blocks[0].cache_control.is_some(),
        "first block should have cache_control"
    );
    assert!(
        blocks[1].cache_control.is_none(),
        "last non-empty block should NOT have cache_control (it is now the tail)"
    );
}

#[test]
fn test_populated_segments_unchanged() {
    // Verify that the existing behavior for fully-populated segments is preserved
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system_with_metadata(
        "base prompt",
        crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some("stable_prefix".to_string()),
                }
            })),
        },
    ));
    conv.messages[0].content.push(ContentBlock::Text {
        text: "repo map".to_string(),
    });
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run shell",
        "input_schema": {"type": "object"}
    })];

    let req = provider.build_request(&conv, &tools, None);
    let system = req["system"].as_array().expect("system block array");
    assert_eq!(system.len(), 2);
    assert_eq!(system[0]["text"], "base prompt");
    assert_eq!(system[1]["text"], "repo map");
    assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));
    assert!(system[1].get("cache_control").is_none());
    assert_eq!(
        req["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

// ─── Native assistant thinking replay ──────────────────────────────────────

#[test]
fn test_build_request_replays_signed_thinking_in_original_position() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::Assistant,
        content: vec![
            ContentBlock::text("before thinking"),
            ContentBlock::Thinking {
                thinking: "internal reasoning".to_string(),
                signature: Some("sig_abc".to_string()),
            },
            ContentBlock::text("visible output"),
        ],
        metadata: None,
    });

    let req = provider.build_request(&conv, &[], None);
    let messages = req["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    let content = messages[0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(
        content[0],
        json!({"type": "text", "text": "before thinking"})
    );
    assert_eq!(
        content[1],
        json!({"type": "thinking", "thinking": "internal reasoning", "signature": "sig_abc"})
    );
    assert_eq!(
        content[2],
        json!({"type": "text", "text": "visible output"})
    );
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "")
    );
}

#[test]
fn test_build_request_replays_redacted_thinking_without_empty_text() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::Assistant,
        content: vec![
            ContentBlock::RedactedThinking {
                data: "opaque_data_blob".to_string(),
            },
            ContentBlock::text("visible output"),
        ],
        metadata: None,
    });

    let req = provider.build_request(&conv, &[], None);
    let content = req["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(
        content[0],
        json!({"type": "redacted_thinking", "data": "opaque_data_blob"})
    );
    assert_eq!(
        content[1],
        json!({"type": "text", "text": "visible output"})
    );
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "")
    );
}

#[test]
fn test_build_request_replays_unknown_passthrough_without_type_override() {
    let provider = test_provider();
    let mut extra = serde_json::Map::new();
    extra.insert("foo".to_string(), json!("bar"));
    extra.insert("type".to_string(), json!("attempted_override"));
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::Assistant,
        content: vec![
            ContentBlock::Unknown {
                content_type: "custom_provider_block".to_string(),
                extra,
            },
            ContentBlock::text("visible output"),
        ],
        metadata: None,
    });

    let req = provider.build_request(&conv, &[], None);
    let content = req["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(
        content[0],
        json!({"type": "custom_provider_block", "foo": "bar"})
    );
    assert_eq!(
        content[1],
        json!({"type": "text", "text": "visible output"})
    );
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == "")
    );
}

#[test]
fn test_build_request_replays_all_anthropic_thinking_blocks() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "internal reasoning".to_string(),
                signature: Some("sig_abc".to_string()),
            },
            ContentBlock::RedactedThinking {
                data: "opaque".to_string(),
            },
        ],
        metadata: None,
    });

    let req = provider.build_request(&conv, &[], None);
    let content = req["messages"][0]["content"].as_array().unwrap();
    assert_eq!(
        content,
        &vec![
            json!({"type": "thinking", "thinking": "internal reasoning", "signature": "sig_abc"}),
            json!({"type": "redacted_thinking", "data": "opaque"}),
        ]
    );
}
