//! The leader-side guarded enforcement pass (task `5mqp`, Phase D).
//!
//! This runs inside the coordinator actor, which exists only after
//! `server/src/leadership.rs::run_with_leadership` wins the database advisory
//! lock, and it reuses `wnrd`'s controller/reaper seam rather than adding a
//! second leader tick.
//!
//! What it does *not* do is as important as what it does. It reads the same
//! expected-path projection and the same fail-closed window qualifier the
//! Phase-C controller already computed; it recomputes no coverage of its own.
//! It touches only `model_turn_*` rows. It never advances a pool on its own
//! authority: every advance goes through
//! [`ModelTurnAdmissionRepository::apply_enforcement_pass_in_transaction`],
//! which re-checks the leadership fence and re-observes coverage inside the
//! transaction that mutates.
//!
//! **The training gap is deliberate and load-bearing.** Phase B stored a
//! capability *instant* rather than a coverage interval, and no authoritative
//! usage column, so `qualify_aligned_phase_c_window_v1` reports
//! `PartialCapabilityCoverage` / `MissingUsage` for every production window.
//! `window_trainable` is therefore false in production and no pool reaches
//! `enforce`. That is the fail-closed answer, not a defect of this pass:
//! widening the heartbeat instant or defaulting the usage would forge the very
//! evidence the enforcement decision rests on.

use std::collections::BTreeMap;

use djinn_db::{
    ModelTurnAdmissionRepository, ModelTurnControllerFence, ModelTurnEnforcementOutcome,
    ModelTurnEnforcementPassInput, ModelTurnExpectedPathKey,
};

use super::ExpectedAttemptPathProjectionV1;
use super::controller::{
    EVIDENCE_PAGE_LIMIT, PHASE_C_FENCE_LIVENESS_FLOOR, last_completed_window_v1,
    project_dispatch_topology_paths_v1, window_bounds_v1,
};

/// What one enforcement pass did, per pool. Counts and bounded codes only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnforcementPassOutcomeV1 {
    /// Pools this pass moved to `draining` because coverage was lost.
    pub drained_pools: Vec<i64>,
    /// Pools this pass advanced to `enforce`.
    pub enforced_pools: Vec<i64>,
    /// `(pool_id, bounded denial code)` for every refused advance.
    pub denials: Vec<(i64, &'static str)>,
    /// True when the durable fence refused: leadership was lost or superseded
    /// and nothing at all was mutated.
    pub fenced: bool,
}

/// Group a projection's expected paths by the pool that owns them.
#[must_use]
pub fn expected_paths_by_pool_v1(
    projection: &ExpectedAttemptPathProjectionV1,
) -> BTreeMap<i64, Vec<ModelTurnExpectedPathKey>> {
    let mut by_pool: BTreeMap<i64, Vec<ModelTurnExpectedPathKey>> = BTreeMap::new();
    for path in &projection.expected_paths {
        // The route fields are private to the parent module on purpose: only a
        // coordinator-resolved pool route may become an expected path, and a
        // report may never invent one. This child module reads them the way
        // production builds them.
        by_pool
            .entry(path.pool_id)
            .or_default()
            .push(ModelTurnExpectedPathKey {
                slot_pod_uid: path.slot_pod_uid.clone(),
                deployment_revision: path.deployment_revision.clone(),
            });
    }
    by_pool
}

/// Run the guarded enforcement decision for every pool in the projection.
///
/// One transaction per pool, so a pool that lost coverage drains without
/// touching any other pool's mode or `learned_concurrency`. The first fenced
/// pool stops the pass: a stale generation may not mutate anything.
pub async fn run_enforcement_pass_v1(
    repository: &ModelTurnAdmissionRepository,
    fence: &ModelTurnControllerFence,
    controller_generation: i64,
    evaluated_at: &str,
    expected_paths: &BTreeMap<i64, Vec<ModelTurnExpectedPathKey>>,
    window_trainable: bool,
) -> djinn_db::Result<EnforcementPassOutcomeV1> {
    let mut outcome = EnforcementPassOutcomeV1::default();
    for (pool_id, paths) in expected_paths {
        let applied = repository
            .apply_enforcement_pass_in_transaction(ModelTurnEnforcementPassInput {
                pool_id: *pool_id,
                expected_paths: paths.clone(),
                evaluated_at: evaluated_at.to_owned(),
                fence: fence.clone(),
                controller_generation,
                window_trainable,
            })
            .await?;
        match applied {
            ModelTurnEnforcementOutcome::Fenced => {
                outcome.fenced = true;
                return Ok(outcome);
            }
            ModelTurnEnforcementOutcome::Drained { .. } => outcome.drained_pools.push(*pool_id),
            ModelTurnEnforcementOutcome::Enforced { .. } => outcome.enforced_pools.push(*pool_id),
            ModelTurnEnforcementOutcome::Denied(rejection) => {
                outcome.denials.push((*pool_id, rejection.code()));
            }
            ModelTurnEnforcementOutcome::Unchanged { .. }
            | ModelTurnEnforcementOutcome::PoolUnavailable => {}
        }
    }
    Ok(outcome)
}

impl crate::CoordinatorActor {
    /// One guarded enforcement pass, run from the leader tick.
    ///
    /// Cancellation is leadership loss in progress: the token is the actor's
    /// own, cancelled before the advisory lock can be released. A cancelled
    /// pass performs no read and no mode mutation, so the last persisted mode
    /// stands exactly as it is.
    pub(crate) async fn run_model_turn_enforcement_pass(&mut self) {
        if self.cancel.is_cancelled() {
            return;
        }
        let Some(inventory) = self.workload_inventory.clone() else {
            return;
        };
        let now_second = ::time::OffsetDateTime::now_utc().unix_timestamp();
        let Some(window) = last_completed_window_v1(now_second) else {
            return;
        };
        let Some((_started_at, ended_at)) = window_bounds_v1(window) else {
            return;
        };
        let repository = ModelTurnAdmissionRepository::new(self.db.clone());
        let pools = match repository.list_observable_pools(EVIDENCE_PAGE_LIMIT).await {
            Ok(pools) => pools,
            Err(error) => {
                tracing::warn!(%error, "model-turn enforcement pass could not read its topology");
                return;
            }
        };
        if pools.is_empty() {
            return;
        }
        let records = match inventory.list().await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "model-turn enforcement pass could not read live slots");
                return;
            }
        };
        let projection = project_dispatch_topology_paths_v1(&self.catalog, &records, &pools);
        let by_pool = expected_paths_by_pool_v1(&projection);
        if by_pool.is_empty() {
            return;
        }
        // The verdict is `wnrd`'s, recorded by the controller cycle that ran
        // earlier in this same tick. This pass does not re-qualify anything.
        let window_trainable = self.last_phase_c_window_trainable;
        let fence = self.model_turn_controller_fence(PHASE_C_FENCE_LIVENESS_FLOOR.to_owned());
        match run_enforcement_pass_v1(
            &repository,
            &fence,
            self.model_turn_controller_generation(),
            &ended_at,
            &by_pool,
            window_trainable,
        )
        .await
        {
            Ok(outcome) => {
                if !outcome.drained_pools.is_empty()
                    || !outcome.enforced_pools.is_empty()
                    || outcome.fenced
                {
                    // Counts and bounded codes only: no pool identity, no slot
                    // uid, no revision, no credential or request identifier.
                    tracing::info!(
                        drained = outcome.drained_pools.len(),
                        enforced = outcome.enforced_pools.len(),
                        denied = outcome.denials.len(),
                        fenced = outcome.fenced,
                        "model-turn enforcement pass"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, "model-turn enforcement pass failed");
            }
        }
    }
}

#[cfg(test)]
#[path = "model_turn_admission_enforcement_tests.rs"]
mod tests;
