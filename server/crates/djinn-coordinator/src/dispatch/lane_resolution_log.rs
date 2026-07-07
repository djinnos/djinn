//! Structured logging for lane-resolution candidate ordering and failover
//! candidate traversal.
//!
//! Emits one `info`-level log record per candidate after final de-duplication
//! and before capacity filtering / dispatch attempts, so post-apply and
//! post-rollback model order can be inspected without production-only tooling.
//!
//! Also emits per-candidate attempt records during failover-chain traversal so
//! each tried candidate (and its outcome) is visible for observability.

/// Split a `provider/model` id into `(provider_id, model_id)`.
///
/// Returns the full string as the model_id with an empty provider_id when no
/// slash is present (tolerates legacy bare model names).
pub(crate) fn parse_provider_model(full_id: &str) -> (&str, &str) {
    match full_id.split_once('/') {
        Some((provider, model)) => (provider, model),
        None => ("", full_id),
    }
}

/// Last-resort models that should only be used as final fallback candidates.
///
/// Covers both spellings of the MiniMax subscription provider
/// (`minimax-coding-plan` used in catalog / credential code and `minimax`
/// used elsewhere).
pub(crate) fn is_last_resort(full_id: &str) -> bool {
    matches!(
        full_id,
        "kimi-for-coding/k2p7" | "minimax-coding-plan/MiniMax-M3" | "minimax/MiniMax-M3"
    )
}

/// Emit one structured `info` log per candidate in the final ordered list.
///
/// Called after de-duplication, rotation exclusion, and diverse-review
/// reordering but before capacity/health filtering and dispatch attempts.
pub(crate) fn emit_lane_resolution_candidates(
    task_id: &str,
    role: &str,
    tenant_id: &str,
    candidates: &[String],
) {
    for (idx, model) in candidates.iter().enumerate() {
        let (provider_id, model_id) = parse_provider_model(model);
        tracing::info!(
            task_id,
            role,
            tenant_id,
            candidate_index = idx,
            provider_id,
            model_id,
            last_resort = is_last_resort(model),
            "lane_resolution_candidate"
        );
    }
}

/// Outcome of a single failover-candidate dispatch attempt.
#[derive(Clone, Debug)]
pub(crate) enum CandidateAttemptOutcome {
    /// The candidate breaker was open (health tracker unavailable).
    BreakerOpen,
    /// The candidate was at capacity.
    AtCapacity,
    /// The dispatch returned an error.
    Error(String),
}

impl std::fmt::Display for CandidateAttemptOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BreakerOpen => write!(f, "breaker_open"),
            Self::AtCapacity => write!(f, "at_capacity"),
            Self::Error(msg) => write!(f, "error: {msg}"),
        }
    }
}

/// Emit a structured `warn`-level log for a single failover-candidate attempt
/// that did NOT succeed.  Called during failover-chain traversal when a
/// candidate is skipped (breaker/capacity) or fails (dispatch error), so each
/// tried candidate is visible for observability.
pub(crate) fn emit_failover_candidate_attempt(
    task_id: &str,
    role: &str,
    candidate_model: &str,
    candidate_index: usize,
    total_candidates: usize,
    attempt_outcome: &CandidateAttemptOutcome,
) {
    let (provider_id, model_id) = parse_provider_model(candidate_model);
    tracing::warn!(
        task_id,
        role,
        candidate_index,
        total_candidates,
        provider_id,
        model_id,
        outcome = %attempt_outcome,
        "failover_candidate_attempt"
    );
}

/// Emit a structured `info`-level log when a failover candidate succeeds.
/// Called during failover-chain traversal when the first candidate that
/// accepts the dispatch is found.
pub(crate) fn emit_failover_candidate_accepted(
    task_id: &str,
    role: &str,
    candidate_model: &str,
    candidate_index: usize,
    total_candidates: usize,
    skipped_count: usize,
) {
    let (provider_id, model_id) = parse_provider_model(candidate_model);
    tracing::info!(
        task_id,
        role,
        candidate_index,
        total_candidates,
        skipped_count,
        provider_id,
        model_id,
        "failover_candidate_accepted"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_provider_model ────────────────────────────────────────────

    #[test]
    fn parse_provider_model_splits_slash_separated_ids() {
        assert_eq!(
            parse_provider_model("kimi-for-coding/k2p7"),
            ("kimi-for-coding", "k2p7"),
        );
        assert_eq!(
            parse_provider_model("minimax-coding-plan/MiniMax-M3"),
            ("minimax-coding-plan", "MiniMax-M3"),
        );
        assert_eq!(
            parse_provider_model("xiaomi/mimo-v2.5-pro"),
            ("xiaomi", "mimo-v2.5-pro"),
        );
        assert_eq!(parse_provider_model("zai/glm-5.2"), ("zai", "glm-5.2"),);
    }

    #[test]
    fn parse_provider_model_tolerates_bare_model_name() {
        assert_eq!(parse_provider_model("gpt-4o"), ("", "gpt-4o"));
    }

    #[test]
    fn parse_provider_model_tolerates_empty_string() {
        assert_eq!(parse_provider_model(""), ("", ""));
    }

    // ── is_last_resort ──────────────────────────────────────────────────

    #[test]
    fn is_last_resort_marks_kimi_k2p7() {
        assert!(is_last_resort("kimi-for-coding/k2p7"));
    }

    #[test]
    fn is_last_resort_marks_minimax_coding_plan() {
        assert!(is_last_resort("minimax-coding-plan/MiniMax-M3"));
    }

    #[test]
    fn is_last_resort_tolerates_bare_minimax_spelling() {
        assert!(is_last_resort("minimax/MiniMax-M3"));
    }

    #[test]
    fn is_last_resort_does_not_flag_primary_models() {
        assert!(!is_last_resort("xiaomi/mimo-v2.5-pro"));
        assert!(!is_last_resort("zai/glm-5.2"));
    }

    #[test]
    fn is_last_resort_does_not_flag_other_kimi_models() {
        assert!(!is_last_resort("kimi-for-coding/k2p5"));
    }

    #[test]
    fn is_last_resort_does_not_flag_other_minimax_models() {
        assert!(!is_last_resort("minimax-coding-plan/MiniMax-M2.5"));
    }

    // ── emit_lane_resolution_candidates ─────────────────────────────────

    #[test]
    fn emit_lane_resolution_candidates_does_not_panic() {
        // Smoke test: ensure the logging path doesn't panic with an empty
        // or populated candidate list.  Real assertion is via structured-log
        // subscriber capture in integration tests if/when available.
        emit_lane_resolution_candidates("t1", "worker", "user-1", &[]);
        emit_lane_resolution_candidates(
            "t2",
            "reviewer",
            "user-2",
            &[
                "xiaomi/mimo-v2.5-pro".to_owned(),
                "zai/glm-5.2".to_owned(),
                "kimi-for-coding/k2p7".to_owned(),
            ],
        );
    }

    // ── emit_failover_candidate_attempt ─────────────────────────────────

    #[test]
    fn emit_failover_candidate_attempt_does_not_panic() {
        emit_failover_candidate_attempt(
            "t1",
            "worker",
            "xiaomi/mimo-v2.5-pro",
            0,
            3,
            &CandidateAttemptOutcome::BreakerOpen,
        );
        emit_failover_candidate_attempt(
            "t2",
            "worker",
            "zai/glm-5.2",
            1,
            3,
            &CandidateAttemptOutcome::AtCapacity,
        );
        emit_failover_candidate_attempt(
            "t3",
            "worker",
            "kimi-for-coding/k2p7",
            2,
            3,
            &CandidateAttemptOutcome::Error("pool dispatch failed".to_owned()),
        );
    }

    #[test]
    fn candidate_attempt_outcome_display() {
        assert_eq!(
            CandidateAttemptOutcome::BreakerOpen.to_string(),
            "breaker_open"
        );
        assert_eq!(
            CandidateAttemptOutcome::AtCapacity.to_string(),
            "at_capacity"
        );
        assert_eq!(
            CandidateAttemptOutcome::Error("timeout".to_owned()).to_string(),
            "error: timeout"
        );
    }

    // ── emit_failover_candidate_accepted ────────────────────────────────

    #[test]
    fn emit_failover_candidate_accepted_does_not_panic() {
        emit_failover_candidate_accepted("t1", "worker", "zai/glm-5.2", 1, 3, 1);
        emit_failover_candidate_accepted("t2", "worker", "xiaomi/mimo-v2.5-pro", 0, 3, 0);
    }
}
