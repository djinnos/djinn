//! Session-message persistence and LLM-input serialization helpers for the
//! reply loop.  Extracted from the original `reply_loop.rs` so each phase of
//! the turn lives in its own focused submodule.  All items are `pub(super)`
//! so they remain reachable from sibling submodules inside `reply_loop`.

use djinn_db::SessionMessageRepository;
use djinn_provider::message::{Conversation, Message};

/// Persist a single conversation message to `session_messages`, best-effort.
///
/// Failures are logged and never propagated — persistence must not affect the
/// task-run outcome.
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
