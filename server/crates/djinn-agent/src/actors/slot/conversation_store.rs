use std::path::PathBuf;

use crate::message::Conversation;

fn conversation_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".djinn").join("conversations")
}

fn conversation_path(session_record_id: &str) -> PathBuf {
    conversation_dir().join(format!("{session_record_id}.json"))
}

/// Persist a conversation to disk so it can be resumed after a review cycle.
pub(crate) async fn save(
    session_record_id: &str,
    conversation: &Conversation,
) -> anyhow::Result<()> {
    let dir = conversation_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let path = conversation_path(session_record_id);
    let json = serde_json::to_string(conversation)?;
    let len = json.len();
    tokio::fs::write(&path, json).await?;
    tracing::debug!(
        session_record_id = %session_record_id,
        bytes = len,
        "conversation_store: saved conversation"
    );
    Ok(())
}

/// Load a previously saved conversation. Returns `None` if no file exists.
pub(crate) async fn load(session_record_id: &str) -> anyhow::Result<Option<Conversation>> {
    let path = conversation_path(session_record_id);
    if !path.exists() {
        return Ok(None);
    }
    let json = tokio::fs::read_to_string(&path).await?;
    let conversation: Conversation = serde_json::from_str(&json)?;
    tracing::debug!(
        session_record_id = %session_record_id,
        messages = conversation.messages.len(),
        "conversation_store: loaded conversation"
    );
    Ok(Some(conversation))
}

/// Delete a saved conversation file (called on task approval/cleanup).
#[expect(
    dead_code,
    reason = "cleanup hook reserved for lifecycle conversation teardown"
)]
pub(crate) async fn delete(session_record_id: &str) {
    let path = conversation_path(session_record_id);
    if let Err(e) = tokio::fs::remove_file(&path).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            session_record_id = %session_record_id,
            error = %e,
            "conversation_store: failed to delete conversation file"
        );
    }
}
