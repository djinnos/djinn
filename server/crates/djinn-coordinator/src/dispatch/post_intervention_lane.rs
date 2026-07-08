/// Default-on configuration: when `true`, post-intervention (`intervention_count >= 1`)
/// worker dispatches resolve their per-user model list from the plan lane
/// instead of the implement lane. Can be disabled via the env var
/// `DJINN_USE_PLAN_LANE_FOR_POST_INTERVENTION_WORKERS=0|false|no|off`.
const ENV_USE_PLAN_LANE_FOR_POST_INTERVENTION_WORKERS: &str =
    "DJINN_USE_PLAN_LANE_FOR_POST_INTERVENTION_WORKERS";

pub(crate) fn use_plan_lane_for_post_intervention_workers() -> bool {
    match std::env::var(ENV_USE_PLAN_LANE_FOR_POST_INTERVENTION_WORKERS) {
        Ok(val) => !matches!(
            val.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// Pure lane-selection decision for a single dispatch.
///
/// Returns `Some(lane)` to override the lane that a user's per-model selection
/// would otherwise be resolved from, or `None` to leave the
/// `ModelLane::for_role(role)` mapping untouched.
///
/// Two roles receive an explicit plan-lane override:
///
/// 1. `role == "worker"` dispatches with `intervention_count >= 1` (a
///    post-intervention retry) are routed to the plan lane, and only when
///    `feature_enabled` (see [`use_plan_lane_for_post_intervention_workers`]) is
///    set.
///
/// 2. `role == "lead"` dispatches (the park-rung forensic arbiter) are always
///    routed to the plan lane. This is an explicit override that matches
///    `ModelLane::for_role("lead")` (`Plan`) but ensures the arbiter path is
///    visible in dispatch tracing and does not depend on the catch-all arm of
///    `for_role`.
///
/// Every other dispatch — normal workers (`intervention_count == 0`),
/// reviewers, planners, architects — returns `None` and is unaffected.
///
/// Kept pure (no env/DB reads) so the routing decision is unit-testable in this
/// crate's `#[cfg(test)]` harness; the caller supplies `feature_enabled` from
/// [`use_plan_lane_for_post_intervention_workers`].
pub(crate) fn effective_dispatch_lane(
    role: &str,
    intervention_count: i64,
    feature_enabled: bool,
) -> Option<djinn_core::models::ModelLane> {
    if (feature_enabled && role == "worker" && intervention_count >= 1) || role == "lead" {
        Some(djinn_core::models::ModelLane::Plan)
    } else {
        None
    }
}
