//! Behavioral coverage for the production direct-services response aggregator.
use djinn_agent::direct_services::append_direct_response_event;
use djinn_provider::message::ContentBlock;
use djinn_provider::provider::{LlmResponse, StreamEvent, TokenUsage};
#[test]
fn direct_services_aggregates_attributed_thinking_once_and_keeps_completion_content() {
    let mut response = LlmResponse::default();
    let usage = TokenUsage {
        input: 3,
        output: 5,
        ..Default::default()
    };
    for event in [
        StreamEvent::ThinkingDelta {
            id: 7,
            text: "x".into(),
        },
        StreamEvent::ThinkingBlockComplete {
            id: 7,
            thinking: "A".into(),
            signature: Some("sig".into()),
        },
        StreamEvent::Delta(ContentBlock::text("ordinary text")),
        StreamEvent::Usage(usage.clone()),
        StreamEvent::Done,
        StreamEvent::ThinkingDelta {
            id: 8,
            text: "retry".into(),
        },
    ] {
        if append_direct_response_event(&mut response, event) {
            break;
        }
    }
    assert_eq!(response.thinking, "x");
    assert_eq!(response.usage.input, usage.input);
    assert_eq!(
        response
            .content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<String>(),
        "ordinary text"
    );
    assert!(
        matches!(response.content.first(), Some(ContentBlock::Thinking { thinking, signature: Some(signature) }) if thinking == "A" && signature == "sig")
    );
}
#[test]
fn direct_services_retains_ordinary_usage_done_and_unattributed_thinking() {
    let mut response = LlmResponse::default();
    assert!(!append_direct_response_event(
        &mut response,
        StreamEvent::Thinking("legacy ".into())
    ));
    assert!(!append_direct_response_event(
        &mut response,
        StreamEvent::ThinkingDelta {
            id: 9,
            text: "x".into()
        }
    ));
    assert!(!append_direct_response_event(
        &mut response,
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "tool-1".into(),
            name: "read".into(),
            input: serde_json::json!({})
        })
    ));
    assert!(!append_direct_response_event(
        &mut response,
        StreamEvent::Usage(TokenUsage {
            output: 11,
            ..Default::default()
        })
    ));
    assert!(append_direct_response_event(
        &mut response,
        StreamEvent::Done
    ));
    assert_eq!(response.thinking, "legacy x");
    assert_eq!(response.usage.output, 11);
}
