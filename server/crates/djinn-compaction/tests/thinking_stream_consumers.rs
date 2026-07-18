use djinn_compaction::call_llm_for_summary_for_test;
use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
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
            .map(|e| e.map_err(anyhow::Error::msg));
        Box::pin(async move {
            let stream: Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(futures::stream::iter(events));
            Ok(stream)
        })
    }
}
fn conversation() -> Conversation {
    let mut c = Conversation::new();
    c.push(Message::user("tail"));
    c
}
#[tokio::test]
async fn summary_excludes_reasoning() {
    let p = ScriptedProvider {
        events: vec![
            Ok(StreamEvent::ThinkingDelta {
                id: 1,
                text: "x".into(),
            }),
            Ok(StreamEvent::ThinkingBlockComplete {
                id: 1,
                thinking: "A".into(),
                signature: None,
            }),
            Ok(StreamEvent::Delta(ContentBlock::text("answer"))),
            Ok(StreamEvent::Done),
        ],
    };
    assert_eq!(
        call_llm_for_summary_for_test(&p, &conversation())
            .await
            .unwrap(),
        "answer"
    );
}
#[tokio::test]
async fn summary_rejects_error_and_premature_end() {
    for events in [
        vec![Ok(StreamEvent::Delta(ContentBlock::text("partial")))],
        vec![Err("retry failed".into())],
    ] {
        assert!(
            call_llm_for_summary_for_test(&ScriptedProvider { events }, &conversation())
                .await
                .is_err()
        );
    }
}
