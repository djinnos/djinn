//! Shared per-user/per-model dispatch admission primitives.
//!
//! These primitives implement the concurrency-cap check and in-flight ledger
//! overlay used by **every** dispatch path — normal task dispatch, planner
//! escalation, and refinement tribunal dispatch. Centralising them here
//! ensures a single source of truth for cap semantics rather than a divergent
//! copy in each caller.
//!
//! The high-level `CoordinatorActor` admission methods that compose these
//! primitives (`check_user_model_admission`, `clear_inflight_dispatch`, etc.)
//! live in [`super::task_dispatch`] alongside the durable dispatch-state
//! machinery they depend on.

use std::collections::HashMap;

use djinn_core::models::ModelLane;

use crate::types::InflightDispatch;

fn record_user_cap_utilization(user: &str, model: &str, used: u32, cap: u32) {
    djinn_telemetry::dispatch::set_user_cap_utilization(user, model, used, cap);
}

/// Check whether a `(creator, model)` is under the per-user concurrency cap.
///
/// `running_by_user_model` must already include the in-flight ledger overlay
/// (see [`overlay_inflight_ledger`]). Returns `true` when the user has room for
/// one more session on `model` under `cap`. A zero cap is defensively treated
/// as one; API validation prevents new zero values from being persisted.
///
/// This is the single shared cap-check primitive. Normal multi-model dispatch
/// calls it directly to filter candidate lists; single-model callers (e.g.
/// refinement tribunal dispatch via `CoordinatorActor::check_user_model_admission`)
/// compose it with a fresh DB + ledger snapshot.
pub(crate) fn model_under_user_cap(
    running_by_user_model: &HashMap<(String, String), u32>,
    creator: &str,
    model: &str,
    cap: u32,
) -> bool {
    let used = running_by_user_model
        .get(&(creator.to_string(), model.to_owned()))
        .copied()
        .unwrap_or(0);
    #[cfg(test)]
    observe_dispatch_cap_count(
        DispatchCapObservationStage::CapConsidered,
        creator,
        model,
        used,
    );
    let cap = cap.max(1);
    record_user_cap_utilization(creator, model, used, cap);
    used < cap
}

/// Check whether a user has room for one more session in `lane`.
///
/// `None` preserves the pre-lane-limit behavior: no lane-specific ceiling is
/// enforced. A present cap is clamped to at least one as a final defensive
/// boundary; API and persistence validation reject zero before it reaches the
/// coordinator.
pub(crate) fn lane_under_user_cap(
    running_by_user_lane: &HashMap<(String, ModelLane), u32>,
    creator: &str,
    lane: ModelLane,
    cap: Option<u32>,
) -> bool {
    let Some(cap) = cap else {
        return true;
    };
    let used = running_by_user_lane
        .get(&(creator.to_string(), lane))
        .copied()
        .unwrap_or(0);
    used < cap.max(1)
}

/// Overlay the in-flight dispatch ledger onto the DB-seeded per-user running
/// counts by adding reservations that do not yet have a running session row.
///
/// Callers capture active task ids before reading aggregate counts, then
/// reconcile against that earlier snapshot. Usually the retained entries are
/// disjoint booting work. If a session row lands between the reads, its
/// reservation is intentionally added too (a conservative one-pass
/// double-count). Using `max` would undercount the common case of one old
/// running task plus one distinct booting task and could admit an overshoot.
pub(crate) fn overlay_inflight_ledger(
    running_by_user_model: &mut HashMap<(String, String), u32>,
    inflight_dispatches: &HashMap<String, InflightDispatch>,
) {
    for dispatch in inflight_dispatches.values() {
        if let Some(creator) = dispatch.creator.as_ref() {
            *running_by_user_model
                .entry((creator.clone(), dispatch.model.clone()))
                .or_insert(0) += 1;
        }
    }
    #[cfg(test)]
    observe_dispatch_cap_counts(
        DispatchCapObservationStage::LedgerOverlay,
        running_by_user_model,
    );
}

/// Overlay the same reconciled in-flight ledger onto DB-seeded lane counts.
pub(crate) fn overlay_inflight_lane_ledger(
    running_by_user_lane: &mut HashMap<(String, ModelLane), u32>,
    inflight_dispatches: &HashMap<String, InflightDispatch>,
) {
    for dispatch in inflight_dispatches.values() {
        if let Some(creator) = dispatch.creator.as_ref() {
            *running_by_user_lane
                .entry((creator.clone(), dispatch.lane))
                .or_insert(0) += 1;
        }
    }
}

// ─── Test-only cap-count instrumentation ──────────────────────────────────
//
// Production dispatch behavior is unchanged: in non-test builds the observer
// types and recording calls are not compiled. Tests can clear the shared sink,
// drive a dispatch pass (or the ledger helper directly), then take the ordered
// observations to assert instantaneous per-user/per-model counts.

/// Test-only cap-count instrumentation for wnd1 stress coverage.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchCapObservation {
    pub creator_user_id: String,
    pub model: String,
    pub effective_count: u32,
    pub stage: DispatchCapObservationStage,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DispatchCapObservationStage {
    /// Count after DB-seeded running rows have been overlaid with the in-flight
    /// ledger by adding reconciled, not-yet-running reservations.
    LedgerOverlay,
    /// Count consulted by the per-user cap gate for a candidate model.
    CapConsidered,
    /// Count immediately after a successful dispatch increments local state and
    /// records an in-flight ledger entry, before any session row may exist.
    InflightIncremented,
}

#[cfg(test)]
static DISPATCH_CAP_OBSERVATIONS: std::sync::Mutex<Vec<DispatchCapObservation>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn clear_dispatch_cap_observations() {
    DISPATCH_CAP_OBSERVATIONS
        .lock()
        .expect("dispatch cap observations mutex poisoned")
        .clear();
}

#[cfg(test)]
pub(crate) fn take_dispatch_cap_observations() -> Vec<DispatchCapObservation> {
    std::mem::take(
        &mut *DISPATCH_CAP_OBSERVATIONS
            .lock()
            .expect("dispatch cap observations mutex poisoned"),
    )
}

#[cfg(test)]
pub(crate) fn observe_dispatch_cap_count(
    stage: DispatchCapObservationStage,
    creator_user_id: &str,
    model: &str,
    effective_count: u32,
) {
    DISPATCH_CAP_OBSERVATIONS
        .lock()
        .expect("dispatch cap observations mutex poisoned")
        .push(DispatchCapObservation {
            creator_user_id: creator_user_id.to_owned(),
            model: model.to_owned(),
            effective_count,
            stage,
        });
}

#[cfg(test)]
pub(crate) fn observe_dispatch_cap_counts(
    stage: DispatchCapObservationStage,
    running_by_user_model: &HashMap<(String, String), u32>,
) {
    let mut counts: Vec<_> = running_by_user_model.iter().collect();
    counts.sort_by(|((creator_a, model_a), _), ((creator_b, model_b), _)| {
        creator_a.cmp(creator_b).then_with(|| model_a.cmp(model_b))
    });
    for ((creator, model), count) in counts {
        observe_dispatch_cap_count(stage, creator, model, *count);
    }
}
