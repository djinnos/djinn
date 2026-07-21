//! Djinn-native conversation compaction.
//!
//! When the accumulated input token count reaches 80% of the model's context
//! window, `compact_conversation` summarises the conversation via the LLM and
//! replaces the in-memory `Conversation` with a compact representation.
//!
//! **Durability boundary.** This crate only owns the in-memory replacement of
//! the conversation with the compacted form. Persisting the original messages
//! and projecting the compacted conversation back onto disk is the
//! responsibility of upstream callers (the chat / worker reply loops that
//! invoke `compact_conversation`) and the durable boundary storage owned by
//! sibling epics. Callers should not assume that simply running compaction
//! here guarantees the original history survives.

mod policy;
mod prompts;
mod summarizer;
mod truncate;

pub use policy::{compact_conversation, needs_compaction};
pub use prompts::CompactionContext;
pub use prompts::{
    COMPACTION_SUMMARY_END_MARKER, PARTIAL_COMPACTION_CONTINUATION, strip_compaction_markers,
};
pub use prompts::{extract_prior_summary, previous_summary_block};

pub use summarizer::{call_llm_for_summary_for_test, do_partial_compact_for_test};
/// Maximum length for a message identity that fits the boundary table's
/// `VARCHAR(36)` id columns (`first_message_id`, `last_compacted_message_id`,
/// `first_retained_message_id` — see migration 92).
const MESSAGE_IDENTITY_MAX_LEN: usize = 36;

/// Stable per-message identity that fits the boundary table's `VARCHAR(36)` id
/// columns (`first_message_id`, `last_compacted_message_id`,
/// `first_retained_message_id` — see migration 92). Prefers the
/// provider-assigned message id when present and short enough; otherwise falls
/// back to a content hash truncated to 36 characters. 34 hex chars (136 bits)
/// is far beyond any collision risk across the messages of one conversation.
///
/// This is the shared implementation used by both the chat-path and worker-path
/// compaction boundary persistence.
pub fn bounded_message_identity(msg: &djinn_provider::message::Message) -> String {
    use sha2::{Digest, Sha256};

    if let Some(serde_json::Value::Object(provider_data)) =
        msg.metadata.as_ref().and_then(|m| m.provider_data.as_ref())
        && let Some(serde_json::Value::String(id)) = provider_data.get("id")
        && id.len() <= MESSAGE_IDENTITY_MAX_LEN
    {
        return id.clone();
    }
    let digest = {
        use std::fmt::Write;
        let raw = Sha256::digest(serde_json::to_vec(msg).unwrap_or_default());
        let mut out = String::with_capacity(raw.len() * 2);
        for b in raw {
            let _ = write!(out, "{b:02x}");
        }
        out
    };
    format!("h:{}", &digest[..MESSAGE_IDENTITY_MAX_LEN - 2])
}

/// Whether `err` is a failure that reactive compaction can recover from — a
/// context-window overflow or an orphaned tool-call/result reference. Both are
/// resolved by summarising the conversation and retrying, so any conversation
/// loop (worker reply loop *or* the chat handler) can gate a compact-and-retry
/// on this. Uses the provider-layer classifiers.
pub fn is_compaction_recoverable_error(err: &anyhow::Error) -> bool {
    djinn_provider::error_classify::is_context_length_error(err)
        || djinn_provider::error_classify::is_orphaned_tool_call_error(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::provider::error::ProviderError;

    #[test]
    fn recoverable_error_covers_overflow_and_orphaned_tool() {
        // Typed context overflow.
        assert!(is_compaction_recoverable_error(&anyhow::Error::new(
            ProviderError::ContextOverflow
        )));
        // Untyped context-length message (legacy path).
        assert!(is_compaction_recoverable_error(&anyhow::anyhow!(
            "400 prompt is too long: 210000 tokens > 200000 maximum"
        )));
        // Orphaned tool-call/result reference.
        assert!(is_compaction_recoverable_error(&anyhow::anyhow!(
            "No tool call found for function call output with id call_123"
        )));
        // Unrelated failures are NOT recoverable by compaction.
        assert!(!is_compaction_recoverable_error(&anyhow::Error::new(
            ProviderError::Authentication
        )));
        assert!(!is_compaction_recoverable_error(&anyhow::anyhow!(
            "connection reset by peer"
        )));
    }

    #[test]
    fn bounded_message_identity_fallback_never_exceeds_36_chars() {
        // A message with no provider_data — triggers the hash fallback.
        let msg = djinn_provider::message::Message::user("hello world");
        let identity = bounded_message_identity(&msg);
        assert!(
            identity.len() <= 36,
            "identity too long: {identity} ({} chars)",
            identity.len()
        );
        assert!(identity.starts_with("h:"), "unexpected prefix: {identity}");
        assert_eq!(identity.len(), 36);
    }

    #[test]
    fn bounded_message_identity_preserves_short_provider_id() {
        use serde_json::json;

        let mut msg = djinn_provider::message::Message::user("test");
        let id = "msg_abc123";
        msg.metadata = Some(djinn_provider::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({"id": id})),
        });
        let identity = bounded_message_identity(&msg);
        assert_eq!(identity, id);
    }

    #[test]
    fn bounded_message_identity_truncates_long_provider_id() {
        use serde_json::json;

        let mut msg = djinn_provider::message::Message::user("test");
        let long_id = "x".repeat(50); // 50 chars > 36
        msg.metadata = Some(djinn_provider::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({"id": long_id})),
        });
        let identity = bounded_message_identity(&msg);
        assert!(
            identity.len() <= 36,
            "identity too long: {identity} ({} chars)",
            identity.len()
        );
        assert!(identity.starts_with("h:"), "unexpected prefix: {identity}");
    }
}
