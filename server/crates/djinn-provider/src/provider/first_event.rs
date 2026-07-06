//! First-event (TTFT) budget derivation for SSE streams.
//!
//! After HTTP response headers arrive, a provider stream that never delivers
//! its first usable SSE `data:` event is wedged — the connection is open but
//! the model produced no output. This module derives the budget for the
//! transport-level first-event guard in [`super::client::ApiClient::stream_sse`]:
//!
//! - **Default non-reasoning budget:** 90 s. Stalled non-reasoning streams fail
//!   over fast instead of holding the turn open for minutes.
//! - **Per-model reasoning floor:** ~600 s via start-anchored slug matching.
//!   Reasoning models (OpenAI o-series/gpt-5, Anthropic Claude 3.7+, Google
//!   Gemini 2.5+, DeepSeek-R, QwQ) can legitimately take minutes before the
//!   first token while thinking.
//! - **Explicit config always wins** and is never lowered or overridden by a
//!   floor.
//! - **Floors never lower** an existing threshold — a floor only raises the
//!   default budget for matching model families.

use std::time::Duration;

/// Default first-event budget for non-reasoning models (90 s).
///
/// Kept at or below the liveness `zero_token_threshold_secs` so a stalled
/// non-reasoning stream fails over before the session-level wedge detector
/// would classify it as dead.
pub const DEFAULT_FIRST_EVENT_TIMEOUT: Duration = Duration::from_secs(90);

/// Per-model floor for reasoning families (600 s).
///
/// Reasoning models can spend minutes in extended thinking before emitting
/// their first token. The floor raises the default budget for known reasoning
/// families so the first-event guard does not fire prematurely.
pub const REASONING_FLOOR_TIMEOUT: Duration = Duration::from_secs(600);

/// Derive the first-event timeout budget for a model.
///
/// Precedence (floors never lower, explicit config never overridden):
///
/// 1. **Explicit config** (`Some(d)`) — always wins, even if lower than the
///    default or the floor. The caller is asserting they know better.
/// 2. **Per-model reasoning floor** — raises the default to
///    [`REASONING_FLOOR_TIMEOUT`] for known reasoning families via
///    start-anchored slug matching ([`is_reasoning_model_slug`]).
/// 3. **Default** ([`DEFAULT_FIRST_EVENT_TIMEOUT`]) for all other models.
pub fn first_event_budget(model_id: &str, explicit: Option<Duration>) -> Duration {
    match explicit {
        Some(d) => d,
        None => {
            if is_reasoning_model_slug(model_id) {
                REASONING_FLOOR_TIMEOUT
            } else {
                DEFAULT_FIRST_EVENT_TIMEOUT
            }
        }
    }
}

/// Start-anchored slug matching for reasoning model families.
///
/// Returns `true` when the model slug starts with a known reasoning-family
/// prefix. Matching is case-insensitive and anchored at the **start** of the
/// slug (the segment after the last `/`, so vendor-prefixed catalog IDs like
/// `google/gemini-2.5-pro` still match).
pub fn is_reasoning_model_slug(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    // Match on the slug after the last path separator so a vendor prefix
    // (e.g. "google/") never blocks a known family prefix.
    let slug = lower.rsplit('/').next().unwrap_or(&lower);
    REASONING_MODEL_PREFIXES
        .iter()
        .any(|prefix| slug.starts_with(prefix))
}

/// Known reasoning-family slug prefixes (start-anchored, case-insensitive).
///
/// These are model families that can legitimately spend minutes in extended
/// thinking before emitting their first token:
///
/// - OpenAI o-series and gpt-5 family (including Codex variants like
///   `gpt-5.1-codex`).
/// - Anthropic Claude 3.7+ (`claude-3-7-sonnet`, `claude-opus-4`,
///   `claude-sonnet-4`).
/// - Google Gemini 2.5+ (`gemini-2.5-pro`, `gemini-3-flash-preview`).
/// - DeepSeek Reasoner (`deepseek-r1`, `deepseek-r1-distill-*`).
/// - Qwen QwQ (`qwq-32b`, etc.).
const REASONING_MODEL_PREFIXES: &[&str] = &[
    // OpenAI reasoning families
    "o1",
    "o3",
    "o4",
    "gpt-5",
    // Anthropic reasoning-capable models (Claude 3.7+)
    "claude-3-7",
    "claude-opus-4",
    "claude-sonnet-4",
    // Google reasoning-capable models (Gemini 2.5+)
    "gemini-2.5",
    "gemini-3",
    // DeepSeek Reasoner
    "deepseek-r",
    // Qwen QwQ reasoning model
    "qwq",
];

#[cfg(test)]
mod tests {
    use super::*;

    // ── Default budget lowering ────────────────────────────────────────────

    #[test]
    fn default_budget_for_non_reasoning_model_is_90s() {
        assert_eq!(
            first_event_budget("gpt-4o", None),
            DEFAULT_FIRST_EVENT_TIMEOUT
        );
        assert_eq!(DEFAULT_FIRST_EVENT_TIMEOUT, Duration::from_secs(90));
    }

    #[test]
    fn default_budget_for_empty_model_id_is_90s() {
        assert_eq!(first_event_budget("", None), DEFAULT_FIRST_EVENT_TIMEOUT);
    }

    #[test]
    fn default_budget_no_more_than_90s() {
        // The non-reasoning default must be within 90s per AC.
        assert!(
            DEFAULT_FIRST_EVENT_TIMEOUT.as_secs() <= 90,
            "default first-event budget must be <= 90s, got {:?}",
            DEFAULT_FIRST_EVENT_TIMEOUT
        );
    }

    // ── Explicit config precedence ─────────────────────────────────────────

    #[test]
    fn explicit_config_overrides_default() {
        // Explicit config raises above the 90s default.
        assert_eq!(
            first_event_budget("gpt-4o", Some(Duration::from_secs(120))),
            Duration::from_secs(120)
        );
    }

    #[test]
    fn explicit_config_overrides_reasoning_floor() {
        // Explicit config always wins, even when lower than the 600s floor.
        assert_eq!(
            first_event_budget("o1-mini", Some(Duration::from_secs(30))),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn explicit_config_lower_than_default_still_wins() {
        // The caller's explicit choice is never overridden.
        assert_eq!(
            first_event_budget("gpt-4o", Some(Duration::from_secs(10))),
            Duration::from_secs(10)
        );
    }

    // ── Start-anchored floor matching ──────────────────────────────────────

    #[test]
    fn reasoning_floor_provides_roughly_600s() {
        assert_eq!(REASONING_FLOOR_TIMEOUT, Duration::from_secs(600));
    }

    #[test]
    fn openai_reasoning_families_match_floor() {
        for id in [
            "o1",
            "o1-mini",
            "o1-pro",
            "o3",
            "o3-mini",
            "o4-mini",
            "gpt-5",
            "gpt-5.1-codex",
        ] {
            assert!(
                is_reasoning_model_slug(id),
                "{id:?} should match reasoning floor"
            );
            assert_eq!(
                first_event_budget(id, None),
                REASONING_FLOOR_TIMEOUT,
                "{id:?} should get 600s floor"
            );
        }
    }

    #[test]
    fn anthropic_reasoning_families_match_floor() {
        for id in [
            "claude-3-7-sonnet-20250219",
            "claude-opus-4-20250514",
            "claude-sonnet-4-20250514",
        ] {
            assert!(
                is_reasoning_model_slug(id),
                "{id:?} should match reasoning floor"
            );
        }
    }

    #[test]
    fn google_reasoning_families_match_floor() {
        for id in [
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-3-pro-preview",
            "gemini-3-flash-preview",
            // Vendor-prefixed catalog IDs still match.
            "google/gemini-2.5-pro",
            "google/gemini-3-flash-preview",
        ] {
            assert!(
                is_reasoning_model_slug(id),
                "{id:?} should match reasoning floor"
            );
        }
    }

    #[test]
    fn deepseek_and_qwq_match_floor() {
        assert!(is_reasoning_model_slug("deepseek-r1"));
        assert!(is_reasoning_model_slug("qwq-32b"));
    }

    // ── Start-anchored floor non-matching ──────────────────────────────────

    #[test]
    fn non_reasoning_models_do_not_match_floor() {
        for id in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4-turbo",
            "claude-3-5-sonnet-20241022",
            "claude-3-opus-20240229",
            "gemini-1.5-pro",
            "deepseek-v3",
            "deepseek-chat",
            "llama-3.3-70b",
        ] {
            assert!(
                !is_reasoning_model_slug(id),
                "{id:?} should NOT match reasoning floor"
            );
            assert_eq!(
                first_event_budget(id, None),
                DEFAULT_FIRST_EVENT_TIMEOUT,
                "{id:?} should get 90s default"
            );
        }
    }

    #[test]
    fn floor_matching_is_case_insensitive() {
        assert!(is_reasoning_model_slug("O1-MINI"));
        assert!(is_reasoning_model_slug("GPT-5.1-CODEX"));
        assert!(is_reasoning_model_slug("Claude-Sonnet-4"));
        assert!(is_reasoning_model_slug("GEMINI-2.5-PRO"));
    }

    #[test]
    fn floor_matching_is_start_anchored_not_substring() {
        // "my-o1-finetune" must NOT match — the prefix must be at the start.
        assert!(!is_reasoning_model_slug("my-o1-finetune"));
        assert!(!is_reasoning_model_slug("custom-gpt-5-clone"));
        assert!(!is_reasoning_model_slug("notgemini-2.5-pro"));
    }
}
