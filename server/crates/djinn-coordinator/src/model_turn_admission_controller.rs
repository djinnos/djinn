//! Fenced Phase-C controller cycle and persisted-timestamp lease reaper (wnrd).
//!
//! Both run only inside the coordinator actor, which exists only after
//! `run_with_leadership` wins the database advisory lock. Neither depends on a
//! process-local semaphore or a local timer: the controller's authority is the
//! durable coordinator-incarnation lease carried in
//! [`ModelTurnControllerFence`], and the reaper's input is entirely the
//! persisted `reserved_at`/`heartbeat_at` columns, so a successor resumes from
//! the database alone.

use std::collections::{BTreeMap, BTreeSet};

use djinn_db::{
    ModelTurnAdmissionRepository, ModelTurnControllerFence, ModelTurnLeaseMutationOutcome,
    ModelTurnPhaseCEvidence, ModelTurnPool,
};
use djinn_k8s::WorkloadRecord;
use djinn_provider::catalog::CatalogService;

use super::{
    AlignedPhaseCWindowV1, ExpectedAttemptPathProjectionV1, ExpectedAttemptPathV1,
    PhaseCAdmittedAttemptV1, PhaseCAttemptEvidenceOutcomeV1, PhaseCAttemptStageEvidenceV1,
    PhaseCAttemptStageV1, PhaseCCapabilityEvidenceV1, PhaseCWindowAccountingV1,
    PhaseCWindowDiagnosticCodeV1, PhaseCWindowQualificationV1, eligible_live_slot,
    persist_catalog_qualified_phase_c_window_v1, qualify_aligned_phase_c_window_v1,
};

/// Admitted and completed turn counts for one pool over one aligned window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhaseCWindowCountsV1 {
    pub admitted_turns: i64,
    pub completed_turns: i64,
}

/// What one completed-window controller cycle actually did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhaseCControllerCycleOutcomeV1 {
    /// Pools whose window row was committed under this fence.
    pub persisted_pools: Vec<i64>,
    /// True when the durable fence refused the write: a stale generation, a
    /// draining incarnation, or one that stopped renewing. The last persisted
    /// target is left exactly as it stands.
    pub fenced: bool,
    /// The window's qualification. Diagnostic windows are still persisted.
    pub qualification: PhaseCWindowQualificationV1,
    /// Enforcing pools this cycle moved to draining after coverage loss.
    pub drained_pools: Vec<i64>,
}

/// Codes that mean complete capability coverage did not hold for the full
/// aligned window. Every one of them is a coverage fact, not a chain fact.
#[must_use]
pub fn is_capability_coverage_loss_v1(code: PhaseCWindowDiagnosticCodeV1) -> bool {
    matches!(
        code,
        PhaseCWindowDiagnosticCodeV1::EmptyExpectedDenominator
            | PhaseCWindowDiagnosticCodeV1::MissingCapability
            | PhaseCWindowDiagnosticCodeV1::UnexpectedCapability
            | PhaseCWindowDiagnosticCodeV1::DuplicateCapability
            | PhaseCWindowDiagnosticCodeV1::UncoveredCapability
            | PhaseCWindowDiagnosticCodeV1::PartialCapabilityCoverage
            | PhaseCWindowDiagnosticCodeV1::StaleHeartbeat
    )
}

/// Everything one completed aligned window contributes to the controller.
pub struct PhaseCCompletedWindowV1<'a> {
    pub window: AlignedPhaseCWindowV1,
    pub started_at: String,
    pub ended_at: String,
    pub projection: &'a ExpectedAttemptPathProjectionV1,
    pub capability_evidence: &'a [PhaseCCapabilityEvidenceV1],
    pub admitted_attempts: &'a [PhaseCAdmittedAttemptV1],
    /// Per-pool counts observed over the window.
    pub counts: BTreeMap<i64, PhaseCWindowCountsV1>,
}

/// Run one completed-window controller cycle under the durable leadership fence.
///
/// The order is the contract. The authoritative denominator and evidence are
/// projected by the caller from live inventory; the fail-closed qualifier runs
/// over them; the typed verdict is written through the catalog-qualified
/// persistence seam under the fence; and only a write that actually committed
/// may go on to drain a pool. A fenced cycle mutates nothing at all.
pub async fn run_completed_window_cycle_v1(
    repository: &ModelTurnAdmissionRepository,
    catalog: &CatalogService,
    fence: &ModelTurnControllerFence,
    completed: &PhaseCCompletedWindowV1<'_>,
    controller_generation: i64,
) -> djinn_db::Result<PhaseCControllerCycleOutcomeV1> {
    let qualification = qualify_aligned_phase_c_window_v1(
        completed.window,
        &completed.projection.expected_paths,
        completed.capability_evidence,
        completed.admitted_attempts,
    );
    let window_sequence = completed
        .window
        .start_second()
        .div_euclid(AlignedPhaseCWindowV1::SECONDS);

    // One row per owning pool, deduplicated: several slots may share a route.
    let mut by_pool: BTreeMap<i64, &ExpectedAttemptPathV1> = BTreeMap::new();
    for path in &completed.projection.expected_paths {
        by_pool.entry(path.pool_id).or_insert(path);
    }

    let mut outcome = PhaseCControllerCycleOutcomeV1 {
        qualification: qualification.clone(),
        ..Default::default()
    };
    for (pool_id, path) in by_pool {
        let counts = completed.counts.get(&pool_id).copied().unwrap_or_default();
        let applied = persist_catalog_qualified_phase_c_window_v1(
            repository,
            catalog,
            path,
            PhaseCWindowAccountingV1 {
                window_sequence,
                started_at: completed.started_at.clone(),
                ended_at: completed.ended_at.clone(),
                admitted_turns: counts.admitted_turns,
                completed_turns: counts.completed_turns,
            },
            &qualification,
            fence,
        )
        .await?;
        match applied {
            ModelTurnLeaseMutationOutcome::Applied => outcome.persisted_pools.push(pool_id),
            // Leadership was lost between the projection and the write. Stop
            // immediately: a stale generation may not mutate anything, and the
            // last persisted target stands.
            _ => {
                outcome.fenced = true;
                return Ok(outcome);
            }
        }
    }

    // Coverage loss drains the selected enforcing pools, and only after the
    // diagnostic window is durable.
    let selected: Vec<i64> = qualification
        .diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.pool_id > 0 && is_capability_coverage_loss_v1(diagnostic.code)
        })
        .map(|diagnostic| diagnostic.pool_id)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if !selected.is_empty() {
        outcome.drained_pools = repository
            .drain_enforcing_pools(&selected, controller_generation)
            .await?;
    }
    Ok(outcome)
}

/// What one reaper pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhaseCReaperOutcomeV1 {
    /// Observations whose compare-and-swap applied.
    pub expired: usize,
    /// Observations that moved between the read and the swap. A lease that
    /// heartbeats in that gap is healthy and is deliberately left alone.
    pub fenced: usize,
}

/// Expire every in-flight lease that is stale at the 90-second boundary.
///
/// Succession needs nothing but the database: the observation list comes from
/// the persisted `reserved_at`/`heartbeat_at` columns, and each expiry is Phase
/// A's exact compare-and-swap on `(lease_id, generation, request_id,
/// lifecycle, heartbeat_at)`. Running it twice therefore expires nothing the
/// second time, so reservation accounting is reclaimed at most once.
/// Reap only while this leader is still admitting mutations.
///
/// Cancellation is leadership loss in progress: the token is the actor's own,
/// cancelled before the advisory lock can be released. A cancelled pass
/// performs no read and no compare-and-swap, so stale work racing succession
/// cannot mutate anything on the way out.
pub async fn reap_stale_model_turn_leases_while_leading_v1(
    repository: &ModelTurnAdmissionRepository,
    cancel: &tokio_util::sync::CancellationToken,
    boundary_at: &str,
    limit: i64,
) -> djinn_db::Result<PhaseCReaperOutcomeV1> {
    if cancel.is_cancelled() {
        return Ok(PhaseCReaperOutcomeV1::default());
    }
    reap_stale_model_turn_leases_v1(repository, boundary_at, limit).await
}

pub async fn reap_stale_model_turn_leases_v1(
    repository: &ModelTurnAdmissionRepository,
    boundary_at: &str,
    limit: i64,
) -> djinn_db::Result<PhaseCReaperOutcomeV1> {
    let observations = repository
        .list_stale_lease_observations(boundary_at, limit)
        .await?;
    let mut outcome = PhaseCReaperOutcomeV1::default();
    for observation in observations {
        match repository.expire_lease(observation).await? {
            ModelTurnLeaseMutationOutcome::Applied => outcome.expired += 1,
            _ => outcome.fenced += 1,
        }
    }
    Ok(outcome)
}

/// How many stale lease observations one reaper pass will compare-and-swap.
pub const REAPER_PASS_LIMIT: i64 = 128;

/// Renewal floor a controller write demands of its own incarnation lease.
///
/// The lease is renewed by the actor's dedicated renewal task, so any value the
/// leader can still beat is a fence; the epoch is the loosest honest one, and
/// it keeps a slow tick from fencing a leader that is plainly alive. What the
/// fence actually excludes is a *different* or *draining* incarnation.
pub const PHASE_C_FENCE_LIVENESS_FLOOR: &str = "1970-01-01T00:00:00Z";

impl crate::CoordinatorActor {
    /// The durable controller fence for this coordinator incarnation.
    ///
    /// It is the incarnation lease the actor already registers and renews, so
    /// the controller inherits the advisory-lock leader lifecycle rather than
    /// inventing a second one.
    /// The audit generation this leader stamps on every durable mode change.
    ///
    /// The *authority* is the incarnation lease in
    /// [`Self::model_turn_controller_fence`]; this is the monotonic per-leader
    /// counter that makes the ledger row attributable to one tick.
    #[must_use]
    pub fn model_turn_controller_generation(&self) -> i64 {
        i64::from(self.prune_tick_counter).saturating_add(1)
    }

    #[must_use]
    pub fn model_turn_controller_fence(&self, live_since_at: String) -> ModelTurnControllerFence {
        ModelTurnControllerFence {
            incarnation_id: self.coordinator_incarnation_id.clone(),
            live_since_at,
        }
    }

    /// One reaper pass, run from the leader tick.
    ///
    /// The actor only exists after `run_with_leadership` wins the advisory
    /// lock, and cancellation stops the pass before it can mutate anything.
    pub(crate) async fn sweep_stale_model_turn_leases(&self) {
        let Ok(boundary_at) = ::time::OffsetDateTime::now_utc()
            .format(&::time::format_description::well_known::Rfc3339)
        else {
            return;
        };
        let repository = ModelTurnAdmissionRepository::new(self.db.clone());
        match reap_stale_model_turn_leases_while_leading_v1(
            &repository,
            &self.cancel,
            &boundary_at,
            REAPER_PASS_LIMIT,
        )
        .await
        {
            Ok(outcome) if outcome.expired > 0 || outcome.fenced > 0 => {
                // Counts only: no pool identity, no request or lease id.
                tracing::info!(
                    expired = outcome.expired,
                    fenced = outcome.fenced,
                    "model-turn lease reaper pass"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "model-turn lease reaper pass failed");
            }
        }
    }

    /// One completed-window controller cycle, run from the leader tick.
    ///
    /// This is the production caller of the Phase-C plane. It builds the
    /// authoritative denominator from this actor's live workload inventory
    /// crossed with the coordinator's durable dispatch topology, collects the
    /// authoritative persisted evidence for the window, runs the fail-closed
    /// qualifier, and persists the typed verdict under this incarnation's
    /// durable fence — draining any enforcing pool that lost coverage.
    ///
    /// Pools sit at `off` until an operator opts one in, so an unarmed
    /// deployment has an empty topology and this writes nothing. The moment a
    /// pool is moved to `shadow` or `enforce` it starts producing windows.
    pub(crate) async fn run_completed_phase_c_window(&mut self) {
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
        // One pass per aligned window: the tick is faster than the window.
        if self.last_phase_c_window_start == Some(window.start_second()) {
            return;
        }
        let Some((started_at, ended_at)) = window_bounds_v1(window) else {
            return;
        };
        let repository = ModelTurnAdmissionRepository::new(self.db.clone());
        let pools = match repository.list_observable_pools(EVIDENCE_PAGE_LIMIT).await {
            Ok(pools) => pools,
            Err(error) => {
                tracing::warn!(%error, "Phase-C controller could not read its dispatch topology");
                return;
            }
        };
        if pools.is_empty() {
            self.last_phase_c_window_start = Some(window.start_second());
            return;
        }
        let records = match inventory.list().await {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "Phase-C controller could not read live slot inventory");
                return;
            }
        };
        let projection = project_dispatch_topology_paths_v1(&self.catalog, &records, &pools);
        if projection.expected_paths.is_empty() {
            self.last_phase_c_window_start = Some(window.start_second());
            return;
        }

        let mut capability_evidence = Vec::new();
        let mut admitted_attempts = Vec::new();
        let mut counts: BTreeMap<i64, PhaseCWindowCountsV1> = BTreeMap::new();
        for path in &projection.expected_paths {
            let pool_id = path.pool_id;
            if let Ok(heartbeats) = repository
                .recent_capability_heartbeats(pool_id, EVIDENCE_PAGE_LIMIT)
                .await
            {
                for (slot_pod_uid, deployment_revision, _, _, heartbeat_at) in heartbeats {
                    if slot_pod_uid != path.slot_pod_uid
                        || deployment_revision != path.deployment_revision
                    {
                        continue;
                    }
                    if let Some(evidence) = capability_evidence_from_heartbeat(path, &heartbeat_at)
                    {
                        capability_evidence.push(evidence);
                    }
                }
            }
            let Ok(rows) = repository
                .phase_c_evidence_in_window(pool_id, &started_at, &ended_at, EVIDENCE_PAGE_LIMIT)
                .await
            else {
                continue;
            };
            let mine: Vec<ModelTurnPhaseCEvidence> = rows
                .into_iter()
                .filter(|row| {
                    row.slot_pod_uid == path.slot_pod_uid
                        && row.deployment_revision == path.deployment_revision
                })
                .collect();
            let attempts = admitted_attempts_from_evidence(path, window, &mine);
            let entry = counts.entry(pool_id).or_default();
            entry.admitted_turns += attempts.len() as i64;
            entry.completed_turns += attempts
                .iter()
                .filter(|attempt| {
                    attempt
                        .stages
                        .iter()
                        .any(|stage| stage.stage == PhaseCAttemptStageV1::Reconcile)
                })
                .count() as i64;
            admitted_attempts.extend(attempts);
        }

        let fence = self.model_turn_controller_fence(PHASE_C_FENCE_LIVENESS_FLOOR.to_owned());
        let completed = PhaseCCompletedWindowV1 {
            window,
            started_at,
            ended_at,
            projection: &projection,
            capability_evidence: &capability_evidence,
            admitted_attempts: &admitted_attempts,
            counts,
        };
        match run_completed_window_cycle_v1(
            &repository,
            &self.catalog,
            &fence,
            &completed,
            self.model_turn_controller_generation(),
        )
        .await
        {
            Ok(outcome) => {
                if !outcome.fenced {
                    self.last_phase_c_window_start = Some(window.start_second());
                }
                // The single qualifier verdict for this window. The Phase-D
                // enforcement pass reads it; it does not re-qualify.
                self.last_phase_c_window_trainable =
                    !outcome.fenced && outcome.qualification.admitted;
                // Counts only: no pool identity, no slot uid, no revision.
                tracing::info!(
                    persisted = outcome.persisted_pools.len(),
                    drained = outcome.drained_pools.len(),
                    fenced = outcome.fenced,
                    trainable = outcome.qualification.admitted,
                    "Phase-C controller window"
                );
            }
            Err(error) => {
                tracing::warn!(%error, "Phase-C controller window failed");
            }
        }
    }
}

// ── Reconstructing evidence from the durable ledgers ────────────────────────

/// Widest durable page either evidence read will take for one pool-window.
pub const EVIDENCE_PAGE_LIMIT: i64 = 256;

/// Parse a persisted timestamp into epoch seconds.
///
/// `timestamptz::text` renders `1970-01-01 00:02:00+00`, which is not RFC 3339,
/// so both spellings are accepted and anything else is simply not evidence.
fn persisted_second(value: &str) -> Option<i64> {
    let rfc = value.replacen(' ', "T", 1);
    let rfc = if rfc.ends_with("+00") {
        format!("{rfc}:00")
    } else {
        rfc
    };
    ::time::OffsetDateTime::parse(&rfc, &::time::format_description::well_known::Rfc3339)
        .ok()
        .map(|parsed| parsed.unix_timestamp())
}

/// Rebuild capability evidence for one expected path from persisted heartbeats.
///
/// A heartbeat row records that this exact path reported *covered* at one
/// instant. It does not record a coverage interval, so the reconstruction is
/// the narrowest one the row actually supports: the instant itself. Widening it
/// to the window would be inventing coverage that was never observed, and the
/// qualifier would then train on it.
fn capability_evidence_from_heartbeat(
    path: &ExpectedAttemptPathV1,
    heartbeat_at: &str,
) -> Option<PhaseCCapabilityEvidenceV1> {
    let second = persisted_second(heartbeat_at)?;
    Some(PhaseCCapabilityEvidenceV1 {
        path: path.clone(),
        coverage_start_second: second,
        coverage_end_second: second,
        observed_at_second: second,
        covered: true,
    })
}

/// Rebuild the admitted attempts of one pool-window from persisted evidence.
///
/// Rows are grouped by their opaque attempt fingerprint. Everything the ledger
/// does not record is reconstructed fail-closed: authoritative usage is not a
/// persisted column, so it is `false`, and the qualifier reports `MissingUsage`
/// until the provider chain persists it. Nothing here invents a stage, an
/// outcome, or a timestamp the ledger did not hold.
fn admitted_attempts_from_evidence(
    path: &ExpectedAttemptPathV1,
    window: AlignedPhaseCWindowV1,
    rows: &[ModelTurnPhaseCEvidence],
) -> Vec<PhaseCAdmittedAttemptV1> {
    let mut by_attempt: BTreeMap<&str, Vec<&ModelTurnPhaseCEvidence>> = BTreeMap::new();
    for row in rows {
        by_attempt
            .entry(row.attempt_fingerprint.as_str())
            .or_default()
            .push(row);
    }
    by_attempt
        .into_values()
        .filter_map(|rows| {
            let mut stages = Vec::with_capacity(rows.len());
            let mut earliest = None;
            for row in rows {
                let Some(stage) = evidence_stage(&row.stage) else {
                    continue;
                };
                let Some(second) = persisted_second(&row.recorded_at) else {
                    continue;
                };
                earliest = Some(earliest.map_or(second, |current: i64| current.min(second)));
                stages.push(PhaseCAttemptStageEvidenceV1 {
                    stage,
                    timestamp_second: second,
                    outcome: evidence_outcome(stage, &row.outcome),
                });
            }
            let admitted_at_second = earliest?;
            if !window.contains(admitted_at_second) {
                return None;
            }
            Some(PhaseCAdmittedAttemptV1 {
                path: path.clone(),
                admitted_at_second,
                // Not a persisted column. Fail closed rather than assume.
                has_authoritative_usage: false,
                lease_expired: false,
                breaker_open: false,
                stages,
            })
        })
        .collect()
}

fn evidence_stage(stage: &str) -> Option<PhaseCAttemptStageV1> {
    match stage {
        "decision" => Some(PhaseCAttemptStageV1::Decision),
        "dispatch" => Some(PhaseCAttemptStageV1::Dispatch),
        "heartbeat" => Some(PhaseCAttemptStageV1::Heartbeat),
        "provider_outcome" => Some(PhaseCAttemptStageV1::ProviderOutcome),
        "reconcile" => Some(PhaseCAttemptStageV1::Reconcile),
        _ => None,
    }
}

/// The persisted outcome vocabulary is coarse. `missing` stays missing, and the
/// provider stage is rebuilt from the typed terminal the ledger recorded.
fn evidence_outcome(stage: PhaseCAttemptStageV1, outcome: &str) -> PhaseCAttemptEvidenceOutcomeV1 {
    if outcome == "missing" {
        return PhaseCAttemptEvidenceOutcomeV1::Missing;
    }
    if stage != PhaseCAttemptStageV1::ProviderOutcome {
        return PhaseCAttemptEvidenceOutcomeV1::Recorded;
    }
    let terminal = match outcome {
        "succeeded" => djinn_provider::ProviderAttemptTerminalV1::Completed,
        _ => djinn_provider::ProviderAttemptTerminalV1::Failed(
            djinn_provider::ProviderAttemptLossV1::Protocol,
        ),
    };
    PhaseCAttemptEvidenceOutcomeV1::Provider(Box::new(djinn_provider::ProviderOutcomeV1 {
        terminal,
        authoritative_usage: None,
        observation: None,
        abort: djinn_provider::ProviderAttemptAbortResultV1::NotRequested,
        token_emission: Default::default(),
    }))
}

/// Cross live Ready slots with the coordinator's durable dispatch topology.
///
/// Both halves are authoritative and neither is a report: the slots come from
/// the actor's own live inventory, the routes from pool rows admission created,
/// and every route must still resolve in the active catalog. A pool whose
/// labels no longer resolve is dropped rather than carried on its own say-so.
#[must_use]
pub fn project_dispatch_topology_paths_v1(
    catalog: &CatalogService,
    records: &[WorkloadRecord],
    pools: &[ModelTurnPool],
) -> ExpectedAttemptPathProjectionV1 {
    let qualified: Vec<&ModelTurnPool> = pools
        .iter()
        .filter(|pool| {
            catalog
                .find_model(&format!("{}/{}", pool.provider_id, pool.model_id))
                .is_some_and(|model| {
                    model.provider_id == pool.provider_id && model.id == pool.model_id
                })
        })
        .collect();
    let mut expected_paths = BTreeSet::new();
    for record in records.iter().filter(eligible_live_slot) {
        let Some(uid) = record
            .uid
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        let Some(revision) = record
            .deployment_revision
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        for pool in &qualified {
            expected_paths.insert(ExpectedAttemptPathV1::from_resolved_route(
                uid.to_owned(),
                revision.to_owned(),
                pool,
            ));
        }
    }
    ExpectedAttemptPathProjectionV1 {
        expected_paths: expected_paths.into_iter().collect(),
        joined_reports: Vec::new(),
    }
}

/// The last aligned 60-second window that has fully elapsed at `now_second`.
#[must_use]
pub fn last_completed_window_v1(now_second: i64) -> Option<AlignedPhaseCWindowV1> {
    let start = now_second
        .div_euclid(AlignedPhaseCWindowV1::SECONDS)
        .checked_sub(1)?
        .checked_mul(AlignedPhaseCWindowV1::SECONDS)?;
    AlignedPhaseCWindowV1::new(start).ok()
}

/// RFC 3339 rendering of one aligned window's exact bounds.
#[must_use]
pub fn window_bounds_v1(window: AlignedPhaseCWindowV1) -> Option<(String, String)> {
    let render = |second: i64| {
        ::time::OffsetDateTime::from_unix_timestamp(second)
            .ok()
            .and_then(|at| {
                at.format(&::time::format_description::well_known::Rfc3339)
                    .ok()
            })
    };
    Some((render(window.start_second())?, render(window.end_second())?))
}

#[cfg(test)]
#[path = "model_turn_admission_controller_tests.rs"]
mod tests;
