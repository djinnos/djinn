/// Default-on configuration: when `true`, post-intervention (`intervention_count >= 1`)
/// worker dispatches resolve their per-user model list from the plan lane
/// instead of the implement lane. Can be disabled via the env var
/// `DJINN_USE_PLAN_LANE_FOR_POST_INTERVENTION_WORKERS=0|false|no|off`.
const ENV_USE_PLAN_LANE_FOR_POST_INTERVENTION_WORKERS: &str =
    "DJINN_USE_PLAN_LANE_FOR_POST_INTERVENTION_WORKERS";

fn use_plan_lane_for_post_intervention_workers() -> bool {
    match std::env::var(ENV_USE_PLAN_LANE_FOR_POST_INTERVENTION_WORKERS) {
        Ok(val) => !matches!(
            val.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}
