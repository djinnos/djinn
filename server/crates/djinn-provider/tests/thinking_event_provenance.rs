//! Provider-owned wire regression for Anthropic indexed thinking events.
//!
//! This uses the public Anthropic provider stream rather than duplicating its
//! parser. The fixture deliberately interleaves blocks: provider content indices,
//! not arrival order, are the durable provenance key.

use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::format::anthropic::AnthropicProvider;
use djinn_provider::provider::{
    AuthMethod, FormatFamily, LlmProvider, ProviderCapabilities, ProviderConfig, StreamEvent,
};
use futures::TryStreamExt;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(base_url: &str) -> ProviderConfig {
    ProviderConfig {
        base_url: base_url.into(),
        auth: AuthMethod::NoAuth,
        format_family: FormatFamily::Anthropic,
        model_id: "claude-test".into(),
        context_window: 200_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: ProviderCapabilities::default(),
        reasoning_effort: None,
        tool_schema_compat: None,
    }
}

fn frame(value: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::to_string(&value).expect("SSE JSON")
    )
}

#[tokio::test]
async fn anthropic_thinking_events_keep_content_index_provenance() {
    let body = [
        frame(json!({"type":"message_start","message":{"usage":{"input_tokens":11}}})),
        // Front-loaded ordinary text remains an ordinary text delta.
        frame(json!({"type":"content_block_start","index":4,"content_block":{"type":"text","text":"front "}})),
        // Anthropic can put the first thinking bytes on the start frame. They
        // belong to index 7's eventual completion, while subsequent deltas
        // retain that same index as their provenance.
        frame(json!({"type":"content_block_start","index":7,"content_block":{"type":"thinking","thinking":"head "}})),
        frame(json!({"type":"content_block_start","index":2,"content_block":{"type":"thinking"}})),
        frame(json!({"type":"content_block_start","index":9,"content_block":{"type":"tool_use","id":"tool-9","name":"shell"}})),
        frame(json!({"type":"content_block_delta","index":7,"delta":{"type":"thinking_delta","thinking":"seven-a "}})),
        frame(json!({"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"two-a "}})),
        frame(json!({"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"text"}})),
        frame(json!({"type":"content_block_delta","index":9,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"pwd\"}"}})),
        frame(json!({"type":"content_block_delta","index":7,"delta":{"type":"thinking_delta","thinking":"seven-b"}})),
        frame(json!({"type":"content_block_delta","index":7,"delta":{"type":"signature_delta","signature":"sig-7"}})),
        frame(json!({"type":"content_block_stop","index":2})),
        frame(json!({"type":"content_block_stop","index":7})),
        // A repeated stop cannot manufacture a second completion.
        frame(json!({"type":"content_block_stop","index":7})),
        frame(json!({"type":"content_block_stop","index":9})),
        frame(json!({"type":"message_delta","usage":{"output_tokens":23}})),
        frame(json!({"type":"message_stop"})),
    ]
    .join("");

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new(config(&server.uri()));
    let mut conversation = Conversation::new();
    conversation.push(Message::user("hello"));
    let events = provider
        .stream(&conversation, &[], None)
        .await
        .expect("Anthropic stream")
        .try_collect::<Vec<_>>()
        .await
        .expect("complete Anthropic stream");

    assert!(
        matches!(&events[0], StreamEvent::Delta(ContentBlock::Text { text }) if text == "front ")
    );
    assert!(matches!(&events[1], StreamEvent::ThinkingDelta { id: 7, text } if text == "seven-a "));
    assert!(matches!(&events[2], StreamEvent::ThinkingDelta { id: 2, text } if text == "two-a "));
    assert!(
        matches!(&events[3], StreamEvent::Delta(ContentBlock::Text { text }) if text == "text")
    );
    assert!(matches!(&events[4], StreamEvent::ThinkingDelta { id: 7, text } if text == "seven-b"));
    assert!(
        matches!(&events[5], StreamEvent::ThinkingBlockComplete { id: 2, thinking, signature: None } if thinking == "two-a ")
    );
    assert!(
        matches!(&events[6], StreamEvent::ThinkingBlockComplete { id: 7, thinking, signature: Some(signature) } if thinking == "head seven-a seven-b" && signature == "sig-7")
    );
    assert!(
        matches!(&events[7], StreamEvent::Delta(ContentBlock::ToolUse { id, name, input }) if id == "tool-9" && name == "shell" && input["cmd"] == "pwd")
    );
    assert!(
        matches!(&events[8], StreamEvent::Usage(usage) if usage.input == 11 && usage.output == 23)
    );
    assert!(matches!(&events[9], StreamEvent::Done));
    assert_eq!(
        events.len(),
        10,
        "each stopped thinking block completes exactly once"
    );
}
