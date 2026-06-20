use djinn_provider::message::Message;
use djinn_provider::provider::ToolChoice;
use djinn_provider::provider::error::ProviderError;
use std::fmt;

/// Maximum retries for empty assistant turns before treating as a hard failure.
pub(super) const MAX_EMPTY_TURN_RETRIES: u32 = 2;
/// Maximum consecutive text-only turns before treating as a session failure.
/// Each text-only turn without a finalize tool call triggers a nudge message.
pub(super) const MAX_NUDGE_ATTEMPTS: u32 = 3;
/// Maximum reactive compaction attempts before giving up.
pub(super) const MAX_COMPACTION_RETRIES: u32 = 2;

/// Typed reply-loop outcome for a budget-triggered wind-down whose one granted
/// final turn produced no hand-off text (typically because the model kept
/// calling tools). Stage settlement catches this before generic failure mapping
/// and deliberately parks the session instead of surfacing a provider/task fault.
#[derive(Debug, Clone)]
pub(crate) struct BudgetWindDownIgnored {
    pub(crate) details: String,
}

impl fmt::Display for BudgetWindDownIgnored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "budget wind-down ignored: {}", self.details)
    }
}

impl std::error::Error for BudgetWindDownIgnored {}

/// Build the wind-down directive injected on the final permitted turn when the
/// reply loop is about to hit the step cap (`MAX_TURNS`).
///
/// Rather than hard-erroring (which re-dispatches the task with no record of
/// what was accomplished), we give the agent exactly ONE more turn to produce a
/// hand-off summary so the next dispatch — or a human — has continuity. The
/// directive explicitly forbids starting new work so the turn stays bounded.
pub(super) fn wind_down_message() -> Message {
    Message::user(
        "You are out of steps for this session. Do NOT start any new work, do NOT \
         call any tools. Reply with a concise hand-off summary covering exactly: \
         (1) what you completed, (2) what remains, and (3) the single most important \
         next action for whoever picks this up.",
    )
}

/// Build the soft-budget converge directive injected exactly once when the
/// reply loop's in-memory usage accumulator crosses the resolved soft threshold
/// (default ~75% of `SessionBudgetPolicy::max_cumulative_tokens`).
///
/// This sits between the per-turn G9 step-cap check and the future hard-threshold
/// wind-down. It tells the agent to *converge*: commit/keep what already works,
/// stop expanding scope, and prepare to wrap the task up — so the eventual
/// hard-threshold wind-down captures a useful summary instead of mid-refactor
/// state. The body is wrapped in `<system-reminder>...</system-reminder>` so the
/// model recognizes it as a platform directive (matches the convention used by
/// the chat prompt layer, see `codebase_header::wrap_in_system_reminder`).
///
/// The caller is responsible for persisting/pushing this message exactly once
/// per session — the `<system-reminder>` does NOT confer one-shot semantics on
/// its own. Pair it with a boolean flag analogous to `wind_down_injected` so
/// later pre-turn iterations that still see the threshold exceeded don't
/// re-inject the same reminder.
pub(super) fn soft_budget_converge_message() -> Message {
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

pub(crate) fn is_context_length_error(e: &anyhow::Error) -> bool {
    // Prefer the typed provider taxonomy when present (set at the
    // provider-crate boundary), then fall back to substring matching for
    // resilience against untyped/legacy error paths.
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

/// Detect "No tool call found for function call output" errors from the OpenAI
/// Responses API. These happen when a `tool` role message references a
/// `tool_call_id` that doesn't exist in any preceding assistant message —
/// typically after compaction removed the assistant message but left orphaned
/// tool results. Also matches the inverse "No tool output found for function
/// call ..." which fires when an assistant function_call has no matching
/// tool_output entry (e.g. session was interrupted mid-turn).
pub(crate) fn is_orphaned_tool_call_error_str(msg: &str) -> bool {
    let msg = msg.to_lowercase();
    msg.contains("no tool call found for function call output")
        || msg.contains("no tool output found for function call")
        || msg.contains("no function call found")
}

pub(crate) fn is_orphaned_tool_call_error(e: &anyhow::Error) -> bool {
    is_orphaned_tool_call_error_str(&e.to_string())
}

/// Providers confirmed to handle `tool_choice: "required"` correctly,
/// even with reasoning/thinking enabled.
pub(super) fn supports_tool_choice_required(model_id: &str) -> bool {
    let provider = model_id.split('/').next().unwrap_or("").to_lowercase();
    matches!(
        provider.as_str(),
        "openai" | "anthropic" | "chatgpt_codex" | "github_copilot"
    )
}

pub(super) fn should_retry_empty_stream(
    saw_round_event: bool,
    empty_turn_retries: u32,
) -> Option<u32> {
    if !saw_round_event && empty_turn_retries < MAX_EMPTY_TURN_RETRIES {
        Some(empty_turn_retries + 1)
    } else {
        None
    }
}

pub(super) fn should_retry_empty_assistant_turn(
    assistant_content_is_empty: bool,
    empty_turn_retries: u32,
) -> Option<u32> {
    if assistant_content_is_empty && empty_turn_retries < MAX_EMPTY_TURN_RETRIES {
        Some(empty_turn_retries + 1)
    } else {
        None
    }
}

/// Discriminate a **reasoning-only** turn from a quota **empty-200** when the
/// assembled assistant content is empty (idea 5).
///
/// Both surface as "empty assistant turn", but they are opposite failure modes:
/// - All counts zero (`reasoning_tokens == 0`): a genuine empty-200. On the
///   Codex/OpenAI consumer backend this is the over-quota account answering with
///   an empty `response.completed` — a THROTTLE that should drive failover.
/// - `reasoning_tokens > 0` with no visible message and no tool call: the model
///   spent reasoning tokens but emitted nothing usable (a reasoning-only stall,
///   e.g. encrypted reasoning we couldn't surface, or summaries-off). This is NOT
///   a throttle — the backend answered, the model just didn't externalize a
///   result. The right response is a NUDGE asking it to emit its answer / call a
///   tool, not a failover.
///
/// Returns `true` when the empty turn is reasoning-only and should be nudged.
pub(super) fn empty_turn_is_reasoning_only(reasoning_tokens_out: u32) -> bool {
    reasoning_tokens_out > 0
}

/// Build the nudge injected after a reasoning-only empty turn (see
/// [`empty_turn_is_reasoning_only`]). It tells the model its visible output was
/// empty and asks it to actually emit the answer or call a tool this turn.
pub(super) fn reasoning_only_nudge_message() -> Message {
    Message::user(
        "<system-reminder>\n\
         Your previous turn produced reasoning but NO visible output and NO tool \
         call, so nothing was delivered. Do not just think — this turn you MUST \
         either reply with your answer as text or call an appropriate tool to make \
         progress.\n\
         </system-reminder>",
    )
}

/// Backoff before retrying an empty / no-event provider turn. A 200-empty
/// completion is a refusal/throttle signal, not a transient blip — notably the
/// ChatGPT-account Codex backend (`chatgpt.com/backend-api/codex`) signals
/// rate limiting by returning an empty `response.completed` (HTTP 200) instead
/// of a 429. Spacing retries out avoids hammering a degraded backend into a
/// deeper/longer throttle. Schedule: 3s, 9s, 27s, capped at 30s.
pub(super) fn empty_turn_backoff(retry: u32) -> std::time::Duration {
    let secs = 3u64.saturating_pow(retry.clamp(1, 3)).min(30);
    std::time::Duration::from_secs(secs)
}

pub(super) fn should_retry_after_tool_call_compaction(
    compacted: bool,
    turn_has_tool_calls: bool,
) -> bool {
    compacted && turn_has_tool_calls
}

pub(super) fn next_nudge_message(
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

pub(super) fn tool_choice_for_turn(
    model_id: &str,
    tools: &[serde_json::Value],
) -> Option<ToolChoice> {
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

    #[test]
    fn empty_turn_backoff_increases_then_caps() {
        assert_eq!(empty_turn_backoff(1), std::time::Duration::from_secs(3));
        assert_eq!(empty_turn_backoff(2), std::time::Duration::from_secs(9));
        assert_eq!(empty_turn_backoff(3), std::time::Duration::from_secs(27));
        // Defensive: clamps so an out-of-range retry never panics or runs away.
        assert_eq!(empty_turn_backoff(0), std::time::Duration::from_secs(3));
        assert_eq!(empty_turn_backoff(10), std::time::Duration::from_secs(27));
    }

    #[test]
    fn empty_turn_reasoning_only_discriminator() {
        // Idea 5: an empty assistant turn that spent reasoning tokens is a
        // reasoning-only stall (nudge), distinct from an all-zero quota empty-200
        // (throttle/failover). The reasoning-token count is the discriminator.
        assert!(
            empty_turn_is_reasoning_only(1),
            "reasoning tokens spent → reasoning-only stall (nudge path)"
        );
        assert!(empty_turn_is_reasoning_only(512));
        assert!(
            !empty_turn_is_reasoning_only(0),
            "all-zero empty turn → genuine empty-200 (throttle path)"
        );
        // The nudge body must steer the model to externalize output / call a tool.
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
}
