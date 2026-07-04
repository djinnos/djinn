#[test]
fn model_lane_for_role_untouched() {
    use djinn_core::models::ModelLane;
    assert_eq!(ModelLane::for_role("worker"), ModelLane::Implement);
    assert_eq!(ModelLane::for_role("reviewer"), ModelLane::Review);
    assert_eq!(ModelLane::for_role("planner"), ModelLane::Plan);
    assert_eq!(ModelLane::for_role("architect"), ModelLane::Plan);
    assert_eq!(ModelLane::for_role("chat"), ModelLane::Plan);
    assert_eq!(ModelLane::for_role("lead"), ModelLane::Plan);
    assert_eq!(ModelLane::for_role("unknown"), ModelLane::Plan);
}

#[test]
fn use_plan_lane_for_post_intervention_workers_defaults_on() {
    use crate::dispatch::post_intervention_lane::use_plan_lane_for_post_intervention_workers;
    assert!(use_plan_lane_for_post_intervention_workers());
}
