//! SSE/event parser + tokio stream integration tests for the Anthropic
//! provider. Moved from `tests.rs` to `tests/streaming.rs` in wave 2 of
//! epic `456f` to bring the test module under the 50 KB / 1,500-line size
//! guard. The split boundary is the `// ─── Empty-segment handling tests`
//! section comment that originally sat at L437 of `tests.rs`: everything
//! above that comment is grouped here, everything from it onward moves to
//! `request.rs`.
//!
//! Tests covered (13 total, byte-for-byte identical to the originals):
//! - 6 message-level parser tests (`test_message_start_extracts_input_tokens`,
//!   `test_message_start_extracts_cache_tokens`, `test_text_delta_event`,
//!   `test_tool_use_accumulation`, `test_message_delta_emits_usage`,
//!   `test_message_stop_emits_done`)
//! - 3 `test_build_request_*` / `test_system_blocks_*` tests that lived
//!   between the parser tests and the Empty-segment section break
//!   (`test_build_request_always_populates_system_field`,
//!   `test_system_blocks_consume_explicit_stable_prefix_metadata_contract`,
//!   `test_build_request_preserves_separate_system_blocks_with_cache_control`)
//! - 1 `content_block_delta` parser test
//!   (`test_content_block_delta_input_json_without_active_tool_is_ignored`)
//! - 2 tokio stream integration tests
//!   (`test_stream_uses_payload_type_over_sse_event_name`,
//!   `test_streamed_error_event_is_ignored_but_http_error_shape_surfaces`)
//! - 1 `test_build_request_sets_required_tool_choice_when_tools_present`
//!   that closes out the streaming-SSE concern just before the L437 break.

use super::*;
use super::{spawn_sse_server, test_anthropic_config, test_provider};
use crate::message::{Conversation, Message};
use futures::TryStreamExt;

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
