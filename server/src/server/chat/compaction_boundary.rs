use djinn_compaction::{COMPACTION_SUMMARY_END_MARKER, bounded_message_identity};
use djinn_db::{
    BeginCompactionParams, CompleteCompactionParams, SessionCompactionBoundaryRepository,
};
use djinn_provider::message::{Conversation, Role};

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Marker metadata written with chat compaction boundaries so that the shared
/// `SessionMessageRepository::load_conversation` projection can recognise the
/// boundary shape. Matches `server/crates/djinn-slot/src/reply_loop/persistence.rs`.
fn compaction_marker_metadata() -> serde_json::Value {
    serde_json::json!({
        "marker_kind": "compaction_summary",
        "end_marker": COMPACTION_SUMMARY_END_MARKER,
    })
}

/// Gather source-range + retained-tail identity for a chat compaction boundary.
///
/// Message ids are taken from `MessageMeta::provider_data["id"]` if available;
/// otherwise a stable SHA-256 of the message JSON is used as a synthetic identity.
fn gather_chat_boundary_identity(
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
        Some(format!("sha256:{}", hex_encode(&hasher.finalize())))
    };

    (
        first_message_id,
        last_compacted_message_id,
        first_retained_message_id,
        retained_tail_hash,
    )
}

/// Extract the accepted summary text from an already compacted chat conversation.
///
/// Returns `None` if the conversation does not contain a recognised compaction
/// summary + continuation pair (i.e. the compaction was not accepted).
fn accepted_summary_text(conversation: &Conversation) -> Option<String> {
    for i in 1..conversation.messages.len() {
        if conversation.messages[i].role == Role::Assistant
            && (conversation.messages[i].text_content()
                == "Your context was compacted. The previous message contains a summary of the conversation so far. Continue calling tools as necessary to complete the task."
                || conversation.messages[i]
                    .text_content()
                    .starts_with("Part of your context was compacted."))
            && conversation.messages[i - 1].role == Role::User
        {
            let summary = conversation.messages[i - 1]
                .text_content()
                .split(COMPACTION_SUMMARY_END_MARKER)
                .next()
                .unwrap_or("")
                .to_string();
            return Some(summary);
        }
    }
    None
}

/// Record a `Started` compaction boundary before attempting chat compaction.
///
/// Returns the boundary id on success, or `None` if the write fails. Failures are
/// logged and never propagated to the caller so persistence cannot abort the chat.
pub(crate) async fn record_chat_compaction_started(
    repo: &SessionCompactionBoundaryRepository,
    session_id: &str,
    conversation: &Conversation,
) -> Option<String> {
    let (
        first_message_id,
        last_compacted_message_id,
        first_retained_message_id,
        retained_tail_hash,
    ) = gather_chat_boundary_identity(conversation);
    let marker_metadata = compaction_marker_metadata();
    match repo
        .record_compaction_started(BeginCompactionParams {
            session_id,
            schema_version: 1,
            trigger: None,
            current_context_tokens_before: None,
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
                "chat: compaction boundary started"
            );
            Some(boundary.id)
        }
        Err(e) => {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "chat: failed to record compaction boundary start"
            );
            None
        }
    }
}

/// Complete a chat compaction boundary after `compact_conversation` succeeds and
/// the in-memory conversation contains an accepted compaction summary.
///
/// This is called only when the compacted conversation has a recognised summary
/// marker pair. Failures are logged and never propagated.
pub(crate) async fn complete_chat_compaction_boundary(
    repo: &SessionCompactionBoundaryRepository,
    boundary_id: Option<&str>,
    compacted_conversation: &Conversation,
) {
    let Some(boundary_id) = boundary_id else {
        return;
    };
    let Some(summary_text) = accepted_summary_text(compacted_conversation) else {
        return;
    };
    let (
        first_message_id,
        last_compacted_message_id,
        first_retained_message_id,
        retained_tail_hash,
    ) = gather_chat_boundary_identity(compacted_conversation);
    let marker_metadata = compaction_marker_metadata();
    if let Err(e) = repo
        .complete_compaction_boundary(CompleteCompactionParams {
            boundary_id,
            schema_version: 1,
            current_context_tokens_after: None,
            first_message_id: first_message_id.as_deref(),
            last_compacted_message_id: last_compacted_message_id.as_deref(),
            first_retained_message_id: first_retained_message_id.as_deref(),
            retained_tail_hash: retained_tail_hash.as_deref(),
            summary_text: &summary_text,
            marker_metadata: Some(&marker_metadata),
        })
        .await
    {
        tracing::warn!(
            boundary_id = %boundary_id,
            error = %e,
            "chat: failed to complete compaction boundary"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::CompactionPhase;
    use djinn_provider::message::Message;

    fn test_db() -> djinn_db::Database {
        djinn_db::Database::open_in_memory().unwrap()
    }

    async fn seed_chat_session(db: &djinn_db::Database, session_id: &str) {
        // Global chat sessions are projectless (agent_type = 'chat',
        // project_id NULL) per migration 15's project-scope CHECK.
        djinn_db::test_support::seed_chat_session_row(db, session_id).await;
    }

    fn compacted_conversation() -> Conversation {
        let mut conv = Conversation::new();
        conv.push(Message::system("be helpful"));
        conv.push(Message::user(format!(
            "compact summary{}",
            COMPACTION_SUMMARY_END_MARKER
        )));
        conv.push(Message::assistant(
            "Your context was compacted. The previous message contains a summary of the conversation so far. Continue calling tools as necessary to complete the task.",
        ));
        conv.push(Message::user("follow up"));
        conv
    }

    fn unaccepted_conversation() -> Conversation {
        let mut conv = Conversation::new();
        conv.push(Message::system("be helpful"));
        conv.push(Message::user("hello"));
        conv.push(Message::assistant("hi"));
        conv
    }

    #[test]
    fn accepted_summary_text_recognises_compacted_pair() {
        let conv = compacted_conversation();
        let summary = accepted_summary_text(&conv).expect("should extract summary");
        assert_eq!(summary, "compact summary");
    }

    #[test]
    fn accepted_summary_text_returns_none_without_marker_pair() {
        let conv = unaccepted_conversation();
        assert!(accepted_summary_text(&conv).is_none());
    }

    #[test]
    fn gather_chat_boundary_identity_finds_retained_tail() {
        let conv = compacted_conversation();
        let (_, _, first_retained, tail_hash) = gather_chat_boundary_identity(&conv);
        assert_eq!(
            first_retained,
            Some(bounded_message_identity(conv.messages.last().unwrap()))
        );
        assert!(tail_hash.unwrap().starts_with("sha256:"));
    }

    #[tokio::test]
    async fn record_and_complete_chat_compaction_boundary() {
        let db = test_db();
        let session_id = uuid::Uuid::now_v7().to_string();
        seed_chat_session(&db, &session_id).await;
        let repo = SessionCompactionBoundaryRepository::new(db.clone());

        let before = unaccepted_conversation();
        let boundary_id = record_chat_compaction_started(&repo, &session_id, &before)
            .await
            .expect("record start should succeed");

        let boundary = repo.fetch_by_id(&boundary_id).await.unwrap();
        assert_eq!(boundary.phase, CompactionPhase::Started);
        assert!(boundary.summary_text.is_none());

        let after = compacted_conversation();
        complete_chat_compaction_boundary(&repo, Some(&boundary_id), &after).await;

        let boundary = repo.fetch_by_id(&boundary_id).await.unwrap();
        assert_eq!(boundary.phase, CompactionPhase::Ended);
        assert_eq!(boundary.summary_text.as_deref(), Some("compact summary"));
        let marker_metadata = boundary.marker_metadata.expect("marker metadata present");
        assert_eq!(marker_metadata["marker_kind"], "compaction_summary");
        assert_eq!(
            marker_metadata["end_marker"].as_str().unwrap(),
            COMPACTION_SUMMARY_END_MARKER
        );
    }

    #[tokio::test]
    async fn failed_chat_compaction_leaves_started_only() {
        let db = test_db();
        let session_id = uuid::Uuid::now_v7().to_string();
        seed_chat_session(&db, &session_id).await;
        let repo = SessionCompactionBoundaryRepository::new(db.clone());

        let before = unaccepted_conversation();
        let boundary_id = record_chat_compaction_started(&repo, &session_id, &before)
            .await
            .expect("record start should succeed");

        // Simulate a failed compaction: compact_conversation returned false, so
        // no completion helper was called.
        let boundary = repo.fetch_by_id(&boundary_id).await.unwrap();
        assert_eq!(boundary.phase, CompactionPhase::Started);
        assert!(boundary.summary_text.is_none());
        assert_eq!(repo.boundary_count(&session_id).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn normal_chat_messages_do_not_create_boundaries() {
        let db = test_db();
        let session_id = uuid::Uuid::now_v7().to_string();
        seed_chat_session(&db, &session_id).await;
        let events = EventBus::noop();
        let message_repo = djinn_db::SessionMessageRepository::new(db.clone(), events);
        let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

        let msg = djinn_provider::message::Message::user("hello");
        message_repo
            .insert_messages_batch(&session_id, "", std::slice::from_ref(&msg))
            .await
            .unwrap();

        assert_eq!(boundary_repo.boundary_count(&session_id).await.unwrap(), 0);
    }
}
