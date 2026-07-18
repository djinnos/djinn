//! Anthropic replay round-trip and tool-continuation regressions.
//!
//! These fixtures span the streaming parser, shared `ContentBlock` serde, and the
//! Anthropic request serializer. They verify that signed/redacted thinking and
//! opaque unknown blocks survive a full `parse → persist → serialize` cycle, keep
//! their original ordering relative to `tool_use`, and are replayed in the
//! assistant message before the required `tool_result` continuation.

use super::{ContentBlockAcc, parse_anthropic_event, test_provider};
use crate::message::{ContentBlock, Conversation, Message, Role};
use crate::provider::StreamEvent;
use serde_json::json;

/// Parse a sequence of Anthropic SSE events and return the completed content
/// blocks in the order they are emitted. Ordinary blocks arrive as
/// `StreamEvent::Delta(ContentBlock)`, while completed thinking is carried only
/// by `ThinkingBlockComplete`; this mirrors how production consumers
/// materialize the load-bearing completion payload before persistence.
fn parse_event_sequence(events: &[(&str, &str)]) -> Vec<ContentBlock> {
    let mut acc = ContentBlockAcc::default();
    let mut input_tokens = 0u32;
    let mut cache_read = 0u32;
    let mut cache_write = 0u32;
    let mut blocks = Vec::new();
    for (event_type, data) in events {
        for event in parse_anthropic_event(
            event_type,
            data,
            &mut acc,
            &mut input_tokens,
            &mut cache_read,
            &mut cache_write,
        ) {
            match event {
                StreamEvent::Delta(block) => blocks.push(block),
                StreamEvent::ThinkingBlockComplete {
                    thinking,
                    signature,
                    ..
                } => blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature,
                }),
                _ => {}
            }
        }
    }
    blocks
}

/// Persist-compatible serde round-trip for a single shared block.
fn round_trip(block: &ContentBlock) -> ContentBlock {
    let serialized = serde_json::to_value(block).unwrap();
    serde_json::from_value(serialized).unwrap()
}

/// Persist-compatible serde round-trip for a slice of shared blocks.
fn round_trip_blocks(blocks: &[ContentBlock]) -> Vec<ContentBlock> {
    blocks.iter().map(round_trip).collect()
}

/// Fails if any assistant content block was emitted as the old empty-text
/// placeholder. This is a direct regression guard against the previous
/// fallback that serialized Thinking as `{"type":"text","text":""}`.
fn assert_no_empty_text_placeholder(content: &[serde_json::Value]) {
    assert!(
        !content
            .iter()
            .any(|b| b["type"] == "text" && b["text"] == ""),
        "assistant content must not contain empty-text placeholders: {content:?}"
    );
}

#[test]
fn signed_thinking_then_tool_use_replays_before_tool_result() {
    // Realistic Anthropic SSE sequence: a signed thinking block at index 0
    // followed immediately by a tool_use block at index 1. The deltas must be
    // accumulated by index and completed at the matching content_block_stop.
    let blocks = parse_event_sequence(&[
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"internal reasoning"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig_abc"}}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool_1","name":"shell"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
    ]);
    assert_eq!(blocks.len(), 2);

    // Simulate the persist round-trip before replay.
    let round_tripped = round_trip_blocks(&blocks);

    let mut conv = Conversation::default();
    conv.push(Message {
        role: Role::Assistant,
        content: round_tripped,
        metadata: None,
    });
    // The next user turn must contain the tool_result matching the tool_use id.
    conv.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tool_1".into(),
            content: vec![ContentBlock::text("/workspace")],
            is_error: false,
        }],
        metadata: None,
    });

    let provider = test_provider();
    let req = provider.build_request(&conv, &[], None);
    let messages = req["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);

    // Assistant message: thinking block must appear before tool_use, in the
    // original order required by Anthropic for the continuation turn.
    let assistant = &messages[0];
    assert_eq!(assistant["role"], "assistant");
    let assistant_content = assistant["content"].as_array().unwrap();
    assert_eq!(assistant_content.len(), 2);
    assert_eq!(
        assistant_content[0],
        json!({"type": "thinking", "thinking": "internal reasoning", "signature": "sig_abc"})
    );
    assert_eq!(assistant_content[1]["type"], "tool_use");
    assert_eq!(assistant_content[1]["id"], "tool_1");
    assert_eq!(assistant_content[1]["name"], "shell");
    assert_eq!(assistant_content[1]["input"], json!({"cmd": "pwd"}));
    assert_no_empty_text_placeholder(assistant_content);

    // User message: the tool_result continuation shape must reference the
    // prior assistant tool_use id and carry its content.
    let user = &messages[1];
    assert_eq!(user["role"], "user");
    let user_content = user["content"].as_array().unwrap();
    assert_eq!(user_content.len(), 1);
    assert_eq!(user_content[0]["type"], "tool_result");
    assert_eq!(user_content[0]["tool_use_id"], "tool_1");
    assert_eq!(
        user_content[0]["content"],
        json!([{"type": "text", "text": "/workspace"}])
    );

    assert_eq!(user_content[0]["is_error"], false);
}

#[test]
fn redacted_thinking_and_unknown_passthrough_round_trip() {
    let blocks = parse_event_sequence(&[
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque_blob"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"vendor_block","vendor_id":"v1"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"vendor_delta","cursor":"next"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        ),
    ]);
    assert_eq!(blocks.len(), 2);

    let round_tripped = round_trip_blocks(&blocks);

    let mut conv = Conversation::default();
    conv.push(Message {
        role: Role::Assistant,
        content: round_tripped,
        metadata: None,
    });

    let provider = test_provider();
    let req = provider.build_request(&conv, &[], None);
    let content = req["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);

    // Redacted thinking survives with its opaque data.
    assert_eq!(
        content[0],
        json!({"type": "redacted_thinking", "data": "opaque_blob"})
    );

    // Unknown block: both the start fields and the unrecognized delta fields
    // survive, and the typed `content_type` becomes the wire `type`.
    assert_eq!(content[1]["type"], "vendor_block");
    assert_eq!(content[1]["vendor_id"], "v1");
    assert_eq!(content[1]["cursor"], "next");
    assert_no_empty_text_placeholder(content);
}

#[test]
fn typed_fields_take_precedence_over_conflicting_passthrough_fields() {
    // Start from a parsed Unknown block so the fixture exercises the full
    // parse → shared serde → request serialization pipeline. The typed
    // `content_type` must become the wire `type` even if a passthrough `type`
    // is injected into the extra map after persistence.
    let blocks = parse_event_sequence(&[
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"vendor_block","vendor_id":"v1"}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"vendor_delta","cursor":"next"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
    ]);
    assert_eq!(blocks.len(), 1);

    let mut round_tripped = round_trip(&blocks[0]);

    // Inject a conflicting passthrough `type` after the serde round-trip. The
    // serializer must still use the typed `content_type` for the wire `type`.
    if let ContentBlock::Unknown { extra, .. } = &mut round_tripped {
        extra.insert("type".into(), json!("attempted_override"));
    } else {
        panic!("expected Unknown block");
    }

    let mut conv = Conversation::default();
    conv.push(Message {
        role: Role::Assistant,
        content: vec![round_tripped],
        metadata: None,
    });

    let provider = test_provider();
    let req = provider.build_request(&conv, &[], None);
    let content = req["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "vendor_block");
    assert_eq!(content[0]["vendor_id"], "v1");
    assert_eq!(content[0]["cursor"], "next");
    assert_no_empty_text_placeholder(content);
}

#[test]
fn replay_regression_empty_text_fallback_for_thinking_is_absent() {
    // Direct regression: if any path serializes a Thinking block as the old
    // empty-text placeholder, the string representation of the assistant
    // content array must contain `{"type":"text","text":""}` and the test
    // must fail. The current implementation skips unsigned thinking, emits
    // native `thinking` for signed thinking, and never falls back to empty text.
    let mut conv = Conversation::default();
    conv.push(Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "signed reasoning".into(),
                signature: Some("sig_123".into()),
            },
            ContentBlock::Thinking {
                thinking: "unsigned reasoning".into(),
                signature: None,
            },
            ContentBlock::Thinking {
                thinking: "empty signature reasoning".into(),
                signature: Some("".into()),
            },
            ContentBlock::text("visible output"),
        ],
        metadata: None,
    });

    let provider = test_provider();
    let req = provider.build_request(&conv, &[], None);
    let content = req["messages"][0]["content"].as_array().unwrap();

    // Only signed thinking and visible text are emitted; unsigned/empty-signature
    // thinking is omitted rather than replaced by an empty text block.
    assert_eq!(content.len(), 2);
    assert_eq!(
        content[0],
        json!({"type": "thinking", "thinking": "signed reasoning", "signature": "sig_123"})
    );
    assert_eq!(
        content[1],
        json!({"type": "text", "text": "visible output"})
    );
    assert_no_empty_text_placeholder(content);

    // String-level guard: fail if the fallback representation is present.
    let serialized = serde_json::to_string(&content).unwrap();
    assert!(
        !serialized.contains(r#""type":"text","text":"""#),
        "serialized assistant content must not contain the empty-text fallback: {serialized}"
    );
}
