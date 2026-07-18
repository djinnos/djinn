//! Behavioral coverage for both production direct-services stream collectors.
use djinn_agent::direct_services::{
    collect_invoke_llm_stream_for_test, collect_planner_stream_for_test,
};
use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::{LlmProvider, StreamEvent, TokenUsage, ToolChoice};
use djinn_supervisor::services::wire::PlannerOutcome;
use serde_json::Value;
use std::pin::Pin;

struct ScriptedProvider {
    events: Vec<Result<StreamEvent, String>>,
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn stream<'a>(
        &'a self,
        _: &'a Conversation,
        _: &'a [Value],
        _: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let events = self
            .events
            .clone()
            .into_iter()
            .map(|event| event.map_err(anyhow::Error::msg));
        Box::pin(async move {
            let stream: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(futures::stream::iter(events));
            Ok(stream)
        })
    }
}

fn conversation() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::user("prompt"));
    conversation
}

fn complete_events() -> Vec<Result<StreamEvent, String>> {
    vec![
        Ok(StreamEvent::ThinkingDelta {
            id: 7,
            text: "x".into(),
        }),
        Ok(StreamEvent::ThinkingBlockComplete {
            id: 7,
            thinking: "A".into(),
            signature: Some("sig".into()),
        }),
        Ok(StreamEvent::Delta(ContentBlock::text("ordinary text"))),
        Ok(StreamEvent::Delta(ContentBlock::ToolUse {
            id: "tool-1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        })),
        Ok(StreamEvent::Usage(TokenUsage {
            input: 3,
            output: 5,
            ..Default::default()
        })),
        Ok(StreamEvent::Done),
    ]
}

fn assert_complete_response(response: &djinn_provider::provider::LlmResponse) {
    assert_eq!(response.thinking, "x");
    assert_eq!(response.usage.input, 3);
    assert_eq!(response.usage.output, 5);
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
    assert!(
        matches!(response.content.get(2), Some(ContentBlock::ToolUse { name, .. }) if name == "read")
    );
}

#[tokio::test]
async fn invoke_llm_collector_aggregates_every_event_kind_and_retry_outcomes() {
    let conversation = conversation();
    let response = collect_invoke_llm_stream_for_test(
        &ScriptedProvider {
            events: complete_events(),
        },
        &conversation,
        &[],
        None,
    )
    .await
    .unwrap();
    assert_complete_response(&response);

    // A failed attempt surfaces its stream error; a subsequent fresh provider
    // stream is the caller-level retry and still executes the same collector.
    assert!(
        collect_invoke_llm_stream_for_test(
            &ScriptedProvider {
                events: vec![Err("transient failure".into())]
            },
            &conversation,
            &[],
            None,
        )
        .await
        .is_err()
    );
    assert_complete_response(
        &collect_invoke_llm_stream_for_test(
            &ScriptedProvider {
                events: complete_events(),
            },
            &conversation,
            &[],
            None,
        )
        .await
        .unwrap(),
    );

    // Direct invocation historically returns its partial aggregate on exhausted
    // streams; execute and pin that production behavior explicitly.
    let partial = collect_invoke_llm_stream_for_test(
        &ScriptedProvider {
            events: vec![Ok(StreamEvent::Delta(ContentBlock::text("partial")))],
        },
        &conversation,
        &[],
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        partial
            .content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<String>(),
        "partial"
    );
}

#[tokio::test]
async fn planner_collector_aggregates_events_and_classifies_error_and_exhaustion() {
    let conversation = conversation();
    let (response, outcome, diagnostic, completed) = collect_planner_stream_for_test(
        &ScriptedProvider {
            events: complete_events(),
        },
        &conversation,
        &[],
        None,
        100,
    )
    .await;
    assert_complete_response(&response);
    assert_eq!(outcome, PlannerOutcome::Success);
    assert!(diagnostic.is_none());
    assert!(completed);

    let (_, outcome, diagnostic, completed) = collect_planner_stream_for_test(
        &ScriptedProvider {
            events: vec![Err("transient failure".into())],
        },
        &conversation,
        &[],
        None,
        100,
    )
    .await;
    assert_eq!(outcome, PlannerOutcome::ProviderError);
    assert!(diagnostic.unwrap().contains("transient failure"));
    assert!(!completed);

    // A fresh collector call models the planner caller retry after an error.
    let (response, outcome, _, completed) = collect_planner_stream_for_test(
        &ScriptedProvider {
            events: complete_events(),
        },
        &conversation,
        &[],
        None,
        100,
    )
    .await;
    assert_complete_response(&response);
    assert_eq!(outcome, PlannerOutcome::Success);
    assert!(completed);

    let (partial, outcome, diagnostic, completed) = collect_planner_stream_for_test(
        &ScriptedProvider {
            events: vec![Ok(StreamEvent::Delta(ContentBlock::text("partial")))],
        },
        &conversation,
        &[],
        None,
        100,
    )
    .await;
    assert_eq!(
        partial
            .content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<String>(),
        "partial"
    );
    assert_eq!(outcome, PlannerOutcome::Success);
    assert!(diagnostic.is_none());
    assert!(completed);
}
