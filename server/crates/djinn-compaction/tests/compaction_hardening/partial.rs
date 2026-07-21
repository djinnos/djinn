use std::pin::Pin;
use std::sync::{Arc, Mutex};

use djinn_compaction::{CompactionContext, PARTIAL_COMPACTION_CONTINUATION, compact_conversation};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use serde_json::Value;

#[derive(Default)]
struct SummaryProvider {
    requests: Arc<Mutex<Vec<String>>>,
}

impl LlmProvider for SummaryProvider {
    fn name(&self) -> &str {
        "partial-fixture"
    }

    fn stream<'a>(
        &'a self,
        conversation: &'a Conversation,
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
        self.requests.lock().unwrap().push(
            conversation
                .messages
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        Box::pin(async {
            Ok(Box::pin(futures::stream::iter([
                Ok(StreamEvent::Delta(ContentBlock::text("fixture summary"))),
                Ok(StreamEvent::Done),
            ]))
                as Pin<
                    Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                >)
        })
    }
}

fn text(role: Role, value: &str) -> Message {
    Message {
        role,
        content: vec![ContentBlock::text(value)],
        metadata: None,
    }
}

fn tool_use(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: "read".into(),
            input: serde_json::json!({"path": id}),
        }],
        metadata: None,
    }
}

fn tool_result(id: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: vec![ContentBlock::text("fixture output")],
            is_error: false,
        }],
        metadata: None,
    }
}

async fn compact(messages: Vec<Message>) -> (Vec<Message>, Vec<String>) {
    let provider = SummaryProvider::default();
    let mut conversation = Conversation { messages };
    assert!(
        compact_conversation(
            &provider,
            &mut conversation,
            "fixture-session",
            "fixture-task",
            CompactionContext::MidSession("worker".into()),
            100,
        )
        .await
    );
    let requests = provider.requests.lock().unwrap().clone();
    (conversation.messages, requests)
}

fn continuation_index(messages: &[Message]) -> usize {
    messages
        .iter()
        .position(|message| message.text_content() == PARTIAL_COMPACTION_CONTINUATION)
        .expect("partial, not full, compaction")
}

fn assert_no_input_message_is_duplicated(input: &[Message], output: &[Message]) {
    for message in input {
        assert!(
            output
                .iter()
                .filter(|candidate| *candidate == message)
                .count()
                <= 1,
            "input message was duplicated: {message:?}"
        );
    }
}

#[tokio::test]
async fn preserves_two_closed_turns() {
    let wide = "x".repeat(100);
    let normal = vec![
        Message::system(wide.clone()),
        Message::user(format!("first {wide}")),
        Message::assistant(format!("first answer {wide}")),
        Message::user(format!("middle {wide}")),
        Message::assistant(format!("middle answer {wide}")),
        Message::user("latest question"),
        Message::assistant("latest answer"),
    ];
    let normal_prefix = normal[..3].to_vec();
    let normal_tail = normal[5..].to_vec();
    let (compacted, _) = compact(normal.clone()).await;
    assert_no_input_message_is_duplicated(&normal, &compacted);
    let marker = continuation_index(&compacted);
    assert_eq!(&compacted[..3], normal_prefix.as_slice());
    assert_eq!(&compacted[marker + 1..], normal_tail.as_slice());

    // When the desired tail reaches the pivot, retain the largest closed tail
    // rather than duplicating the prefix or dropping the available turns.
    let short = vec![
        Message::system(wide.clone()),
        Message::user(format!("short question {wide}")),
        Message::assistant(format!("short answer {wide}")),
        Message::user("latest short question"),
    ];
    let short_prefix = short[..1].to_vec();
    let short_tail = short[2..].to_vec();
    let (compacted, _) = compact(short.clone()).await;
    assert_no_input_message_is_duplicated(&short, &compacted);
    let marker = continuation_index(&compacted);
    assert_eq!(&compacted[..1], short_prefix.as_slice());
    assert_eq!(&compacted[marker + 1..], short_tail.as_slice());

    // A trailing unanswered tool call is summarized, never retained; the two
    // preceding closed turns remain adjacent and byte-for-byte equal.
    let unresolved = vec![
        Message::system(wide.clone()),
        text(Role::User, &format!("old {wide}")),
        text(Role::Assistant, &format!("old answer {wide}")),
        tool_use("resolved"),
        tool_result("resolved"),
        text(Role::Assistant, "resolved follow-up"),
        tool_use("unanswered"),
    ];
    let closed_tail = unresolved[3..6].to_vec();
    let (compacted, summary_requests) = compact(unresolved.clone()).await;
    assert_no_input_message_is_duplicated(&unresolved, &compacted);
    let marker = continuation_index(&compacted);
    assert_eq!(&compacted[marker + 1..], closed_tail.as_slice());
    assert!(!compacted.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { id, .. } if id == "unanswered"))
    }));
    assert!(
        summary_requests
            .iter()
            .any(|request| request.contains("\"path\":\"unanswered\"")),
        "the trailing unanswered tool call must be included in the summary request"
    );
}
