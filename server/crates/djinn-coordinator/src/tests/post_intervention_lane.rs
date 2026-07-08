//! Coverage for the post-intervention plan-lane routing feature.
//!
//! AC1/AC2 (the routing *decision*) are exercised directly against the pure
//! [`effective_dispatch_lane`] seam. AC3 asserts `ModelLane::for_role` is
//! untouched. AC4's model-*resolution* override cannot be observed end-to-end
//! in this crate's `#[cfg(test)]` harness — see `resolution_override_*`
//! below for why, and where the production path is actually covered.

use crate::dispatch::post_intervention_lane::{
    effective_dispatch_lane, use_plan_lane_for_post_intervention_workers,
};
use djinn_core::models::ModelLane;

// ── AC3: ModelLane::for_role is unchanged (regression assertion) ────────────

#[test]
fn model_lane_for_role_untouched() {
    assert_eq!(ModelLane::for_role("worker"), ModelLane::Implement);
    assert_eq!(ModelLane::for_role("reviewer"), ModelLane::Review);
    assert_eq!(ModelLane::for_role("planner"), ModelLane::Plan);
    assert_eq!(ModelLane::for_role("architect"), ModelLane::Plan);
    assert_eq!(ModelLane::for_role("chat"), ModelLane::Plan);
    assert_eq!(ModelLane::for_role("lead"), ModelLane::Plan);
    assert_eq!(ModelLane::for_role("unknown"), ModelLane::Plan);
}

// ── Config: default-on feature flag ─────────────────────────────────────────

#[test]
fn use_plan_lane_for_post_intervention_workers_defaults_on() {
    // No env override in the default CI harness → feature is on.
    assert!(use_plan_lane_for_post_intervention_workers());
}

// ── AC1: post-intervention worker dispatches route to the plan lane ─────────

#[test]
fn post_intervention_worker_routes_to_plan_lane() {
    // role == "worker" && intervention_count >= 1 && feature on → plan lane.
    assert_eq!(
        effective_dispatch_lane("worker", 1, true),
        Some(ModelLane::Plan)
    );
    assert_eq!(
        effective_dispatch_lane("worker", 5, true),
        Some(ModelLane::Plan)
    );
}

// ── AC2: normal workers and other roles are unaffected ──────────────────────

#[test]
fn normal_worker_dispatch_keeps_implement_lane() {
    // intervention_count == 0 → no override; the caller falls back to
    // ModelLane::for_role("worker") == Implement.
    assert_eq!(effective_dispatch_lane("worker", 0, true), None);
}

#[test]
fn reviewer_and_planner_lanes_unchanged_even_post_intervention() {
    // Only the worker and lead roles are rerouted; reviewers, planners,
    // architects never get an override regardless of intervention_count.
    // (Lead is tested separately in `lead_arbiter_always_routes_to_plan_lane`.)
    for role in ["reviewer", "planner", "architect", "chat", "unknown"] {
        assert_eq!(
            effective_dispatch_lane(role, 0, true),
            None,
            "role {role} must not be rerouted"
        );
        assert_eq!(
            effective_dispatch_lane(role, 3, true),
            None,
            "role {role} must not be rerouted post-intervention"
        );
    }
}

#[test]
fn feature_disabled_leaves_post_intervention_worker_on_implement_lane() {
    // With the flag off, even a post-intervention worker resolves from its
    // role-implied (implement) lane — the override is fully gated on the flag.
    assert_eq!(effective_dispatch_lane("worker", 2, false), None);
}

// ── Lead arbiter always routes to plan lane ─────────────────────────────────

#[test]
fn lead_arbiter_always_routes_to_plan_lane() {
    // The park-rung forensic arbiter (role == "lead") always resolves through
    // the plan lane, regardless of feature flag or intervention count.
    assert_eq!(
        effective_dispatch_lane("lead", 0, true),
        Some(ModelLane::Plan),
        "lead with intervention_count=0 must route to plan lane"
    );
    assert_eq!(
        effective_dispatch_lane("lead", 1, true),
        Some(ModelLane::Plan),
        "lead with intervention_count=1 must route to plan lane"
    );
    assert_eq!(
        effective_dispatch_lane("lead", 5, true),
        Some(ModelLane::Plan),
        "lead with intervention_count=5 must route to plan lane"
    );
}

#[test]
fn lead_arbiter_routes_to_plan_lane_even_with_feature_disabled() {
    // Unlike workers, lead arbiter routing is not gated on the post-intervention
    // feature flag. The lead role is always an arbiter dispatch, not a worker
    // retry, so its plan-lane routing is unconditional.
    assert_eq!(
        effective_dispatch_lane("lead", 0, false),
        Some(ModelLane::Plan),
        "lead must route to plan lane even with feature disabled"
    );
    assert_eq!(
        effective_dispatch_lane("lead", 3, false),
        Some(ModelLane::Plan),
        "lead must route to plan lane even with feature disabled"
    );
}

// ── AC4: model-resolution override is out of reach for the in-crate harness ─
//
// The two resolution helpers that consume `effective_dispatch_lane`'s output
// are compiled to fixed stubs under `#[cfg(test)]`, so their production
// branches — the plan-lane override in `resolve_user_model_priority[_with_lane]`
// and the per-user fallback in `resolve_dispatch_models_for_role` — are
// unreachable when this crate is built for tests:
//
//   * `resolve_user_model_priority_with_lane` (dispatch/retry.rs) returns
//     `Vec::new()` unconditionally under `#[cfg(test)]` — it never reads
//     `UserSettings.lanes`, so the `effective_lane`-vs-`for_role` branch that
//     the override drives is compiled out. The override is therefore observable
//     only in the `#[cfg(not(test))]` body.
//
//   * `resolve_dispatch_models_for_role` (actor.rs) returns
//     `vec![DEFAULT_MODEL_ID]` unconditionally under `#[cfg(test)]`. Its
//     per-user fallback (`selected.is_empty() → resolve_user_model_priority`)
//     lives entirely in the `#[cfg(not(test))]` body, so the fallback path
//     cannot be entered from any in-crate test.
//
// These stubs exist deliberately: the dispatch fixtures seed no real users,
// credentials, or `UserSettings`, so a faithful resolution would need a live
// provider catalog and credential store that this unit harness does not stand
// up. The production resolution + override are exercised via the live
// MCP/session flow and the model-selection unit tests in the crates that own
// `UserSettings`/catalog resolution. What *is* faithfully testable in this
// crate — the routing decision that selects the plan lane — is covered above
// against the pure `effective_dispatch_lane` seam.

#[test]
fn resolution_override_seam_is_documented_not_observable_in_crate() {
    // A guard rather than a behavioral assertion: it pins the invariant this
    // module relies on — the routing decision is a pure function of
    // (role, intervention_count, feature_enabled) and does not itself touch the
    // cfg(test)-stubbed resolution helpers. If the decision ever grows a
    // dependency on user/credential state, this seam (and the note above) must
    // be revisited so AC4 regains real end-to-end coverage.
    assert_eq!(
        effective_dispatch_lane("worker", 1, true),
        Some(ModelLane::Plan)
    );
    assert_eq!(effective_dispatch_lane("worker", 0, true), None);
    // Lead arbiter is always plan-lane.
    assert_eq!(
        effective_dispatch_lane("lead", 0, true),
        Some(ModelLane::Plan)
    );
}
