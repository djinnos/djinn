use djinn_compaction::{COMPACTION_SUMMARY_END_MARKER, bounded_message_identity};
use djinn_db::{
    BeginCompactionParams, CompleteCompactionParams, SessionCompactionBoundaryRepository,
    SessionMessageRepository,
};
use djinn_provider::message::{Conversation, Message, Role};

/// Persist a single conversation message to `session_messages`, best-effort.
///
/// Failures are logged and never propagated — persistence must not affect the task-run outcome.
pub(super) async fn persist_session_message(
    repo: &SessionMessageRepository,
    session_id: &str,
    task_id: &str,
    message: &Message,
) {
    if let Err(e) = repo
        .insert_messages_batch(session_id, task_id, std::slice::from_ref(message))
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            task_id = %task_id,
            error = %e,
            "reply_loop: failed to persist session message"
        );
    }
}

/// Record a `Started` compaction boundary before entering compaction.
///
/// Returns the boundary id so callers can complete it after successful
/// compaction, or `None` if the write fails. Failures are logged and never
/// propagated to the caller.
pub(super) async fn record_compaction_started(
    repo: &SessionCompactionBoundaryRepository,
    session_id: &str,
    conversation: &Conversation,
) -> Option<String> {
    let (
        first_message_id,
        last_compacted_message_id,
        first_retained_message_id,
        retained_tail_hash,
    ) = gather_boundary_identity(conversation);

    let marker_metadata = serde_json::json!({
        "marker_kind": "compaction_summary",
        "end_marker": COMPACTION_SUMMARY_END_MARKER,
    });

    match repo
        .record_compaction_started(BeginCompactionParams {
            session_id,
            schema_version: 1,
            first_message_id: first_message_id.as_deref(),
            last_compacted_message_id: last_compacted_message_id.as_deref(),
            first_retained_message_id: first_retained_message_id.as_deref(),
            retained_tail_hash: retained_tail_hash.as_deref(),
            marker_metadata: Some(&marker_metadata),
        })
        .await
    {
        Ok(boundary) => {
            tracing::info!(
                session_id = %session_id,
                boundary_id = %boundary.id,
                "reply_loop: compaction boundary started"
            );
            Some(boundary.id)
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "reply_loop: failed to record compaction boundary start"
            );
            None
        }
    }
}

/// Complete a compaction boundary after `compact_conversation` succeeds.
///
/// This should be called only when the in-memory `conversation` has been
/// replaced with a compacted representation and the summary is accepted. The
/// `compacted_conversation` passed here is the *already compacted* conversation
/// (used to derive the retained-tail identity). Failures are logged and never
/// propagated.
pub(super) async fn complete_compaction_boundary(
    repo: &SessionCompactionBoundaryRepository,
    boundary_id: Option<&str>,
    compacted_conversation: &Conversation,
    summary_text: &str,
) {
    let Some(boundary_id) = boundary_id else {
        return;
    };

    let (
        first_message_id,
        last_compacted_message_id,
        first_retained_message_id,
        retained_tail_hash,
    ) = gather_boundary_identity(compacted_conversation);

    let marker_metadata = serde_json::json!({
        "marker_kind": "compaction_summary",
        "end_marker": COMPACTION_SUMMARY_END_MARKER,
    });

    if let Err(e) = repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id,
            schema_version: 1,
            first_message_id: first_message_id.as_deref(),
            last_compacted_message_id: last_compacted_message_id.as_deref(),
            first_retained_message_id: first_retained_message_id.as_deref(),
            retained_tail_hash: retained_tail_hash.as_deref(),
            summary_text,
            marker_metadata: Some(&marker_metadata),
        })
        .await
    {
        tracing::warn!(
            boundary_id = %boundary_id,
            error = %e,
            "reply_loop: failed to complete compaction boundary"
        );
    }
}

/// Gather the source range and retained-tail identity for a boundary record.
///
/// For the source range we use the first non-system message id and the last
/// message id before the compaction point. For the retained tail we use the
/// first message id of the preserved tail after compaction, plus a stable hash
/// of the entire compacted conversation. Message ids are taken from
/// `MessageMeta::provider_data["id"]` if available; otherwise a stable SHA-256
/// of the message JSON is used as a synthetic identity bounded to 36 chars.
fn gather_boundary_identity(
    conversation: &Conversation,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    use sha2::{Digest, Sha256};

    let first_message_id = conversation
        .messages
        .iter()
        .find(|m| m.role != Role::System)
        .map(bounded_message_identity);
    let last_compacted_message_id = conversation.messages.last().map(bounded_message_identity);

    // The retained tail is the first non-summary, non-system, non-continuation
    // message after compaction. We detect the summary by looking for a user
    // message containing the end marker; the continuation message follows it.
    let mut first_retained_message_id: Option<String> = None;
    let mut found_summary = false;
    for msg in &conversation.messages {
        if found_summary
            && msg.role != Role::System
            && msg.text_content()
                != "Your context was compacted. The previous message contains a summary of the conversation so far. Continue calling tools as necessary to complete the task."
            && !msg
                .text_content()
                .starts_with("Part of your context was compacted.")
        {
            first_retained_message_id = Some(bounded_message_identity(msg));
            break;
        }
        if msg.role == Role::User && msg.text_content().contains(COMPACTION_SUMMARY_END_MARKER) {
            found_summary = true;
        }
    }

    let retained_tail_hash = {
        let mut hasher = Sha256::new();
        for msg in &conversation.messages {
            if let Ok(bytes) = serde_json::to_vec(msg) {
                hasher.update(&bytes);
            }
        }
        Some(format!("sha256:{}", hex::encode(hasher.finalize())))
    };

    (
        first_message_id,
        last_compacted_message_id,
        first_retained_message_id,
        retained_tail_hash,
    )
}

pub(super) fn serialize_message(msg: &Message) -> serde_json::Value {
    serde_json::to_value(msg).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "failed to serialize Message for SessionMessage event");
        serde_json::json!({
            "role": format!("{:?}", msg.role).to_lowercase(),
            "content": msg.content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>(),
        })
    })
}

pub(super) fn serialize_llm_input(
    conversation: &Conversation,
    tools: &[serde_json::Value],
) -> serde_json::Value {
    serde_json::json!({
        "messages": conversation.to_openai_messages(),
        "tools": tools,
    })
}

/// Assemble the canonical persisted assistant content for a turn.
///
/// Side-effect-free: builds and returns a `Vec<ContentBlock>` without mutating
/// any input.  Both normal finalization in `turn.rs` and the interrupted
/// `flush_in_flight_turn` path use this so the persisted shape is identical
/// regardless of which exit the turn took.
///
/// Canonical order:
/// 1. Provider-state blocks (signed thinking, OpenAIReasoning, etc.)
/// 2. One unsigned thinking block — the concatenation of all non-empty
///    unresolved `ThinkingDelta` fragments whose block ID was NOT matched by a
///    `ThinkingBlockComplete`. Unattributed `Thinking(String)` text (OpenAI
///    reasoning) is always included because it has no block ID to reconcile
///    against.
/// 3. Assistant text.
/// 4. Tool calls.
///
/// Reconciliation is by exact block ID only. No value-, prefix-, suffix-, or
/// presence-based deduplication is ever applied.
pub(super) fn assemble_persisted_content(
    provider_state: &[djinn_provider::message::ContentBlock],
    unresolved_thinking: &[super::streaming::UnresolvedThinkingFragment],
    completed_thinking_ids: &std::collections::HashSet<u64>,
    text: &str,
    tool_calls: &[djinn_provider::message::ContentBlock],
) -> Vec<djinn_provider::message::ContentBlock> {
    use djinn_provider::message::ContentBlock;

    let mut content: Vec<ContentBlock> = Vec::new();

    // 1. Provider-state blocks.
    content.extend(provider_state.iter().cloned());

    // 2. One unsigned thinking block from non-empty unresolved fragments.
    //    Reconcile by exact block ID: a fragment is suppressed only if its ID
    //    appears in the completed set. Unattributed text (Thinking(String),
    //    OpenAI reasoning) has no block ID and is always included.
    let mut unsigned = String::new();
    for fragment in unresolved_thinking {
        match fragment {
            super::streaming::UnresolvedThinkingFragment::Attributed { id, text }
                if !completed_thinking_ids.contains(id) =>
            {
                unsigned.push_str(text)
            }
            super::streaming::UnresolvedThinkingFragment::Unattributed(text) => {
                unsigned.push_str(text);
            }
            super::streaming::UnresolvedThinkingFragment::Attributed { .. } => {}
        }
    }
    if !unsigned.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: unsigned,
            signature: None,
        });
    }

    // 3. Assistant text.
    if !text.is_empty() {
        content.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }

    // 4. Tool calls.
    content.extend(tool_calls.iter().cloned());

    content
}

/// Idempotently persist any observed assistant/tool rows from the current
/// in-flight turn that were not finalized through the normal reply-loop
/// completion path.
///
/// This is called when a turn is interrupted, cancelled, or ends early (e.g.
/// stream cancellation, deploy drain, stall-kill) so that partially-observed
/// assistant content and completed streaming tool results survive session
/// release and are visible on resume/timeline.
///
/// The `turn_flushed` flag on [`StreamTurnState`] guards against duplicate
/// persistence: once set, repeated calls within the same turn are no-ops.
/// This makes the helper safe to call from multiple teardown paths.
///
/// Best-effort: persistence failures are logged and never propagated.
pub(super) async fn flush_in_flight_turn(
    repo: &SessionMessageRepository,
    session_id: &str,
    task_id: &str,
    now_timestamp: i64,
    stream_state: &mut super::streaming::StreamTurnState,
) {
    use djinn_provider::message::{ContentBlock, Message, MessageMeta, Role};

    if stream_state.turn_flushed {
        return;
    }

    // Build and persist the assistant message from accumulated turn content
    // using the canonical assembler — provider-state blocks, one unsigned
    // thinking block from unresolved fragments, assistant text, then tool
    // calls. Reconciles thinking fragments by exact block ID only.
    let assistant_content = assemble_persisted_content(
        &stream_state.turn_provider_state,
        &stream_state.turn_unresolved_thinking,
        &stream_state.turn_completed_thinking_ids,
        &stream_state.turn_text,
        &stream_state.turn_tool_calls,
    );
    if !assistant_content.is_empty() {
        let assistant_msg = Message {
            role: Role::Assistant,
            content: assistant_content,
            metadata: Some(MessageMeta {
                input_tokens: Some(stream_state.turn_tokens_in),
                output_tokens: Some(stream_state.turn_tokens_out),
                timestamp: Some(now_timestamp),
                provider_data: None,
            }),
        };
        persist_session_message(repo, session_id, task_id, &assistant_msg).await;
    }

    // Persist any completed streaming tool results.  `streaming_results`
    // contains `(content_block_index, result_block)` for tool calls that
    // were dispatched early and whose futures completed before the stream
    // was interrupted.  We wrap them as a single User/ToolResult message,
    // matching the normal turn finalize pattern.
    if !stream_state.streaming_results.is_empty() {
        let tool_result_content: Vec<ContentBlock> = stream_state
            .streaming_results
            .iter()
            .map(|(_, result_block)| match result_block {
                ContentBlock::ToolResult { .. } => result_block.clone(),
                other => ContentBlock::ToolResult {
                    tool_use_id: String::new(),
                    content: vec![other.clone()],
                    is_error: false,
                },
            })
            .collect();
        let result_msg = Message {
            role: Role::User,
            content: tool_result_content,
            metadata: None,
        };
        persist_session_message(repo, session_id, task_id, &result_msg).await;
    }

    stream_state.turn_flushed = true;
}

#[cfg(test)]
mod thinking_reconciliation_tests {
    use super::assemble_persisted_content;
    use crate::reply_loop::streaming::UnresolvedThinkingFragment::{Attributed, Unattributed};
    use djinn_provider::message::ContentBlock;
    use std::collections::HashSet;

    fn fallback(
        fragments: Vec<crate::reply_loop::streaming::UnresolvedThinkingFragment>,
    ) -> String {
        let content = assemble_persisted_content(&[], &fragments, &HashSet::new(), "", &[]);
        match content.as_slice() {
            [
                ContentBlock::Thinking {
                    thinking,
                    signature: None,
                },
            ] => thinking.clone(),
            other => panic!("unexpected canonical content: {other:?}"),
        }
    }

    #[test]
    fn unresolved_fragments_preserve_both_arrival_orders() {
        assert_eq!(
            fallback(vec![
                Unattributed("U1".into()),
                Attributed {
                    id: 9,
                    text: "A".into()
                },
            ]),
            "U1A"
        );
        assert_eq!(
            fallback(vec![
                Attributed {
                    id: 9,
                    text: "A".into()
                },
                Unattributed("U1".into()),
            ]),
            "AU1"
        );
    }

    #[test]
    fn exact_id_suppression_preserves_other_fragments() {
        let fragments = vec![
            Attributed {
                id: 1,
                text: "done".into(),
            },
            Unattributed("U".into()),
            Attributed {
                id: 2,
                text: "partial".into(),
            },
        ];
        let content = assemble_persisted_content(&[], &fragments, &HashSet::from([1]), "", &[]);
        assert!(matches!(
            content.as_slice(),
            [ContentBlock::Thinking { thinking, signature: None }] if thinking == "Upartial"
        ));
    }
}
