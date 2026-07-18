//! Production text-only completion consumer regression coverage.

use std::pin::Pin;
use std::sync::Mutex;

use djinn_provider::CompletionRequest;
use djinn_provider::complete;
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::{LlmProvider, StreamEvent, TokenUsage, ToolChoice};
use futures::{Stream, stream};
use serde_json::Value;

struct ScriptedProvider(Mutex<Vec<Vec<anyhow::Result<StreamEvent>>>>);

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
                        Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let events = self.0.lock().expect("script lock").remove(0);
        Box::pin(async move {
            let stream: Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>> =
                Box::pin(stream::iter(events));
            Ok(stream)
        })
    }
}

fn request() -> CompletionRequest {
    CompletionRequest {
        system: "system".into(),
        prompt: "prompt".into(),
        max_tokens: 32,
    }
}

#[tokio::test]
async fn text_only_completion_ignores_attributed_thinking_and_other_non_text_events() {
    let provider = ScriptedProvider(Mutex::new(vec![vec![
        Ok(StreamEvent::Thinking("unattributed ".into())),
        Ok(StreamEvent::ThinkingDelta {
            id: 4,
            text: "attributed ".into(),
        }),
        Ok(StreamEvent::Delta(ContentBlock::ToolUse {
            id: "tool-1".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        })),
        Ok(StreamEvent::Delta(ContentBlock::text("visible "))),
        Ok(StreamEvent::ThinkingBlockComplete {
            id: 4,
            thinking: "attributed ".into(),
            signature: Some("sig".into()),
        }),
        Ok(StreamEvent::Usage(TokenUsage {
            input: 13,
            output: 21,
            ..Default::default()
        })),
        Ok(StreamEvent::Done),
        Ok(StreamEvent::Delta(ContentBlock::text("after done"))),
    ]]));

    let response = complete(&provider, request()).await.expect("completion");
    assert_eq!(response.text, "visible ");
    assert_eq!((response.input_tokens, response.output_tokens), (13, 21));
}

#[tokio::test]
async fn text_only_completion_preserves_retry_and_error_behavior() {
    let provider = ScriptedProvider(Mutex::new(vec![
        vec![Err(anyhow::anyhow!("429 rate limit"))],
        vec![
            Ok(StreamEvent::Delta(ContentBlock::text("retry text"))),
            Ok(StreamEvent::Done),
        ],
    ]));
    assert_eq!(
        complete(&provider, request())
            .await
            .expect("one retry")
            .text,
        "retry text"
    );

    let failing = ScriptedProvider(Mutex::new(vec![vec![Err(anyhow::anyhow!(
        "permanent failure"
    ))]]));
    let error = complete(&failing, request())
        .await
        .expect_err("non-retryable error propagates");
    assert!(error.to_string().contains("permanent failure"));
}
