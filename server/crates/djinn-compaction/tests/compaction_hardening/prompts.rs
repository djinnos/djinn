//! Compaction prompt hardening: output-lookup advisory and pointer-placeholder
//! regression targets.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use djinn_compaction::{
    CompactionContext, OUTPUT_LOOKUP_ADVISORY, ToolOutputPointer,
    compact_conversation_with_pointers,
};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use serde_json::Value;

/// Fake provider that returns a fixed summary and records the request text so
/// tests can assert on the advisory and placeholder content carried into the
/// summariser prompt.
#[derive(Default)]
struct SummaryProvider {
    requests: Arc<Mutex<Vec<String>>>,
}

impl LlmProvider for SummaryProvider {
    fn name(&self) -> &str {
        "prompts-fixture"
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

/// Provider fixture that forces every summarisation attempt to report a context
/// limit, driving the outer overflow-retry and aggressive-microcompaction path.
struct ContextLimitProvider;

impl LlmProvider for ContextLimitProvider {
    fn name(&self) -> &str {
        "context-limit-fixture"
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
        Box::pin(async { Err(anyhow::anyhow!("context limit exceeded")) })
    }
}

/// Build a conversation with `num_turns` resolved tool-call turns, each
/// consisting of an assistant `ToolUse`, a user `ToolResult`, and an assistant
/// follow-up. This matches the shape microcompaction expects so that
/// middle-aged results are cleared while recent edges are preserved.
fn build_tool_conversation(num_turns: usize) -> Vec<Message> {
    let mut messages = vec![
        Message::system("You are a coding agent."),
        Message::user("Do the task."),
    ];

    for i in 0..num_turns {
        let call_id = format!("call_{i}");
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: call_id.clone(),
                name: "bash".into(),
                input: serde_json::json!({"command": format!("echo turn {i}")}),
            }],
            metadata: None,
        });
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call_id,
                content: vec![ContentBlock::text(format!(
                    "Verbose result from turn {i}: {}",
                    "x".repeat(200)
                ))],
                is_error: false,
            }],
            metadata: None,
        });
        messages.push(Message::assistant(format!("Processed turn {i}.")));
    }

    messages
}

/// Compaction summary instructions must contain the advisory lookup hint naming
/// `output_list` (authoritative discovery) and `output_view` (retrieval). This
/// is the stable test target required by acceptance criterion 3.
#[test]
fn summary_contains_output_lookup_hint() {
    // The advisory constant itself names both output_list and output_view.
    assert!(
        OUTPUT_LOOKUP_ADVISORY
            .to_lowercase()
            .contains("output_list"),
        "advisory must name output_list: {OUTPUT_LOOKUP_ADVISORY}"
    );
    assert!(
        OUTPUT_LOOKUP_ADVISORY
            .to_lowercase()
            .contains("output_view"),
        "advisory must name output_view: {OUTPUT_LOOKUP_ADVISORY}"
    );
    assert!(
        OUTPUT_LOOKUP_ADVISORY.contains("tool_use_id"),
        "advisory must mention tool_use_id: {OUTPUT_LOOKUP_ADVISORY}"
    );

    // The advisory is folded into TEMPLATE_RULES, which the summariser
    // substitutes into the {rules} slot of every compaction prompt. Verify the
    // rendered summariser prompt actually carries the hint by running a
    // compaction through the fixture provider and inspecting the request.
    let provider = SummaryProvider::default();
    let mut conversation = Conversation {
        messages: vec![
            Message::system("sys"),
            Message::user("first question"),
            Message::assistant("first answer"),
            Message::user("latest question"),
        ],
    };
    let _ = futures::executor::block_on(compact_conversation_with_pointers(
        &provider,
        &mut conversation,
        "fixture-session",
        "fixture-task",
        CompactionContext::MidSession("worker".into()),
        100,
        &[],
    ));

    let requests = provider.requests.lock().unwrap();
    let combined: String = requests.join("\n");
    assert!(
        combined.to_lowercase().contains("output_list"),
        "summariser prompt must carry the output_list advisory: {combined}"
    );
    assert!(
        combined.to_lowercase().contains("output_view"),
        "summariser prompt must carry the output_view advisory: {combined}"
    );
    assert!(
        combined.contains("tool_use_id"),
        "summariser prompt must carry the tool_use_id hint: {combined}"
    );
}

/// A microcompaction placeholder produced from supplied pointer metadata must
/// state turn, original character count, output kind, `tool_use_id`, and an
/// actionable `output_view(tool_use_id=...)` hint. Message ordering and
/// tool-pair validity must be preserved.
///
/// Uses a large `context_window` so microcompaction alone reclaims enough
/// tokens and the LLM summarisation path does not replace the placeholders.
#[tokio::test]
async fn pointer_placeholder_includes_metadata_and_hint() {
    let provider = SummaryProvider::default();
    // 12 turns gives a range of middle-aged results that microcompaction will
    // clear (turn_map in [exempt_recent=3, effective_current-6]).
    let conversation_messages = build_tool_conversation(12);
    let mut conversation = Conversation {
        messages: conversation_messages.clone(),
    };

    // Provide pointers for all tool results; microcompaction will use them for
    // the ones it clears.
    let pointers: Vec<ToolOutputPointer> = (0..12)
        .map(|i| ToolOutputPointer {
            tool_use_id: format!("call_{i}"),
            turn: i as u64,
            original_chars: 222, // "Verbose result from turn {i}: " + 200 x's
            result_kind: "tool_result".into(),
        })
        .collect();

    // Large context window so microcompaction suffices and LLM summarisation
    // does not replace the placeholders.
    let compacted = compact_conversation_with_pointers(
        &provider,
        &mut conversation,
        "fixture-session",
        "fixture-task",
        CompactionContext::MidSession("worker".into()),
        1_000_000,
        &pointers,
    )
    .await;

    assert!(compacted, "compaction should have run");
    let messages = &conversation.messages;

    // The cleared tool results must now carry pointer placeholders with the
    // actionable output_view hint and the supplied metadata. Placeholders live
    // inside the ToolResult block's content (replaced in-place by
    // microcompaction), so we extract text from ToolResult content blocks.
    let pointer_placeholders: Vec<&str> = messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => content.first().and_then(|c| c.as_text()),
            _ => None,
        })
        .filter(|t| t.starts_with("[Cleared") && t.contains("output_view"))
        .collect();

    assert!(
        !pointer_placeholders.is_empty(),
        "at least one cleared tool result must have a pointer placeholder"
    );

    for ph in &pointer_placeholders {
        assert!(
            ph.contains("tool_result"),
            "placeholder must state output kind: {ph}"
        );
        assert!(
            ph.contains("222 chars"),
            "placeholder must state original character count: {ph}"
        );
        assert!(
            ph.contains("output_view(tool_use_id=\""),
            "placeholder must have an actionable output_view hint: {ph}"
        );
    }

    // Verify a specific pointer's metadata appears in a placeholder.
    let first_with_id = pointer_placeholders
        .iter()
        .find(|p| p.contains("output_view(tool_use_id=\""));
    assert!(
        first_with_id.is_some(),
        "at least one placeholder must carry a concrete tool_use_id"
    );
    // The tool_use_id in the hint must match a real call_id from the conversation.
    let hint_id = first_with_id
        .unwrap()
        .split("tool_use_id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("extractable tool_use_id from hint");
    assert!(
        (0..12).any(|i| format!("call_{i}") == hint_id),
        "hint tool_use_id must match a real call_id: {hint_id}"
    );

    // Tool-pair validity: no orphaned tool results after compaction.
    assert!(find_orphaned_tool_result(messages).is_none());

    // The most recent exempt tool result must NOT be cleared — it survives
    // inline with its original content.
    let recent_survives = messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(
                b,
                ContentBlock::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "call_11" && content.iter().any(|c| {
                    c.as_text().map(|t| t.contains("Verbose result from turn 11")).unwrap_or(false)
                })
            )
        })
    });
    assert!(
        recent_survives,
        "most recent exempt tool result (call_11) must survive microcompaction unchanged"
    );
}

/// When every summary request overflows, the aggressive microcompaction fallback
/// must still use caller-supplied pointer metadata for results that the initial
/// pass preserved due to its recent-turn exemption.
#[tokio::test]
async fn overflow_retry_aggressive_microcompaction_keeps_pointer_metadata() {
    let provider = ContextLimitProvider;
    let mut conversation = Conversation {
        // The initial pass exempts call_3 as a recent result. The aggressive
        // fallback removes that exemption and must replace it with a pointer.
        messages: build_tool_conversation(4),
    };
    let pointers = [ToolOutputPointer {
        tool_use_id: "call_3".into(),
        turn: 42,
        original_chars: 222,
        result_kind: "tool_result".into(),
    }];

    let compacted = compact_conversation_with_pointers(
        &provider,
        &mut conversation,
        "fixture-session",
        "fixture-task",
        CompactionContext::MidSession("worker".into()),
        // Keep deterministic fallback from removing the in-place placeholder.
        1_000_000,
        &pointers,
    )
    .await;

    assert!(
        !compacted,
        "the all-overflow fixture should leave the original conversation in place"
    );
    let placeholder = conversation
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == "call_3" => content.first().and_then(|item| item.as_text()),
            _ => None,
        })
        .expect("call_3 tool result remains paired and in place");

    assert!(
        placeholder.starts_with("[Cleared"),
        "placeholder: {placeholder}"
    );
    assert!(
        placeholder.contains("turn 42"),
        "placeholder: {placeholder}"
    );
    assert!(
        placeholder.contains("222 chars"),
        "placeholder: {placeholder}"
    );
    assert!(
        placeholder.contains("tool_result"),
        "placeholder: {placeholder}"
    );
    assert!(
        placeholder.contains("tool_use_id=\"call_3\""),
        "placeholder: {placeholder}"
    );
    assert!(
        placeholder.contains("output_view(tool_use_id=\"call_3\")"),
        "placeholder: {placeholder}"
    );
    assert!(find_orphaned_tool_result(&conversation.messages).is_none());
}

/// When no pointer metadata is supplied, microcompaction falls back to the
/// legacy `[Cleared — tool result from turn N]` placeholder — the non-tool
/// transcript behavior and retention semantics are unchanged.
#[tokio::test]
async fn no_pointers_uses_legacy_placeholder() {
    let provider = SummaryProvider::default();
    let mut conversation = Conversation {
        messages: build_tool_conversation(12),
    };

    // Large context window so microcompaction suffices and LLM summarisation
    // does not replace the placeholders.
    let compacted = compact_conversation_with_pointers(
        &provider,
        &mut conversation,
        "fixture-session",
        "fixture-task",
        CompactionContext::MidSession("worker".into()),
        1_000_000,
        &[],
    )
    .await;

    assert!(compacted);

    let legacy_placeholders: Vec<&str> = conversation
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => content.first().and_then(|c| c.as_text()),
            _ => None,
        })
        .filter(|t| t.starts_with("[Cleared — tool result from turn"))
        .collect();

    assert!(
        !legacy_placeholders.is_empty(),
        "legacy placeholders must be produced when no pointers are supplied"
    );

    // None of the placeholders should carry the output_view hint (no pointers).
    assert!(
        legacy_placeholders
            .iter()
            .all(|p| !p.contains("output_view")),
        "legacy placeholders must not have output_view hint: {legacy_placeholders:?}"
    );

    // Tool-pair validity preserved.
    assert!(find_orphaned_tool_result(&conversation.messages).is_none());
}

/// A pointer whose `tool_use_id` does not match any conversation tool result
/// is silently ignored — it cannot introduce a placeholder where none belongs.
#[tokio::test]
async fn unmatched_pointer_is_ignored() {
    let provider = SummaryProvider::default();
    let mut conversation = Conversation {
        messages: build_tool_conversation(12),
    };

    // Pointer for a non-existent tool_use_id.
    let pointers = vec![ToolOutputPointer {
        tool_use_id: "nonexistent".into(),
        turn: 99,
        original_chars: 9999,
        result_kind: "tool_result".into(),
    }];

    // Large context window so microcompaction suffices.
    let _ = compact_conversation_with_pointers(
        &provider,
        &mut conversation,
        "s",
        "t",
        CompactionContext::MidSession("worker".into()),
        1_000_000,
        &pointers,
    )
    .await;

    // No placeholder should mention the nonexistent id.
    let has_nonexistent = conversation.messages.iter().any(|m| {
        m.content.iter().any(|b| match b {
            ContentBlock::ToolResult { content, .. } => content
                .first()
                .and_then(|c| c.as_text())
                .map(|t| t.contains("nonexistent"))
                .unwrap_or(false),
            ContentBlock::Text { text } => text.contains("nonexistent"),
            _ => false,
        })
    });
    assert!(!has_nonexistent, "unmatched pointer must be ignored");

    // The real middle-aged tool results still get cleared with the legacy placeholder.
    let legacy: Vec<&str> = conversation
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => content.first().and_then(|c| c.as_text()),
            _ => None,
        })
        .filter(|t| t.starts_with("[Cleared — tool result"))
        .collect();
    assert!(
        !legacy.is_empty(),
        "real middle-aged results should still be cleared with legacy placeholders"
    );
}

/// Mirror of the production `find_orphaned_tool_result` check used by the
/// policy module's own tests: a tool result without a preceding matching
/// `ToolUse` is an orphan.
fn find_orphaned_tool_result(messages: &[Message]) -> Option<String> {
    let mut known_tool_ids = std::collections::HashSet::new();

    for msg in messages {
        if msg.role == Role::Assistant {
            for block in &msg.content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    known_tool_ids.insert(id.clone());
                }
            }
        }
        if msg.role == Role::User {
            for block in &msg.content {
                if let ContentBlock::ToolResult { tool_use_id, .. } = block
                    && !known_tool_ids.contains(tool_use_id)
                {
                    return Some(tool_use_id.clone());
                }
            }
        }
    }

    None
}
