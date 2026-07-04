use djinn_compaction::COMPACTION_SUMMARY_END_MARKER;
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
/// `MessageMeta::provider_data["id"]` if available; otherwise the stable SHA-256
/// of the message JSON is used as a synthetic identity.
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
        .map(message_identity);
    let last_compacted_message_id = conversation.messages.last().map(message_identity);

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
            first_retained_message_id = Some(message_identity(msg));
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

fn message_identity(msg: &Message) -> String {
    if let Some(serde_json::Value::Object(provider_data)) =
        msg.metadata.as_ref().and_then(|m| m.provider_data.as_ref())
        && let Some(serde_json::Value::String(id)) = provider_data.get("id")
    {
        return id.clone();
    }
    use sha2::{Digest, Sha256};
    format!(
        "hash:{}",
        hex::encode(Sha256::digest(serde_json::to_vec(msg).unwrap_or_default()))
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
