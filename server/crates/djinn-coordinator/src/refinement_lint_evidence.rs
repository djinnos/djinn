//! Durable Advocate lint-rejection evidence parsing and correction context.
//!
//! Proposal mutation ToolResults are persisted in session messages, not task
//! activity. Classification deliberately reads the raw conversation so a
//! compaction boundary cannot hide a rejected candidate.

use djinn_core::events::DjinnEventEnvelope;
use djinn_core::message::{ContentBlock, Conversation};
use djinn_db::{Database, SessionMessageRepository, SessionRepository};
use tokio::sync::broadcast;

use super::refinement::AdvocateLintViolation;

/// A refinement task is dedicated to a single role pass. Its completed session
/// contains the reply loop's durable ToolResult evidence.
pub(super) async fn advocate_lint_rejection_from_session(
    db: &Database,
    events_tx: &broadcast::Sender<DjinnEventEnvelope>,
    task_id: &str,
) -> Result<Option<Vec<AdvocateLintViolation>>, String> {
    let event_bus = crate::events::event_bus_for(events_tx);
    let sessions = SessionRepository::new(db.clone(), event_bus.clone())
        .list_for_task(task_id)
        .await
        .map_err(|error| error.to_string())?;
    let session = sessions
        .into_iter()
        .find(|session| session.status == "completed")
        .ok_or_else(|| "no completed session exists for refinement task".to_string())?;

    // This is audit/classification evidence, not model context. The normal
    // conversation accessor applies compaction and can replace the rejecting
    // ToolResult with a summary. Read the immutable raw timeline instead.
    let conversation = SessionMessageRepository::new(db.clone(), event_bus)
        .load_raw_conversation(&session.id)
        .await
        .map_err(|error| error.to_string())?;
    Ok(parse_spec_lint_rejection_from_conversation(&conversation))
}

/// Inspect only persisted ToolResult blocks. Assistant prose mentioning the
/// error must not turn an ordinary completion into a correction retry.
pub(super) fn parse_spec_lint_rejection_from_conversation(
    conversation: &Conversation,
) -> Option<Vec<AdvocateLintViolation>> {
    conversation.messages.iter().rev().find_map(|message| {
        message
            .content
            .iter()
            .rev()
            .find_map(parse_spec_lint_rejection_from_tool_result)
    })
}

fn parse_spec_lint_rejection_from_tool_result(
    block: &ContentBlock,
) -> Option<Vec<AdvocateLintViolation>> {
    let ContentBlock::ToolResult { content, .. } = block else {
        return None;
    };
    content.iter().rev().find_map(|block| match block {
        ContentBlock::Text { text } => parse_spec_lint_rejection(text),
        ContentBlock::ToolResult { .. } => parse_spec_lint_rejection_from_tool_result(block),
        _ => None,
    })
}

/// Decode the structured authoring rejection emitted by proposal mutations.
/// Tool-result text can wrap a response, so walk object/array values but only
/// accept the stable code together with a fully structured violation.
pub(super) fn parse_spec_lint_rejection(payload: &str) -> Option<Vec<AdvocateLintViolation>> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    find_spec_lint_rejection(&value)
}

fn find_spec_lint_rejection(value: &serde_json::Value) -> Option<Vec<AdvocateLintViolation>> {
    if let Some(object) = value.as_object() {
        if object.get("code").and_then(serde_json::Value::as_str) == Some("SPEC_LINT_REJECTED") {
            let violations = object.get("violations")?.as_array()?;
            let parsed: Option<Vec<_>> = violations
                .iter()
                .map(|violation| {
                    let span = violation.get("span")?.as_object()?;
                    Some(AdvocateLintViolation {
                        code: violation.get("code")?.as_str()?.to_owned(),
                        message: violation.get("message")?.as_str()?.to_owned(),
                        start_byte: span.get("start_byte")?.as_u64()?,
                        end_byte: span.get("end_byte")?.as_u64()?,
                    })
                })
                .collect();
            if let Some(violations) = parsed.filter(|violations| !violations.is_empty()) {
                return Some(violations);
            }
        }
        return object.values().find_map(find_spec_lint_rejection);
    }
    value.as_array()?.iter().find_map(find_spec_lint_rejection)
}

/// Render persisted rejection diagnostics in their already-established stable
/// order. Do not sort here: authoring owns ordering (span, then code).
pub(super) fn format_advocate_lint_correction_context(
    violations: &[AdvocateLintViolation],
) -> Option<String> {
    if violations.is_empty() {
        return None;
    }
    let mut context = String::from(
        "Your previous proposal mutation was rejected with SPEC_LINT_REJECTED. Correct every violation below and submit a clean material write; the proposal head was not changed.\n",
    );
    for violation in violations {
        context.push_str(&format!(
            "- {}: {} at bytes {}..{}\n",
            violation.code, violation.message, violation.start_byte, violation.end_byte
        ));
    }
    Some(context)
}
