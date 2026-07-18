use djinn_compaction::{call_llm_for_summary_for_test, do_partial_compact_for_test};
use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::{LlmProvider, StreamEvent, TokenUsage, ToolChoice};
use serde_json::Value;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Mutex;

struct ScriptedProvider {
    attempts: Mutex<VecDeque<Vec<Result<StreamEvent, String>>>>,
    calls: Mutex<usize>,
}

impl ScriptedProvider {
    fn one(events: Vec<Result<StreamEvent, String>>) -> Self {
        Self::attempts(vec![events])
    }

    fn attempts(attempts: Vec<Vec<Result<StreamEvent, String>>>) -> Self {
        Self {
            attempts: Mutex::new(attempts.into()),
            calls: Mutex::new(0),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
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
        *self.calls.lock().unwrap() += 1;
        let events = self
            .attempts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
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
    conversation.push(Message::user("tail"));
    conversation
}

#[tokio::test]
async fn summary_excludes_reasoning_and_ignores_usage() {
    let provider = ScriptedProvider::one(vec![
        Ok(StreamEvent::ThinkingDelta {
            id: 1,
            text: "x".into(),
        }),
        Ok(StreamEvent::ThinkingBlockComplete {
            id: 1,
            thinking: "A".into(),
            signature: None,
        }),
        Ok(StreamEvent::Usage(TokenUsage {
            input: 3,
            output: 5,
            ..Default::default()
        })),
        Ok(StreamEvent::Delta(ContentBlock::text("answer"))),
        Ok(StreamEvent::Done),
    ]);

    assert_eq!(
        call_llm_for_summary_for_test(&provider, &conversation())
            .await
            .unwrap(),
        "answer"
    );
}

#[tokio::test]
async fn summary_rejects_error_and_premature_end() {
    for events in [
        vec![Ok(StreamEvent::Delta(ContentBlock::text("partial")))],
        vec![Err("provider failed".into())],
    ] {
        assert!(
            call_llm_for_summary_for_test(&ScriptedProvider::one(events), &conversation())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn partial_compaction_retries_context_error_through_summary_consumer() {
    let provider = ScriptedProvider::attempts(vec![
        vec![Err("context limit exceeded".into())],
        vec![
            Ok(StreamEvent::ThinkingDelta {
                id: 2,
                text: "x".into(),
            }),
            Ok(StreamEvent::ThinkingBlockComplete {
                id: 2,
                thinking: "A".into(),
                signature: None,
            }),
            Ok(StreamEvent::Usage(TokenUsage {
                output: 7,
                ..Default::default()
            })),
            Ok(StreamEvent::Delta(ContentBlock::text("answer"))),
            Ok(StreamEvent::Done),
        ],
    ]);

    assert_eq!(
        do_partial_compact_for_test(&provider, &[Message::user("tail")])
            .await
            .unwrap(),
        "answer"
    );
    assert_eq!(provider.calls(), 2, "context error must trigger one retry");
}
