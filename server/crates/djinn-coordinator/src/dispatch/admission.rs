//! Shared per-user/per-model dispatch admission primitives.
//!
//! These primitives implement the concurrency-cap check and in-flight ledger
//! overlay used by **every** dispatch path — normal task dispatch, planner
//! escalation, and (once wired in a follow-up task) refinement tribunal
//! dispatch. Centralising them here ensures a single source of truth for cap
//! semantics rather than a divergent copy in each caller.
//!
//! The high-level `CoordinatorActor` admission methods that compose these
//! primitives (`check_user_model_admission`, `clear_inflight_dispatch`, etc.)
//! live in [`super::task_dispatch`] alongside the durable dispatch-state
//! machinery they depend on.

use std::collections::HashMap;

fn record_user_cap_utilization(user: &str, model: &str, used: u32, cap: u32) {
    djinn_telemetry::dispatch::set_user_cap_utilization(user, model, used, cap);
}

/// Check whether a `(creator, model)` is under the per-user concurrency cap.
///
/// `running_by_user_model` must already include the in-flight ledger overlay
/// (see [`overlay_inflight_ledger`]). Returns `true` when the user has room for
/// one more session on `model` under `cap` (always `true` when `cap` is 0,
/// which is clamped to 1 internally).
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

/// Overlay the in-flight dispatch ledger onto the DB-seeded per-user running
/// counts, taking `max(db, ledger)` per `(creator, model)`.
///
/// The DB seed counts only sessions that reached `running`, which lags a fresh
/// dispatch by the worker pod's boot time (20-60s). The ledger holds dispatches
/// that have not yet produced a `running` row, so overlaying it makes those
/// count against the per-user cap immediately and prevents re-firing passes from
/// overshooting it. `max` (not sum) is deliberate: a task present in BOTH the
/// running rows and the ledger must count once, not twice.
pub(crate) fn overlay_inflight_ledger(
    running_by_user_model: &mut HashMap<(String, String), u32>,
    inflight_dispatches: &HashMap<String, (Option<String>, String)>,
) {
    let mut ledger_counts: HashMap<(String, String), u32> = HashMap::new();
    for (creator, model) in inflight_dispatches.values() {
        if let Some(c) = creator {
            *ledger_counts.entry((c.clone(), model.clone())).or_insert(0) += 1;
        }
    }
    for (key, lcount) in ledger_counts {
        let entry = running_by_user_model.entry(key).or_insert(0);
        *entry = (*entry).max(lcount);
    }
    #[cfg(test)]
    observe_dispatch_cap_counts(
        DispatchCapObservationStage::LedgerOverlay,
        running_by_user_model,
    );
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
    /// ledger via `max(db_count, ledger_count)`.
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
