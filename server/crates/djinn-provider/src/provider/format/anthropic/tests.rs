// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use super::*;
use crate::message::{Conversation, Message};
use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig};
use axum::{Router, routing::post};
use futures::TryStreamExt;

fn spawn_sse_server(status: u16, body: &'static str) -> String {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind local tcp listener");
    let addr = listener.local_addr().expect("local addr");
    listener.set_nonblocking(true).expect("set nonblocking");

    let rt = tokio::runtime::Handle::current();
    rt.spawn(async move {
        let app = Router::new().route(
            "/v1/messages",
            post(move |_req: axum::extract::Request| async move {
                (
                    axum::http::StatusCode::from_u16(status).expect("status"),
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }),
        );

        let tokio_listener =
            tokio::net::TcpListener::from_std(listener).expect("convert to tokio listener");
        axum::serve(tokio_listener, app).await.ok();
    });

    format!("http://{}:{}", addr.ip(), addr.port())
}

fn test_anthropic_config() -> ProviderConfig {
    ProviderConfig {
        base_url: "https://example.com".to_string(),
        auth: AuthMethod::NoAuth,
        format_family: FormatFamily::Anthropic,
        model_id: "claude-3-5-sonnet".to_string(),
        context_window: 200_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: std::collections::HashMap::new(),
        capabilities: ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(64_000),
        },
        reasoning_effort: None,
    }
}

fn test_provider() -> AnthropicProvider {
    AnthropicProvider::new(test_anthropic_config())
}

#[test]
fn test_message_start_extracts_input_tokens() {
    let data = r#"{"type":"message_start","message":{"id":"msg_01","type":"message","role":"assistant","content":[],"model":"claude-3-5-sonnet","stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":25,"output_tokens":1}}}"#;
    let mut acc = None;
    let mut input_tokens = 0u32;
    let mut cache_read = 0u32;
    let mut cache_write = 0u32;
    let events = parse_anthropic_event(
        "message_start",
        data,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );
    assert!(events.is_empty());
    assert_eq!(input_tokens, 25);
}

#[test]
fn test_message_start_extracts_cache_tokens() {
    let data = r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":1000,"cache_creation_input_tokens":40}}}"#;
    let mut acc = None;
    let mut input_tokens = 0u32;
    let mut cache_read = 0u32;
    let mut cache_write = 0u32;
    parse_anthropic_event(
        "message_start",
        data,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );
    assert_eq!(input_tokens, 25);
    assert_eq!(cache_read, 1000);
    assert_eq!(cache_write, 40);
}

#[test]
fn test_text_delta_event() {
    let data =
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
    let mut acc = None;
    let mut input_tokens = 0u32;
    let mut cache_read = 0u32;
    let mut cache_write = 0u32;
    let events = parse_anthropic_event(
        "content_block_delta",
        data,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );
    assert_eq!(events.len(), 1);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::Text { text }) => assert_eq!(text, "Hello"),
        _ => panic!("expected text delta"),
    }
}

#[test]
fn test_tool_use_accumulation() {
    let mut acc = None;
    let mut input_tokens = 0u32;
    let mut cache_read = 0u32;
    let mut cache_write = 0u32;

    let start = r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"shell"}}"#;
    let e1 = parse_anthropic_event(
        "content_block_start",
        start,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );
    assert!(e1.is_empty());
    assert!(acc.is_some());

    let frag1 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"l"}}"#;
    parse_anthropic_event(
        "content_block_delta",
        frag1,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );

    let frag2 = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"s\",\"dir\":\"/tmp\"}"}}"#;
    parse_anthropic_event(
        "content_block_delta",
        frag2,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );

    let stop = r#"{"type":"content_block_stop","index":0}"#;
    let events = parse_anthropic_event(
        "content_block_stop",
        stop,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );
    assert_eq!(events.len(), 1);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) => {
            assert_eq!(id.as_str(), "toolu_01");
            assert_eq!(name.as_str(), "shell");
            assert_eq!(input["cmd"].as_str(), Some("ls"));
            assert_eq!(input["dir"].as_str(), Some("/tmp"));
        }
        _ => panic!("expected tool use"),
    }
    assert!(acc.is_none());
}

#[test]
fn test_message_delta_emits_usage() {
    let data = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#;
    let mut acc = None;
    let mut input_tokens = 10u32;
    let mut cache_read = 3u32;
    let mut cache_write = 7u32;
    let events = parse_anthropic_event(
        "message_delta",
        data,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );
    assert_eq!(events.len(), 1);
    match &events[0] {
        StreamEvent::Usage(u) => {
            assert_eq!(u.input, 10);
            assert_eq!(u.output, 42);
            // Cache counts carried from message_start are folded into usage.
            assert_eq!(u.cache_read, 3);
            assert_eq!(u.cache_write, 7);
        }
        _ => panic!("expected usage"),
    }
}

#[test]
fn test_message_stop_emits_done() {
    let data = r#"{"type":"message_stop"}"#;
    let mut acc = None;
    let mut input_tokens = 0u32;
    let mut cache_read = 0u32;
    let mut cache_write = 0u32;
    let events = parse_anthropic_event(
        "message_stop",
        data,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], StreamEvent::Done));
}

#[test]
fn test_build_request_always_populates_system_field() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system("system prompt"));
    conv.push(crate::message::Message::user("first user"));
    conv.push(crate::message::Message::assistant("first assistant"));
    conv.push(crate::message::Message::user("second user"));

    let req = provider.build_request(&conv, &[], None);
    assert_eq!(req["system"], "system prompt");
    let messages = req["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"][0]["text"], "first user");
}

#[test]
fn test_system_blocks_consume_explicit_stable_prefix_metadata_contract() {
    let mut conv = Conversation::default();
    conv.push(crate::message::Message {
        role: crate::message::Role::System,
        content: vec![
            ContentBlock::text("base prompt"),
            ContentBlock::text("project context"),
            ContentBlock::text("repo map"),
            ContentBlock::text("dynamic tail"),
        ],
        metadata: Some(crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: {
                    "kind": ANTHROPIC_STABLE_PREFIX_KIND,
                }
            })),
        }),
    });
    conv.push(crate::message::Message::user("hello"));

    let blocks = AnthropicProvider::system_blocks(&conv);
    assert_eq!(blocks.len(), 4);
    // `kind` is internal metadata and must NOT leak into the wire object.
    assert_eq!(blocks[0].cache_control, Some(json!({"type": "ephemeral"})));
    assert_eq!(blocks[1].cache_control, Some(json!({"type": "ephemeral"})));
    assert_eq!(blocks[2].cache_control, Some(json!({"type": "ephemeral"})));
    assert_eq!(blocks[3].cache_control, None);
}

#[test]
fn test_build_request_preserves_separate_system_blocks_with_cache_control() {
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
                    kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
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
    assert_eq!(req["tools"][0]["name"], "shell");
    assert_eq!(
        req["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

#[test]
fn test_content_block_delta_input_json_without_active_tool_is_ignored() {
    let data =
        r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}"#;
    let mut acc = None;
    let mut input_tokens = 0u32;
    let mut cache_read = 0u32;
    let mut cache_write = 0u32;
    let events = parse_anthropic_event(
        "content_block_delta",
        data,
        &mut acc,
        &mut input_tokens,
        &mut cache_read,
        &mut cache_write,
    );
    assert!(events.is_empty());
    assert!(acc.is_none());
}

#[tokio::test]
async fn test_stream_uses_payload_type_over_sse_event_name() {
    let body = concat!(
        "event: nope\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n",
        "event: wrong-name\n",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello from payload\"}}\n\n",
        "event: definitely-not-message-delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n\n",
        "event: not-message-stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let mut config = test_anthropic_config();
    config.base_url = spawn_sse_server(200, body);
    let provider = AnthropicProvider::new(config);
    let mut conv = Conversation::new();
    conv.push(Message::user("Hello"));

    let events = provider
        .stream(&conv, &[], None)
        .await
        .expect("stream")
        .try_collect::<Vec<_>>()
        .await
        .expect("collect events");

    assert_eq!(events.len(), 3);
    match &events[0] {
        StreamEvent::Delta(ContentBlock::Text { text }) => {
            assert_eq!(text, "Hello from payload")
        }
        _ => panic!("expected text delta"),
    }
    match &events[1] {
        StreamEvent::Usage(u) => {
            assert_eq!(u.input, 7);
            assert_eq!(u.output, 9);
        }
        _ => panic!("expected usage"),
    }
    assert!(matches!(events[2], StreamEvent::Done));
}

#[tokio::test]
async fn test_streamed_error_event_is_ignored_but_http_error_shape_surfaces() {
    let body = concat!(
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"try again later\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let mut config = test_anthropic_config();
    config.base_url = spawn_sse_server(200, body);
    let provider = AnthropicProvider::new(config);
    let mut conv = Conversation::new();
    conv.push(Message::user("Hello"));

    let events = provider
        .stream(&conv, &[], None)
        .await
        .expect("stream")
        .try_collect::<Vec<_>>()
        .await
        .expect("collect events");
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], StreamEvent::Done));

    let error_body =
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
    let mut error_config = test_anthropic_config();
    error_config.base_url = spawn_sse_server(401, error_body);
    let provider = AnthropicProvider::new(error_config);
    let err = provider
        .stream(&conv, &[], None)
        .await
        .expect("stream")
        .try_collect::<Vec<_>>()
        .await
        .expect_err("expected anthropic http error");
    let err_text = err.to_string();
    assert!(err_text.contains("provider API error 401 Unauthorized"));
    assert!(err_text.contains("authentication_error"));
    assert!(err_text.contains("invalid x-api-key"));
}

#[test]
fn test_build_request_sets_required_tool_choice_when_tools_present() {
    let provider = test_provider();
    let mut conv = Conversation::new();
    conv.push(crate::message::Message::user("Hello"));
    let tools = vec![json!({
        "name": "shell",
        "description": "Run shell",
        "input_schema": {"type": "object"}
    })];

    let req = provider.build_request(&conv, &tools, Some(ToolChoice::Required));
    assert_eq!(req["tool_choice"]["type"], "any");
}

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

// ─── End-to-end prompt assembly → Anthropic request coverage ──────────────

/// Build a system message using the current chat-layer production contract:
/// trim the base prompt, keep project context as a stable block,
/// collapse dynamic client/task text into a trailing block, and attach
/// Anthropic cache metadata only for Anthropic models.
fn build_system_message_for_test(
    base_prompt: &str,
    project_context: Option<&str>,
    client_system: Option<&str>,
    is_anthropic: bool,
) -> Message {
    let mut content = vec![ContentBlock::text(base_prompt.trim())];
    if let Some(project_context) = project_context.filter(|s| !s.trim().is_empty()) {
        content.push(ContentBlock::text(project_context));
    }
    if let Some(client_system) = client_system.filter(|s| !s.trim().is_empty()) {
        content.push(ContentBlock::text(client_system));
    }

    let metadata = is_anthropic.then(|| crate::message::MessageMeta {
        input_tokens: None,
        output_tokens: None,
        timestamp: None,
        provider_data: Some(json!({
            ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                kind: Some("stable_prefix".to_string()),
            }
        })),
    });

    Message {
        role: crate::message::Role::System,
        content,
        metadata,
    }
}

/// E2E: with repo map present, Anthropic keeps tool definitions in the
/// dedicated request-level `tools` block while preserving the system block
/// ordering from `chat.rs` (base -> project context -> repo map -> dynamic
/// tail). Stable-prefix `cache_control` appears on the stable system prefix
/// and on the last tool-definition entry, but not on the dynamic tail.
#[test]
fn e2e_system_blocks_ordered_with_cache_control() {
    let provider = test_provider();
    let base = "You are a helpful assistant.";
    let project_context = "## Project Context\nworkspace: demo";
    let client = "Be concise.";

    let sys_msg = build_system_message_for_test(base, Some(project_context), Some(client), true);

    let mut conv = Conversation::new();
    conv.push(sys_msg);
    conv.push(Message::user("What does this project do?"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run a shell command",
        "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
    })];

    let req = provider.build_request(&conv, &tools, None);
    let system = req["system"]
        .as_array()
        .expect("system should be an array when cache_control is present");

    assert_eq!(system.len(), 3, "expected 3 system blocks");
    assert_eq!(system[0]["text"], base.trim());
    assert_eq!(system[1]["text"], project_context);
    assert_eq!(system[2]["text"], client);

    for stable_block in &system[..2] {
        assert_eq!(stable_block["cache_control"], json!({"type": "ephemeral"}));
    }
    assert!(
        system[2].get("cache_control").is_none(),
        "dynamic tail block must not have cache_control"
    );
    assert_eq!(
        req["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

/// E2E: without tools or dynamic context, a single non-cacheable
/// system block collapses to a plain string (no array, no cache_control).
#[test]
fn e2e_single_block_no_cache_control() {
    let provider = test_provider();
    let base = "You are a helpful assistant.";

    let sys_msg = build_system_message_for_test(base, None, None, false);

    let mut conv = Conversation::new();
    conv.push(sys_msg);
    conv.push(Message::user("Hello"));

    let req = provider.build_request(&conv, &[], None);

    assert!(
        req["system"].is_string(),
        "single-block system without cache_control should serialize as a plain string"
    );
    assert_eq!(req["system"], base.trim());
}

/// E2E: Anthropic model with base prompt only (no optional contexts) still
/// serializes as a plain string because the only block is also the dynamic
/// cache boundary and therefore receives no `cache_control`.
#[test]
fn e2e_anthropic_base_only_with_cache_metadata_formats_as_single_block() {
    let provider = test_provider();
    let base = "You are a helpful assistant.";

    let sys_msg = build_system_message_for_test(base, None, None, true);

    let mut conv = Conversation::new();
    conv.push(sys_msg);
    conv.push(Message::user("Hello"));

    let req = provider.build_request(&conv, &[], None);

    assert!(
        req["system"].is_string(),
        "single-block anthropic system should still be a plain string \
             when cache_control is absent on the only block"
    );
    assert_eq!(req["system"], base.trim());
}

/// E2E: session with request-level tools verifies that Anthropic
/// keeps the stable system prefix ordered as base -> project context,
/// preserves the uncached dynamic tail, and still emits the separate
/// request `tools` array unchanged.
#[test]
fn e2e_tools_preserves_both_system_and_tools() {
    let provider = test_provider();
    let base = "You are a helpful assistant.";
    let project_context = "## Tool Definitions\nshell(cmd: string)";

    let sys_msg =
        build_system_message_for_test(base, Some(project_context), Some("be brief"), true);

    let mut conv = Conversation::new();
    conv.push(sys_msg);
    conv.push(Message::user("List files"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run a shell command",
        "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
    })];

    let req = provider.build_request(&conv, &tools, None);
    let system = req["system"]
        .as_array()
        .expect("system should be array with cache_control");
    assert_eq!(system.len(), 3);
    assert_eq!(system[0]["text"], base.trim());
    assert_eq!(system[1]["text"], project_context);
    assert_eq!(system[2]["text"], "be brief");
    assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(system[1]["cache_control"], json!({"type": "ephemeral"}));
    assert!(system[2].get("cache_control").is_none());

    let req_tools = req["tools"].as_array().expect("tools array");
    assert_eq!(req_tools.len(), 1);
    assert_eq!(req_tools[0]["name"], "shell");
}

// ─── B2: cache_control breakpoint cap (Anthropic max 4) ───────────────────

/// Count every `cache_control` marker present across tools, system blocks,
/// and message content in a serialized request body.
fn count_cache_markers(body: &Value) -> usize {
    let mut count = 0;
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        count += tools
            .iter()
            .filter(|t| t.get("cache_control").is_some())
            .count();
    }
    if let Some(system) = body.get("system").and_then(Value::as_array) {
        count += system
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(content) = message.get("content").and_then(Value::as_array) {
                count += content
                    .iter()
                    .filter(|b| b.get("cache_control").is_some())
                    .count();
            }
        }
    }
    count
}

#[test]
fn test_cache_control_markers_capped_at_four() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    // Six non-empty system text blocks with cache metadata. system_blocks
    // marks all-but-last (5 cached), the request marks the last tool (1),
    // and add_message_cache_breakpoint marks the last message (1): 7 raw
    // markers, well over the cap of 4.
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::System,
        content: vec![
            ContentBlock::text("base prompt"),
            ContentBlock::text("project context"),
            ContentBlock::text("repo map"),
            ContentBlock::text("conventions"),
            ContentBlock::text("more stable context"),
            ContentBlock::text("dynamic tail"),
        ],
        metadata: Some(crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
                }
            })),
        }),
    });
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run shell",
        "input_schema": {"type": "object"}
    })];

    let req = provider.build_request(&conv, &tools, None);

    // Hard cap enforced.
    let total = count_cache_markers(&req);
    assert!(
        total <= 4,
        "expected at most 4 cache_control markers, got {total}"
    );
    assert_eq!(total, 4, "should keep exactly 4 markers when over the cap");

    // Highest-priority segments keep their markers: the tool definition (1)
    // and the earliest system blocks (priority after tools).
    assert_eq!(
        req["tools"][0]["cache_control"],
        json!({"type": "ephemeral"}),
        "the tool definition is the highest-priority cache segment and must keep its marker"
    );
    let system = req["system"].as_array().expect("system array");
    assert!(
        system[0].get("cache_control").is_some(),
        "earliest system block must keep its marker"
    );
    assert!(
        system[1].get("cache_control").is_some(),
        "second system block must keep its marker"
    );
    assert!(
        system[2].get("cache_control").is_some(),
        "third system block must keep its marker"
    );
    // Total kept = 1 (tool) + 3 (system) = 4; everything else dropped,
    // including the trailing message breakpoint (lowest priority).
    let messages = req["messages"].as_array().expect("messages array");
    let last = messages.last().expect("last message");
    let last_block = last["content"].as_array().expect("content").last().unwrap();
    assert!(
        last_block.get("cache_control").is_none(),
        "lowest-priority message breakpoint must be dropped past the cap"
    );
}

#[test]
fn test_cache_control_under_cap_is_unchanged() {
    // <= 4 markers: enforcement is a no-op (no regression for the common case).
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
                    kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
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
    // tool(1) + system stable prefix(1) + message breakpoint(1) = 3 <= 4.
    let total = count_cache_markers(&req);
    assert!(total <= 4, "expected <= 4 markers, got {total}");
    // Tool and first system block markers preserved exactly.
    assert_eq!(
        req["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(req["system"][0]["cache_control"]["type"], "ephemeral");
}

// ─── B3: cache stable-prefix drift guard ──────────────────────────────────

/// Build a representative cache-enabled conversation + tools used by the B3
/// drift-guard tests: a stable base prompt, a stable project-context block, a
/// dynamic trailing block, plus a tool definition. Mirrors the production
/// chat-layer contract so the cache_control markers land on tools + system
/// prefix + trailing message breakpoint.
fn drift_guard_fixture() -> (Conversation, Vec<Value>) {
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
        text: "project context / repo map".to_string(),
    });
    conv.messages[0].content.push(ContentBlock::Text {
        text: "dynamic tail".to_string(),
    });
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run shell",
        "input_schema": {"type": "object"}
    })];
    (conv, tools)
}

/// Determinism: identical logical inputs must produce a byte-identical cached
/// prefix, hence an identical stable-prefix hash, across two independent
/// builds. This is the core invariant prompt caching depends on — if it ever
/// fails, some non-deterministic value (timestamp, map-iteration order, …) has
/// leaked into the cached prefix and every cache hit silently becomes a miss.
#[test]
fn test_stable_prefix_hash_is_deterministic() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();

    let body_a = provider.build_request(&conv, &tools, None);
    let body_b = provider.build_request(&conv, &tools, None);

    let hash_a = AnthropicProvider::stable_prefix_hash(&body_a)
        .expect("cache markers present => hash should exist");
    let hash_b = AnthropicProvider::stable_prefix_hash(&body_b)
        .expect("cache markers present => hash should exist");

    assert_eq!(
        hash_a, hash_b,
        "stable cache prefix must hash identically across two builds of the same inputs"
    );
    // And the full serialized prefix bytes must match too (stronger than the
    // hash, and guards against an accidental hash collision masking real drift).
    assert_eq!(
        serde_json::to_string(&body_a["system"]).unwrap(),
        serde_json::to_string(&body_b["system"]).unwrap(),
        "serialized system prefix must be byte-identical"
    );
    assert_eq!(
        serde_json::to_string(&body_a["tools"]).unwrap(),
        serde_json::to_string(&body_b["tools"]).unwrap(),
        "serialized tool prefix must be byte-identical"
    );
}

/// The hash must be sensitive to actual changes in the cached prefix: a
/// perturbed stable block (here, a mutated project-context string) must yield a
/// different hash. Without this, the guard would never detect real drift.
#[test]
fn test_stable_prefix_hash_detects_perturbed_prefix() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();
    let baseline = provider.build_request(&conv, &tools, None);
    let baseline_hash = AnthropicProvider::stable_prefix_hash(&baseline).unwrap();

    // Perturb a STABLE (cache_control-marked) system block, simulating a
    // timestamp / non-deterministic leak into the supposedly-stable prefix.
    let mut perturbed = conv.clone();
    perturbed.messages[0].content[1] = ContentBlock::Text {
        text: "project context / repo map @ 2026-06-01T12:00:00Z".to_string(),
    };
    let perturbed_body = provider.build_request(&perturbed, &tools, None);
    let perturbed_hash = AnthropicProvider::stable_prefix_hash(&perturbed_body).unwrap();

    assert_ne!(
        baseline_hash, perturbed_hash,
        "a mutated stable-prefix block must change the stable-prefix hash"
    );
}

/// A change confined to the DYNAMIC tail (the trailing message after the
/// breakpoint) must NOT change the stable-prefix hash — otherwise the guard
/// would warn on every legitimately-changing turn and become noise. Here the
/// trailing breakpoint marker sits on the last *user* message content; the
/// stable prefix is tools + system blocks, which are unchanged.
#[test]
fn test_stable_prefix_hash_ignores_dynamic_tail_changes() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();
    let body_a = provider.build_request(&conv, &tools, None);

    let mut conv_b = conv.clone();
    // The last message is the user turn; its content is the dynamic tail and is
    // not part of the cached system/tool prefix.
    conv_b.push(crate::message::Message::user(
        "a different follow-up question",
    ));
    let body_b = provider.build_request(&conv_b, &tools, None);

    // The system + tool stable prefix is byte-identical, so those serialized
    // segments must match even though the conversation tail differs.
    assert_eq!(
        serde_json::to_string(&body_a["system"]).unwrap(),
        serde_json::to_string(&body_b["system"]).unwrap(),
        "system prefix must be unaffected by a dynamic-tail change"
    );
    assert_eq!(
        serde_json::to_string(&body_a["tools"]).unwrap(),
        serde_json::to_string(&body_b["tools"]).unwrap(),
        "tool prefix must be unaffected by a dynamic-tail change"
    );
    // And, crucially, the stable-prefix HASH must be identical. The trailing
    // message breakpoint moved to the new last message (different content), but
    // that segment is deliberately excluded from the hash, so the guard must NOT
    // see drift here.
    assert_eq!(
        AnthropicProvider::stable_prefix_hash(&body_a),
        AnthropicProvider::stable_prefix_hash(&body_b),
        "a dynamic-tail (trailing message) change must not move the stable-prefix hash"
    );
}

/// Regression for the B3 drift-guard bug: two requests that share an identical
/// system + tool prefix but differ in their conversation messages — exactly what
/// consecutive turns of a growing conversation look like — must hash to the SAME
/// stable prefix. Before the fix the hash folded the trailing message breakpoint,
/// so this changed every turn and the guard warned on every single request.
#[test]
fn test_stable_prefix_hash_stable_across_growing_conversation() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();

    // Turn 1: the fixture conversation as-is.
    let turn_1 = provider.build_request(&conv, &tools, None);

    // Turn 2: the conversation has grown — append an assistant reply and a new
    // user message, mirroring a real multi-turn session. The trailing breakpoint
    // now sits on a different message, but system + tools are untouched.
    let mut grown = conv.clone();
    grown.push(crate::message::Message::assistant("an assistant reply"));
    grown.push(crate::message::Message::user(
        "a follow-up that grows the convo",
    ));
    let turn_2 = provider.build_request(&grown, &tools, None);

    // Sanity: the message arrays genuinely differ (otherwise the test is vacuous).
    assert_ne!(
        turn_1["messages"], turn_2["messages"],
        "the two turns must have different messages for this regression to be meaningful"
    );

    let hash_1 = AnthropicProvider::stable_prefix_hash(&turn_1)
        .expect("stable tool/system markers present => hash should exist");
    let hash_2 = AnthropicProvider::stable_prefix_hash(&turn_2)
        .expect("stable tool/system markers present => hash should exist");
    assert_eq!(
        hash_1, hash_2,
        "identical system+tools across growing conversation turns must hash identically"
    );
}

/// Companion to the regression above: a mutated system block OR a mutated tool
/// definition must still move the hash, so the guard retains its teeth for the
/// drift it is actually meant to catch.
#[test]
fn test_stable_prefix_hash_detects_system_or_tool_mutation() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();
    let baseline = provider.build_request(&conv, &tools, None);
    let baseline_hash = AnthropicProvider::stable_prefix_hash(&baseline).unwrap();

    // Mutated system block (the cached project-context text drifts).
    let mut conv_sys = conv.clone();
    conv_sys.messages[0].content[1] = ContentBlock::Text {
        text: "project context / repo map (DRIFTED)".to_string(),
    };
    let sys_body = provider.build_request(&conv_sys, &tools, None);
    assert_ne!(
        baseline_hash,
        AnthropicProvider::stable_prefix_hash(&sys_body).unwrap(),
        "a mutated cached system block must change the stable-prefix hash"
    );

    // Mutated tool definition (the cached tool schema/description drifts).
    let mut tools_mut = tools.clone();
    tools_mut[0]["description"] = json!("Run shell (description drifted)");
    let tool_body = provider.build_request(&conv, &tools_mut, None);
    assert_ne!(
        baseline_hash,
        AnthropicProvider::stable_prefix_hash(&tool_body).unwrap(),
        "a mutated cached tool definition must change the stable-prefix hash"
    );
}

/// The hash folds objects in sorted-key order, so it is independent of physical
/// map storage order even if `serde_json`'s `preserve_order` feature is ever
/// enabled. Build the same object with keys inserted in two different orders and
/// assert the fold produces the same hash.
#[test]
fn test_stable_prefix_hash_is_key_order_independent() {
    // Two tool arrays with the same logical content but different key insertion
    // order. With default serde_json (BTreeMap) these already serialize the
    // same, but the explicit sorted-key fold makes the guarantee robust.
    let body_a = json!({
        "model": "claude-3-5-sonnet",
        "tools": [{
            "cache_control": {"type": "ephemeral", "kind": "stable_prefix"},
            "name": "shell",
            "description": "Run shell"
        }]
    });
    let body_b = json!({
        "model": "claude-3-5-sonnet",
        "tools": [{
            "description": "Run shell",
            "name": "shell",
            "cache_control": {"kind": "stable_prefix", "type": "ephemeral"}
        }]
    });
    assert_eq!(
        AnthropicProvider::stable_prefix_hash(&body_a),
        AnthropicProvider::stable_prefix_hash(&body_b),
        "stable-prefix hash must be independent of object key order"
    );
}

/// No cache markers => no prefix to guard => `None` (guard stays silent).
#[test]
fn test_stable_prefix_hash_none_when_no_markers() {
    let body = json!({
        "model": "claude-3-5-sonnet",
        "system": "plain string system, no cache_control",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });
    assert!(
        AnthropicProvider::stable_prefix_hash(&body).is_none(),
        "a request with no cache_control markers has no stable prefix to hash"
    );
}

// ─── Default caching policy (metadata-less agentic conversations) ─────────

/// Worker/task sessions assemble their system prompt as one plain string
/// with no breakpoint metadata. With tools present, the default policy must
/// still cache: marker on the last tool, on the (single) system block, and
/// the trailing message breakpoint — 3 markers, within the cap of 4.
#[test]
fn test_default_cache_policy_marks_tools_system_and_trailing_message() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system("worker system prompt"));
    conv.push(crate::message::Message::user("do the task"));

    let tools = vec![
        json!({"name": "shell", "description": "Run shell", "input_schema": {"type": "object"}}),
        json!({"name": "read", "description": "Read file", "input_schema": {"type": "object"}}),
    ];

    let req = provider.build_request(&conv, &tools, None);

    // Marker on the LAST tool only (breakpoint = end of cacheable prefix).
    assert!(req["tools"][0].get("cache_control").is_none());
    assert_eq!(
        req["tools"][1]["cache_control"],
        json!({"type": "ephemeral"})
    );

    // The single system block is marked, forcing array serialization.
    let system = req["system"].as_array().expect("system array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["text"], "worker system prompt");
    assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));

    // Trailing message breakpoint present.
    let messages = req["messages"].as_array().expect("messages");
    let last_block = messages.last().unwrap()["content"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last_block["cache_control"], json!({"type": "ephemeral"}));

    assert_eq!(count_cache_markers(&req), 3);
}

/// One-shot utility calls (no tools, no metadata — e.g. compaction
/// summaries) must stay unmarked: a cache write that is never read back is
/// pure cost.
#[test]
fn test_default_cache_policy_inactive_without_tools() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system("summarise this"));
    conv.push(crate::message::Message::user("transcript…"));

    let req = provider.build_request(&conv, &[], None);
    assert_eq!(count_cache_markers(&req), 0);
    assert!(req["system"].is_string());
}

/// Explicit breakpoint metadata wins over the default policy: the system
/// split stays all-but-last (dynamic tail uncached).
#[test]
fn test_explicit_metadata_overrides_default_policy() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();
    let req = provider.build_request(&conv, &tools, None);

    let system = req["system"].as_array().expect("system array");
    assert_eq!(system.len(), 3);
    assert!(system[0].get("cache_control").is_some());
    assert!(system[1].get("cache_control").is_some());
    assert!(
        system[2].get("cache_control").is_none(),
        "dynamic tail must stay uncached under the explicit contract"
    );
}

// ─── effective_url: Anthropic-compatible base URLs ────────────────────────

#[test]
fn test_effective_url_joins_native_and_v1_suffixed_bases() {
    let mut config = test_anthropic_config();
    config.base_url = "https://api.anthropic.com".to_string();
    assert_eq!(
        AnthropicProvider::new(config.clone()).effective_url(),
        "https://api.anthropic.com/v1/messages"
    );

    // MiniMax coding plan publishes a base that already ends in /v1.
    config.base_url = "https://api.minimax.io/anthropic/v1".to_string();
    assert_eq!(
        AnthropicProvider::new(config.clone()).effective_url(),
        "https://api.minimax.io/anthropic/v1/messages"
    );

    config.base_url = "https://api.minimax.io/anthropic/v1/".to_string();
    assert_eq!(
        AnthropicProvider::new(config).effective_url(),
        "https://api.minimax.io/anthropic/v1/messages"
    );
}

// ─── RMCP → Anthropic tool-shape conversion ───────────────────────────────

/// djinn's tool registry hands providers RMCP-shaped tools
/// (`{"name","description","inputSchema"}`). The Anthropic wire format
/// requires `input_schema`; the serializer must convert and emit a clean
/// object (no stray `inputSchema` key — strict Anthropic-compatible
/// vendors reject requests whose tools have no `input_schema`).
#[test]
fn test_rmcp_tools_converted_to_anthropic_input_schema() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({
        "name": "epic_list",
        "description": "List epics",
        "inputSchema": {"type": "object", "properties": {"project": {"type": "string"}}}
    })];

    let req = provider.build_request(&conv, &tools, None);
    let tool = &req["tools"][0];
    assert_eq!(tool["name"], "epic_list");
    assert_eq!(tool["description"], "List epics");
    assert_eq!(
        tool["input_schema"]["properties"]["project"]["type"],
        "string"
    );
    assert!(
        tool.get("inputSchema").is_none(),
        "camelCase RMCP key must not leak onto the wire"
    );
}

/// A tool with neither schema key still gets a minimal valid input_schema.
#[test]
fn test_tool_without_schema_gets_default_input_schema() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({"name": "ping", "description": "Ping"})];
    let req = provider.build_request(&conv, &tools, None);
    assert_eq!(req["tools"][0]["input_schema"], json!({"type": "object"}));
}
