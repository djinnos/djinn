//! Provider error classification helpers.
//!
//! These functions detect specific error classes that the reply loop and
//! compaction subsystem need to react to.  They live in the provider crate
//! because they reference [`ProviderError`] and are consumed across crate
//! boundaries (djinn-agent reply loop, djinn-compaction, and
//! lifecycle/teardown).

use crate::provider::error::ProviderError;

/// Whether `e` is a context-length / context-window overflow error.
///
/// Prefers the typed provider taxonomy when present (set at the
/// provider-crate boundary), then falls back to substring matching for
/// resilience against untyped/legacy error paths.
pub fn is_context_length_error(e: &anyhow::Error) -> bool {
    if let Some(ProviderError::ContextOverflow) = e.downcast_ref::<ProviderError>() {
        return true;
    }
    let msg = e.to_string().to_lowercase();
    msg.contains("context_length")
        || msg.contains("context limit")
        || msg.contains("too many tokens")
        || msg.contains("maximum context")
        || msg.contains("context window")
        || msg.contains("prompt is too long")
        || msg.contains("max_tokens")
        || msg.contains("token limit")
}

/// Detect "No tool call found for function call output" errors from the
/// OpenAI Responses API.
///
/// These happen when a `tool` role message references a `tool_call_id` that
/// doesn't exist in any preceding assistant message — typically after
/// compaction removed the assistant message but left orphaned tool results.
/// Also matches the inverse "No tool output found for function call ..."
/// which fires when an assistant function_call has no matching tool_output
/// entry (e.g. session was interrupted mid-turn).
pub fn is_orphaned_tool_call_error_str(msg: &str) -> bool {
    let msg = msg.to_lowercase();
    msg.contains("no tool call found for function call output")
        || msg.contains("no tool output found for function call")
        || msg.contains("no function call found")
        // OpenAI-compatible Chat Completions variant (e.g. kimi-for-coding):
        // "an assistant message with 'tool_calls' must be followed by tool
        //  messages responding to each 'tool_call_id'. The following
        //  tool_call_ids did not have response messages: ...".
        || msg.contains("must be followed by tool messages")
        || msg.contains("did not have response messages")
}

/// Whether `e` is an orphaned tool-call / tool-result reference error.
pub fn is_orphaned_tool_call_error(e: &anyhow::Error) -> bool {
    is_orphaned_tool_call_error_str(&e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_orphaned_tool_call_variants() {
        // Variant the existing detector already covered.
        assert!(is_orphaned_tool_call_error_str(
            "No tool call found for function call output call_abc"
        ));
        assert!(is_orphaned_tool_call_error_str("No function call found"));
        // The 400 message we observed in production for poisoned sessions.
        assert!(is_orphaned_tool_call_error_str(
            "provider stream event failed: display=provider API error 400 Bad Request: { \
             \"error\": { \"message\": \"No tool output found for function call \
             call_GTQn9uVLax1RG4uWvMNrl3Sq.\", \"type\": \"invalid_request_error\" } }"
        ));
        // The OpenAI-compatible Chat Completions variant (kimi-for-coding, 2026-07-02).
        assert!(is_orphaned_tool_call_error_str(
            "provider stream event failed: provider API error 400 Bad Request: {\"error\":\
             {\"type\":\"invalid_request_error\",\"message\":\"an assistant message with \
             'tool_calls' must be followed by tool messages responding to each \
             'tool_call_id'. The following tool_call_ids did not have response messages: \
             code_search:24\"}}"
        ));
        // Negative cases.
        assert!(!is_orphaned_tool_call_error_str("rate limited"));
        assert!(!is_orphaned_tool_call_error_str("context length exceeded"));
    }

    #[test]
    fn context_length_error_prefers_typed_then_substring() {
        // Typed source via downcast — no context-length substring present.
        let typed = anyhow::Error::new(ProviderError::ContextOverflow)
            .context("provider API error 413: too big");
        assert!(is_context_length_error(&typed));

        // A typed RateLimit is NOT a context-length error.
        let rate = anyhow::Error::new(ProviderError::RateLimit {
            retry_after_ms: None,
        })
        .context("provider API error 429");
        assert!(!is_context_length_error(&rate));

        // Substring fallback for untyped/legacy errors.
        let untyped = anyhow::anyhow!("This model's maximum context length is 128000 tokens");
        assert!(is_context_length_error(&untyped));

        let unrelated = anyhow::anyhow!("connection reset by peer");
        assert!(!is_context_length_error(&unrelated));
    }
}
