//! Structured logging for lane-resolution candidate ordering.
//!
//! Emits one `info`-level log record per candidate after final de-duplication
//! and before capacity filtering / dispatch attempts, so post-apply and
//! post-rollback model order can be inspected without production-only tooling.

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
}
