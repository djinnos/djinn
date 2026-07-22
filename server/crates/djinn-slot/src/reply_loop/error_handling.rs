//! Reply loop error handling.
//!
//! Error classification, wind-down directives, nudge messages, and retry
//! predicates for the reply loop.  Adapted from the agent implementation;
//! depends only on `djinn-provider` message types (no `djinn-agent` imports).

use djinn_provider::message::Message;
use djinn_provider::provider::ToolChoice;
use std::fmt;

// Re-export provider-layer classifiers so callers in the reply loop can import
// them from this module as before.
pub use djinn_provider::error_classify::{is_context_length_error, is_orphaned_tool_call_error};
pub use djinn_provider::provider::{
    ExhaustedTransportCategory, ExhaustedTransportDiagnostic, TransportClassificationInput,
    classify_exhausted_transport,
};

/// Exact serialized-body predicate; the compatibility value name means UTF-8 bytes.
pub fn is_oversized_transport_payload(
    estimated_payload_chars: usize,
    known_context_window_tokens: u32,
) -> bool {
    djinn_provider::provider::oversized_transport_request(
        estimated_payload_chars,
        known_context_window_tokens,
    )
}

/// Provider/account failure patterns that may appear as "successful" assistant
/// prose when the model echoes back a rate-limit, quota, out-of-credits, or
/// provider error message instead of refusing or erroring at the HTTP/stream
/// level.  When detected, the turn should be reclassified as a typed provider
/// failure and must not be persisted as a successful assistant message.
const PROVIDER_FAILURE_PROSE_PATTERNS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "ratelimit",
    "too many requests",
    "too many retries",
    "requests per minute",
    "tokens per minute",
    "tpm limit",
    "rpm limit",
    "quota exceeded",
    "quota_exceeded",
    "insufficient_quota",
    "out of credits",
    "out of quota",
    "credit balance",
    "billing",
    "subscription required",
    "payment required",
    "usage limit",
    "monthly limit",
    "api key not valid",
    "invalid api key",
    "authentication error",
    "account suspended",
    "account disabled",
    "service unavailable",
    "provider error",
    "internal server error",
    "overloaded",
    "capacity",
    "try again later",
    "temporarily unavailable",
    "exceeded the maximum",
    "usage cap",
    "spending limit",
];

/// Detect assistant text that looks like a disguised provider failure rather
/// than genuine model output.  Prose in [`PROVIDER_FAILURE_PROSE_PATTERNS`] is
/// matched case-insensitively and must dominate the text (not just an
/// incidental mention inside an otherwise productive response).
///
/// Returns `true` when the text is short (< 200 chars) and contains any of the
/// failure patterns.  Long responses that happen to mention "rate limit" in
/// passing are not flagged.
pub fn is_provider_failure_prose(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let normalized = lower.trim();
    if normalized.is_empty() {
        return false;
    }
    // Only flag short texts that contain a failure pattern — avoid false
    // positives on a long response that happens to mention "rate limit" once.
    normalized.len() < 200
        && PROVIDER_FAILURE_PROSE_PATTERNS
            .iter()
            .any(|pat| normalized.contains(pat))
}

/// Maximum retries for empty assistant turns before treating as a hard failure.
pub const MAX_EMPTY_TURN_RETRIES: u32 = 2;

/// Number of consecutive **zero-progress** provider turns *at session start*
/// after which the run is terminated so the host model circuit-breaker is fed
/// early — before the coordinator's 30-minute stall kill would otherwise be the
/// only failure signal (each such stall costs a full worker slot for ~30 min).
///
/// "Zero-progress" means a turn that produced no usable assistant output AND no
/// token usage — the provider accepted the dispatch and did no real work (the
/// `xiaomi-token-plan-sgp/mimo-v2.5-pro` incident: accept a priority task, emit
/// nothing, burn the slot). It is deliberately conservative:
/// - It only counts turns *before the session's first productive turn*. A single
///   empty turn mid-session (after the model has already done real work) does NOT
///   count — models recover, and a mid-run blip must not fail a working session.
/// - Reasoning-only turns (usage > 0, no visible output) are NOT zero-progress:
///   they carry their own nudge path ([`reasoning_only_nudge_message`]).
///
/// Two such turns is enough to distinguish "structurally broken for this
/// dispatch" from a one-off empty first turn.
pub const EMPTY_START_TURNS_BEFORE_BREAKER: u32 = 2;

/// Whether a run of consecutive zero-progress turns *at session start* should
/// terminate the run to feed the breaker early. `saw_productive_turn` gates the
/// whole thing: once the session has made any real progress, this never trips —
/// a lone empty turn mid-session is handled by the ordinary empty-turn retry
/// path, not by failing the session over.
pub fn empty_start_streak_feeds_breaker(
    consecutive_empty_start_turns: u32,
    saw_productive_turn: bool,
) -> bool {
    !saw_productive_turn && consecutive_empty_start_turns >= EMPTY_START_TURNS_BEFORE_BREAKER
}
/// Maximum consecutive text-only turns before treating as a session failure.
/// Each text-only turn without a finalize tool call triggers a nudge message.
pub const MAX_NUDGE_ATTEMPTS: u32 = 3;
/// Maximum reactive compaction attempts before giving up.
pub const MAX_COMPACTION_RETRIES: u32 = 2;

/// Typed reply-loop outcome for a budget-triggered wind-down whose one granted
/// final turn produced no hand-off text.
#[derive(Debug, Clone)]
pub struct BudgetWindDownIgnored {
    pub details: String,
}

impl fmt::Display for BudgetWindDownIgnored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "budget wind-down ignored: {}", self.details)
    }
}

impl std::error::Error for BudgetWindDownIgnored {}

/// Build the wind-down directive injected on the final permitted turn when the
/// reply loop is about to hit the step cap (`MAX_TURNS`).
pub fn wind_down_message() -> Message {
    Message::user(
        "You are out of steps for this session. Do NOT start any new work, do NOT \
         call any tools. Reply with a concise hand-off summary covering exactly: \
         (1) what you completed, (2) what remains, and (3) the single most important \
         next action for whoever picks this up.",
    )
}

/// Build the soft-budget converge directive injected exactly once when the
/// reply loop's in-memory usage accumulator crosses the resolved soft threshold.
pub fn soft_budget_converge_message() -> Message {
    Message::user(
        "<system-reminder>\n\
         Budget for this session is mostly consumed. You should now CONVERGE on the \
         current task: stop expanding scope, commit or keep what already works, and \
         avoid opening new files, tools, or sub-tasks. Prepare to finish soon — if \
         you have a final summarize / hand-off / submit step available, prefer it. \
         If you cannot finish, leave the work in a state the next agent or human \
         can pick up cleanly.\n\
         </system-reminder>",
    )
}

/// Providers confirmed to handle `tool_choice: "required"` correctly,
/// even with reasoning/thinking enabled.
pub fn supports_tool_choice_required(model_id: &str) -> bool {
    let provider = model_id.split('/').next().unwrap_or("").to_lowercase();
    matches!(
        provider.as_str(),
        "openai" | "anthropic" | "chatgpt_codex" | "github_copilot"
    )
}

pub fn should_retry_empty_stream(
    saw_round_event: bool,
    empty_turn_retries: u32,
    is_codex: bool,
) -> Option<u32> {
    // Non-Codex providers fail immediately on the first empty/no-event stream.
    // Codex/OpenAI consumers signal over-quota by answering with an empty 200,
    // so the existing bounded retry lets us distinguish a one-off blip from a
    // real throttle.
    if !saw_round_event && empty_turn_retries < MAX_EMPTY_TURN_RETRIES && is_codex {
        Some(empty_turn_retries + 1)
    } else {
        None
    }
}

pub fn should_retry_empty_assistant_turn(
    assistant_content_is_empty: bool,
    empty_turn_retries: u32,
    is_codex: bool,
) -> Option<u32> {
    // Same rationale as `should_retry_empty_stream`: only Codex-family
    // providers use empty assistant turns as a throttle signal; other
    // providers should fail immediately.
    if assistant_content_is_empty && empty_turn_retries < MAX_EMPTY_TURN_RETRIES && is_codex {
        Some(empty_turn_retries + 1)
    } else {
        None
    }
}

/// Discriminate a **reasoning-only** turn from a quota **empty-200** when the
/// assembled assistant content is empty.
pub fn empty_turn_is_reasoning_only(reasoning_tokens_out: u32) -> bool {
    reasoning_tokens_out > 0
}

/// Build the nudge injected after a reasoning-only empty turn.
pub fn reasoning_only_nudge_message() -> Message {
    Message::user(
        "<system-reminder>\n\
         Your previous turn produced reasoning but NO visible output and NO tool \
         call, so nothing was delivered. Do not just think — this turn you MUST \
         either reply with your answer as text or call an appropriate tool to make \
         progress.\n\
         </system-reminder>",
    )
}

/// Backoff before retrying an empty / no-event provider turn.
pub fn empty_turn_backoff(retry: u32) -> std::time::Duration {
    let secs = 3u64.saturating_pow(retry.clamp(1, 3)).min(30);
    std::time::Duration::from_secs(secs)
}

pub fn should_retry_after_tool_call_compaction(compacted: bool, turn_has_tool_calls: bool) -> bool {
    compacted && turn_has_tool_calls
}

pub fn next_nudge_message(
    turn_has_tool_calls: bool,
    tools_are_available: bool,
    consecutive_nudge_count: u32,
    finalize_tool_names: &[&str],
) -> Result<Option<(u32, Message)>, anyhow::Error> {
    if turn_has_tool_calls || !tools_are_available {
        return Ok(None);
    }
    let finalize_list = finalize_tool_names.join("` or `");
    let next_count = consecutive_nudge_count + 1;
    if next_count >= MAX_NUDGE_ATTEMPTS {
        return Err(anyhow::anyhow!(
            "session failed: {} consecutive text-only responses without calling {}",
            next_count,
            finalize_list
        ));
    }
    Ok(Some((
        next_count,
        Message::user(format!(
            "You have not completed your session. You MUST call `{finalize_list}` \
             when you are done. If you still have work to do, use the appropriate tools \
             to continue. If you are done, call one of these tools now."
        )),
    )))
}

pub fn tool_choice_for_turn(model_id: &str, tools: &[serde_json::Value]) -> Option<ToolChoice> {
    if tools.is_empty() {
        None
    } else if supports_tool_choice_required(model_id) {
        Some(ToolChoice::Required)
    } else {
        Some(ToolChoice::Auto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_turn_backoff_increases_then_caps() {
        assert_eq!(empty_turn_backoff(1), std::time::Duration::from_secs(3));
        assert_eq!(empty_turn_backoff(2), std::time::Duration::from_secs(9));
        assert_eq!(empty_turn_backoff(3), std::time::Duration::from_secs(27));
        assert_eq!(empty_turn_backoff(0), std::time::Duration::from_secs(3));
        assert_eq!(empty_turn_backoff(10), std::time::Duration::from_secs(27));
    }
    #[test]
    fn empty_start_streak_feeds_breaker_only_before_first_progress() {
        // An empty-first-turns session (no productive turn yet) trips at the
        // threshold, so the breaker is fed without waiting for the 30-min stall.
        assert!(!empty_start_streak_feeds_breaker(0, false));
        assert!(!empty_start_streak_feeds_breaker(
            EMPTY_START_TURNS_BEFORE_BREAKER - 1,
            false
        ));
        assert!(empty_start_streak_feeds_breaker(
            EMPTY_START_TURNS_BEFORE_BREAKER,
            false
        ));
        assert!(empty_start_streak_feeds_breaker(
            EMPTY_START_TURNS_BEFORE_BREAKER + 5,
            false
        ));
    }
    #[test]
    fn empty_start_streak_never_trips_after_a_productive_turn() {
        // A single empty turn mid-session (after real progress) must NOT fail the
        // session over — even a large empty streak is ignored once the model has
        // produced something. Models recover; a working session must not be
        // killed by a lone mid-run empty turn.
        assert!(!empty_start_streak_feeds_breaker(1, true));
        assert!(!empty_start_streak_feeds_breaker(
            EMPTY_START_TURNS_BEFORE_BREAKER,
            true
        ));
        assert!(!empty_start_streak_feeds_breaker(100, true));
    }
    #[test]
    fn empty_turn_reasoning_only_discriminator() {
        assert!(empty_turn_is_reasoning_only(1));
        assert!(empty_turn_is_reasoning_only(512));
        assert!(!empty_turn_is_reasoning_only(0));
        let msg = reasoning_only_nudge_message();
        let text: String = msg
            .content
            .iter()
            .filter_map(djinn_provider::message::ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("NO visible output"), "{text}");
        assert!(text.contains("system-reminder"), "{text}");
    }
    #[test]
    fn tool_choice_auto_for_unsupported_providers() {
        assert!(!supports_tool_choice_required("synthetic/Kimi-K2.5"));
        assert!(!supports_tool_choice_required("synthetic/GLM-4.7"));
        assert!(!supports_tool_choice_required("deepinfra/some-model"));
        assert!(supports_tool_choice_required("openai/gpt-5.4"));
        assert!(supports_tool_choice_required("anthropic/claude-sonnet-4-5"));
        assert!(supports_tool_choice_required("chatgpt_codex/codex-mini"));
    }
    #[test]
    fn should_retry_empty_stream_codex_allows_retries() {
        // Codex gets bounded retries.
        assert_eq!(
            should_retry_empty_stream(false, 0, true),
            Some(1),
            "Codex retry 0→1"
        );
        assert_eq!(
            should_retry_empty_stream(false, 1, true),
            Some(2),
            "Codex retry 1→2"
        );
        assert_eq!(
            should_retry_empty_stream(false, 2, true),
            None,
            "Codex exhausted retries → terminal"
        );
    }
    #[test]
    fn should_retry_empty_stream_non_codex_fails_immediately() {
        // Non-Codex providers fail on the first empty turn with no retries.
        assert_eq!(
            should_retry_empty_stream(false, 0, false),
            None,
            "non-Codex empty stream → immediate terminal failure"
        );
        assert_eq!(
            should_retry_empty_stream(false, 1, false),
            None,
            "non-Codex always terminal"
        );
    }
    #[test]
    fn should_retry_empty_assistant_turn_codex_allows_retries() {
        assert_eq!(should_retry_empty_assistant_turn(true, 0, true), Some(1));
        assert_eq!(should_retry_empty_assistant_turn(true, 1, true), Some(2));
        assert_eq!(
            should_retry_empty_assistant_turn(true, 2, true),
            None,
            "Codex exhausted → terminal"
        );
    }
    #[test]
    fn should_retry_empty_assistant_turn_non_codex_fails_immediately() {
        assert_eq!(
            should_retry_empty_assistant_turn(true, 0, false),
            None,
            "non-Codex empty assistant → immediate terminal failure"
        );
    }
    #[test]
    fn is_provider_failure_prose_detects_rate_limit_short_text() {
        assert!(is_provider_failure_prose(
            "Rate limit exceeded. Please try again later."
        ));
        assert!(is_provider_failure_prose(
            "You have exceeded your quota. Please check your billing."
        ));
        assert!(is_provider_failure_prose(
            "insufficient_quota: you need to add credits"
        ));
        assert!(is_provider_failure_prose("429 Too Many Requests"));
        assert!(is_provider_failure_prose(
            "Out of credits. Please top up your account."
        ));
        assert!(is_provider_failure_prose(
            "Account suspended due to unpaid balance."
        ));
        assert!(is_provider_failure_prose(
            "API key not valid. Please pass a valid API key."
        ));
        assert!(is_provider_failure_prose(
            "Service unavailable. Please try again."
        ));
    }
    #[test]
    fn is_provider_failure_prose_ignores_long_productive_text() {
        // Long assistant response that happens to mention "rate limit" should not be flagged.
        let long_text = format!(
            "I've analyzed the codebase and found several issues. First, the error handling \
             in the networking module doesn't properly handle rate limit scenarios from the \
             upstream provider. We need to implement retry logic with exponential backoff. \
             {} \
             Here's my plan to fix this:\n1. Add a retry wrapper\n2. Implement backoff\n3. Write tests",
            "Additional context that makes this a long response. "
        );
        assert!(!is_provider_failure_prose(&long_text));
    }
    #[test]
    fn is_provider_failure_prose_empty_is_false() {
        assert!(!is_provider_failure_prose(""));
        assert!(!is_provider_failure_prose("   "));
    }
}
