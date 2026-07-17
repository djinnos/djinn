//! Coordinator-owned durable admission policy for build-producing workloads.
//!
//! The journal supplies serialization and lifecycle fencing; this module fixes
//! workload classification before dispatch and translates controller facts into
//! the data-only graph-warmer protocol.

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use async_trait::async_trait;
use djinn_db::{
    AdmissionDomain, AdmissionJournalKey, AdmissionJournalRepository, AdmissionJournalRow,
    AdmissionRecoveryResult, AdmissionState, AdmissionWorkloadKind, CreateStartedInput,
    ReserveAdmissionInput, ReserveAdmissionResult, TerminalAdmissionInput, UidFencedAdmissionInput,
};
use djinn_k8s::{
    WarmAdmission, WarmAdmissionError, WarmAdmissionPermit, WarmAdmissionRequest,
    WarmAdmissionTransition,
};
use tokio::sync::{Mutex, Notify};

/// Policy applied at the coordinator admission boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BuildAdmissionMode {
    /// Deliberately bypass durable admission during rollout.
    Off,
    /// Record reservations but never deny at the configured reference cap.
    Observe,
    /// Atomically enforce the configured cap.
    #[default]
    Enforce,
}

/// Typed classification captured before dispatch; only the audited bypass weighs zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildWorkloadKind {
    TaskRun {
        role: TaskRunRole,
    },
    GraphWarmJob,
    /// Explicit, auditable non-build work. This is the only zero-slot class.
    NonBuild {
        audit_reason: &'static str,
    },
}

/// All currently dispatchable task-run roles are build-producing work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskRunRole {
    Worker,
    Reviewer,
    Lead,
    Planner,
    Architect,
    Advocate,
    Adversary,
    Judge,
}

impl TaskRunRole {
    /// Classify a known coordinator role. Unknown and missing values fail closed.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value {
            Some("worker") => Some(Self::Worker),
            Some("reviewer") => Some(Self::Reviewer),
            Some("lead") => Some(Self::Lead),
            Some("planner") => Some(Self::Planner),
            Some("architect") => Some(Self::Architect),
            Some("advocate") => Some(Self::Advocate),
            Some("adversary") => Some(Self::Adversary),
            Some("judge") => Some(Self::Judge),
            _ => None,
        }
    }
}

/// Immutable identity fixed before capacity is reserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildAdmissionRequest {
    pub domain: AdmissionDomain,
    pub work_id: String,
    pub generation: i64,
    pub object_name: String,
    pub kind: BuildWorkloadKind,
}

/// Admission decision returned to task dispatch callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildAdmissionDecision {
    Permitted {
        permit: WarmAdmissionPermit,
        idempotent: bool,
    },
    Denied {
        occupancy: i64,
        cap: i64,
    },
    /// Classification was absent or unrecognized. The observation counter is bounded.
    Unclassified,
}

/// Bounded, deterministic readiness reason for Enforce admission gating.
///
/// Enforce admission fails closed until every required gate is healthy. Observe
/// records degradation but remains non-denying. Off has no readiness coupling.
/// Variants are exhaustive and intentionally bounded so telemetry and tests can
/// rely on a stable, enumerated set. The default is fail-closed
/// ([`BuildAdmissionReadiness::JournalRecoveryIncomplete`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BuildAdmissionReadiness {
    /// The journal has not been recovered yet; Enforce starts in this state.
    #[default]
    JournalRecoveryIncomplete,
    /// The journal itself is unhealthy (a recovery/seed query failed).
    JournalUnhealthy,
    /// At least one recovered row is in CreateUnknown state.
    CreateUnknownHealth,
    /// Seeded occupancy exceeded the configured cap after recovery.
    SeededOccupancyAboveCap,
    /// Kubernetes inventory has not completed yet.
    InventoryPending,
    /// Single-active topology check has not succeeded yet.
    TopologyPending,
    /// Graceful shutdown is draining; new reservations are blocked.
    ShutdownDraining,
    /// Every required gate is healthy; admission may proceed.
    Healthy,
}

impl BuildAdmissionReadiness {
    #[must_use]
    pub fn is_healthy(self) -> bool {
        matches!(self, Self::Healthy)
    }
}

#[derive(Clone, Debug)]
struct PermitState {
    key: AdmissionJournalKey,
    creator_server_epoch: String,
    object_name: String,
    durable: bool,
    released: bool,
    /// This permit was seeded from a recovered CreateUnknown row and has not
    /// yet been adopted into Live. Tracked so the startup CreateUnknown gate
    /// is decremented exactly once when the row resolves.
    create_unknown_outstanding: bool,
}

/// Outcome of durable predecessor-epoch recovery and controller seeding.
///
/// The controller seeds in-memory permit bookkeeping from the durable active
/// rows returned by [`AdmissionJournalRepository::recover_predecessor_epoch`]
/// without duplicating occupancy or relying on a separate in-memory permit
/// count: occupancy is always derived from the journal itself.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AdmissionSeedReport {
    /// Number of predecessor Reserved rows atomically retired to Terminal.
    pub retired_reserved: u64,
    /// Number of predecessor CreateInFlight rows converted to CreateUnknown.
    pub marked_create_unknown: u64,
    /// Number of recovered rows the controller seeded as occupying permits.
    pub seeded_rows: u64,
    /// Final readiness reason applied after seeding completed.
    pub readiness: BuildAdmissionReadiness,
}

/// A single controller shared by task-run dispatch and graph warming.
pub struct BuildAdmissionController {
    journal: Arc<AdmissionJournalRepository>,
    mode: BuildAdmissionMode,
    cap: i64,
    creator_server_epoch: String,
    permits: Mutex<HashMap<WarmAdmissionPermit, PermitState>>,
    permits_by_key: Mutex<HashMap<String, WarmAdmissionPermit>>,
    /// Runtime task-run IDs are learned when a session starts. This binding
    /// prevents a delayed terminal callback from selecting a later generation.
    permits_by_task_run: Mutex<HashMap<String, WarmAdmissionPermit>>,
    unclassified_observations: Mutex<u64>,
    would_defer_observations: Mutex<u64>,
    /// Readiness gate flags. The bounded [`BuildAdmissionReadiness`] reason is
    /// DERIVED from these flags in fail-closed priority order, so no caller can
    /// mark Enforce healthy without every real startup check completing:
    /// journal recovery, journal health, CreateUnknown resolution, cap
    /// seeding, Kubernetes inventory, and single-active topology.
    ///
    /// The durable journal has been loaded and recovered for this process.
    journal_recovered: AtomicBool,
    /// The recovery/seed queries themselves succeeded.
    journal_healthy: AtomicBool,
    /// Recovered rows still occupying as CreateUnknown. Seeding sets this from
    /// the durable journal; adopting a seeded CreateUnknown row into Live
    /// decrements it exactly once. Enforce stays closed while it is non-zero.
    create_unknown_pending: AtomicU64,
    /// Seeded durable occupancy exceeded the configured cap at recovery.
    /// Cleared when a terminal release brings occupancy back within the cap.
    over_cap: AtomicBool,
    /// The broad Kubernetes inventory LIST completed successfully.
    inventory_ready: AtomicBool,
    /// The single-active topology gate (coordinator leadership) is held by
    /// this process.
    topology_ready: AtomicBool,
    /// Graceful shutdown begins draining before permit release. New Enforce
    /// reservations are blocked while this is set; Observe/Off are unaffected.
    draining: AtomicBool,
    released: Notify,
}

impl BuildAdmissionController {
    #[must_use]
    pub fn new(
        journal: Arc<AdmissionJournalRepository>,
        mode: BuildAdmissionMode,
        cap: i64,
        creator_server_epoch: impl Into<String>,
    ) -> Self {
        Self {
            journal,
            mode,
            cap,
            creator_server_epoch: creator_server_epoch.into(),
            permits: Mutex::new(HashMap::new()),
            permits_by_key: Mutex::new(HashMap::new()),
            permits_by_task_run: Mutex::new(HashMap::new()),
            unclassified_observations: Mutex::new(0),
            would_defer_observations: Mutex::new(0),
            journal_recovered: AtomicBool::new(true),
            journal_healthy: AtomicBool::new(true),
            create_unknown_pending: AtomicU64::new(0),
            over_cap: AtomicBool::new(false),
            inventory_ready: AtomicBool::new(true),
            topology_ready: AtomicBool::new(true),
            draining: AtomicBool::new(false),
            released: Notify::new(),
        }
    }

    /// Construct an Enforce controller which cannot admit work until every
    /// startup gate completes.
    ///
    /// The controller starts fail-closed with all startup gates unsatisfied:
    /// journal recovery, Kubernetes inventory, and the single-active topology
    /// check must each complete before admission opens. Observe and Off never
    /// gate admission and are constructed via [`Self::new`].
    #[must_use]
    pub fn new_closed(
        journal: Arc<AdmissionJournalRepository>,
        cap: i64,
        creator_server_epoch: impl Into<String>,
    ) -> Self {
        let controller = Self::new(
            journal,
            BuildAdmissionMode::Enforce,
            cap,
            creator_server_epoch,
        );
        controller.journal_recovered.store(false, Ordering::Release);
        controller.inventory_ready.store(false, Ordering::Release);
        controller.topology_ready.store(false, Ordering::Release);
        controller
    }

    /// Open the controller after every startup gate has completed.
    ///
    /// This satisfies all readiness gates at once. Production startup uses the
    /// granular `mark_*` methods as each real check completes (journal
    /// recovery first, then inventory, then topology); this helper is for
    /// tests that need an open Enforce controller without walking startup.
    pub fn mark_ready(&self) {
        self.journal_recovered.store(true, Ordering::Release);
        self.journal_healthy.store(true, Ordering::Release);
        self.create_unknown_pending.store(0, Ordering::Release);
        self.over_cap.store(false, Ordering::Release);
        self.inventory_ready.store(true, Ordering::Release);
        self.topology_ready.store(true, Ordering::Release);
    }

    /// Record that journal recovery failed. Enforce stays fail-closed with
    /// [`BuildAdmissionReadiness::JournalUnhealthy`]; Observe records the same
    /// degradation but never denies.
    pub fn mark_journal_unhealthy(&self) {
        self.journal_recovered.store(true, Ordering::Release);
        self.journal_healthy.store(false, Ordering::Release);
    }

    /// The broad Kubernetes inventory LIST completed successfully.
    pub fn mark_inventory_ready(&self) {
        self.inventory_ready.store(true, Ordering::Release);
    }

    /// The Kubernetes inventory has not completed (or failed); Enforce stays
    /// fail-closed with [`BuildAdmissionReadiness::InventoryPending`].
    pub fn mark_inventory_pending(&self) {
        self.inventory_ready.store(false, Ordering::Release);
    }

    /// The single-active topology gate is held: this process won the
    /// coordinator leadership race, so it is the only active admission writer.
    pub fn mark_topology_ready(&self) {
        self.topology_ready.store(true, Ordering::Release);
    }

    /// Inspect the current bounded readiness reason, derived from the startup
    /// gates in fail-closed priority order.
    #[must_use]
    pub fn readiness(&self) -> BuildAdmissionReadiness {
        if self.draining.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::ShutdownDraining;
        }
        if !self.journal_recovered.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::JournalRecoveryIncomplete;
        }
        if !self.journal_healthy.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::JournalUnhealthy;
        }
        if self.create_unknown_pending.load(Ordering::Acquire) > 0 {
            return BuildAdmissionReadiness::CreateUnknownHealth;
        }
        if self.over_cap.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::SeededOccupancyAboveCap;
        }
        if !self.inventory_ready.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::InventoryPending;
        }
        if !self.topology_ready.load(Ordering::Acquire) {
            return BuildAdmissionReadiness::TopologyPending;
        }
        BuildAdmissionReadiness::Healthy
    }

    /// The configured admission mode.
    #[must_use]
    pub fn mode(&self) -> BuildAdmissionMode {
        self.mode
    }

    /// The unique server epoch allocated for this controller's process.
    #[must_use]
    pub fn server_epoch(&self) -> &str {
        &self.creator_server_epoch
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.readiness().is_healthy()
    }

    /// Begin graceful shutdown draining. New Enforce reservations are blocked
    /// while draining; in-flight permits may still transition to terminal.
    pub fn begin_draining(&self) {
        self.draining.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    /// Queue consumers may wait here after a terminal release instead of polling.
    #[must_use]
    pub fn release_notifier(&self) -> &Notify {
        &self.released
    }

    /// Durable inspection seam used by coordinator integration tests.
    #[cfg(test)]
    pub(crate) fn journal(&self) -> &Arc<AdmissionJournalRepository> {
        &self.journal
    }

    /// Bounded count suitable for a telemetry exporter; values saturate at 1024.
    pub async fn unclassified_observation_count(&self) -> u64 {
        *self.unclassified_observations.lock().await
    }

    /// Bounded Observe-mode signal that the reference cap would have deferred work.
    pub async fn would_defer_observation_count(&self) -> u64 {
        *self.would_defer_observations.lock().await
    }

    pub async fn admit(
        &self,
        request: BuildAdmissionRequest,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        if self.mode == BuildAdmissionMode::Enforce && (!self.is_ready() || self.is_draining()) {
            return Ok(BuildAdmissionDecision::Denied {
                occupancy: 0,
                cap: self.cap,
            });
        }
        let workload_kind = match request.kind {
            BuildWorkloadKind::TaskRun { .. } => match request.domain {
                AdmissionDomain::TaskObservation => AdmissionWorkloadKind::Task,
                AdmissionDomain::InvocationBuild => AdmissionWorkloadKind::Invocation,
                AdmissionDomain::WarmBuild => AdmissionWorkloadKind::Warm,
            },
            BuildWorkloadKind::GraphWarmJob => AdmissionWorkloadKind::Warm,
            BuildWorkloadKind::NonBuild { audit_reason } if !audit_reason.is_empty() => {
                return Ok(BuildAdmissionDecision::Permitted {
                    permit: WarmAdmissionPermit::new(),
                    idempotent: false,
                });
            }
            BuildWorkloadKind::NonBuild { .. } => {
                self.observe_unclassified().await;
                return Ok(BuildAdmissionDecision::Unclassified);
            }
        };
        let key = AdmissionJournalKey {
            domain: request.domain,
            work_id: request.work_id,
            generation: request.generation,
        };
        let permit_key = permit_key(&key);
        let durable = self.mode != BuildAdmissionMode::Off;
        let idempotent_permit = self.permits_by_key.lock().await.get(&permit_key).cloned();
        if let Some(permit) = idempotent_permit {
            return Ok(BuildAdmissionDecision::Permitted {
                permit,
                idempotent: true,
            });
        }
        let mut idempotent = false;
        if durable {
            let reservation = if self.mode == BuildAdmissionMode::Observe {
                let observed = self
                    .journal
                    .reserve_observed(
                        &ReserveAdmissionInput {
                            key: key.clone(),
                            workload_kind,
                            creator_server_epoch: self.creator_server_epoch.clone(),
                            object_name: request.object_name.clone(),
                        },
                        self.cap,
                    )
                    .await;
                let observed = match observed {
                    Ok(observed) => observed,
                    Err(error) => {
                        // Observe is telemetry-only: a journal outage must not become a dispatch denial.
                        tracing::warn!(%error, "build admission observation unavailable; permitting without journal telemetry");
                        return self
                            .permit_without_reservation(key, permit_key, request.object_name)
                            .await;
                    }
                };
                if observed.would_defer {
                    let mut count = self.would_defer_observations.lock().await;
                    *count = count.saturating_add(1).min(1024);
                }
                observed.reservation
            } else {
                self.journal
                    .reserve(
                        &ReserveAdmissionInput {
                            key: key.clone(),
                            workload_kind,
                            creator_server_epoch: self.creator_server_epoch.clone(),
                            object_name: request.object_name.clone(),
                        },
                        self.cap,
                    )
                    .await
                    .map_err(unavailable)?
            };
            match reservation {
                ReserveAdmissionResult::Denied { occupancy, cap } => {
                    return Ok(BuildAdmissionDecision::Denied { occupancy, cap });
                }
                ReserveAdmissionResult::Reserved {
                    idempotent: value, ..
                } => idempotent = value,
            }
        }
        let permit = WarmAdmissionPermit::new();
        let state = PermitState {
            key: key.clone(),
            creator_server_epoch: self.creator_server_epoch.clone(),
            object_name: request.object_name,
            durable,
            released: false,
            create_unknown_outstanding: false,
        };
        self.permits.lock().await.insert(permit.clone(), state);
        self.permits_by_key
            .lock()
            .await
            .insert(permit_key, permit.clone());
        Ok(BuildAdmissionDecision::Permitted { permit, idempotent })
    }

    async fn permit_without_reservation(
        &self,
        key: AdmissionJournalKey,
        permit_key: String,
        object_name: String,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        let permit = WarmAdmissionPermit::new();
        self.permits.lock().await.insert(
            permit.clone(),
            PermitState {
                key,
                creator_server_epoch: self.creator_server_epoch.clone(),
                object_name,
                durable: false,
                released: false,
                create_unknown_outstanding: false,
            },
        );
        self.permits_by_key
            .lock()
            .await
            .insert(permit_key, permit.clone());
        Ok(BuildAdmissionDecision::Permitted {
            permit,
            idempotent: false,
        })
    }

    /// A missing or unknown task role is a fail-closed classification result.
    pub async fn admit_task_run(
        &self,
        role: Option<&str>,
        domain: AdmissionDomain,
        work_id: String,
        generation: i64,
        object_name: String,
    ) -> Result<BuildAdmissionDecision, WarmAdmissionError> {
        let Some(role) = TaskRunRole::parse(role) else {
            self.observe_unclassified().await;
            return Ok(BuildAdmissionDecision::Unclassified);
        };
        self.admit(BuildAdmissionRequest {
            domain,
            work_id,
            generation,
            object_name,
            kind: BuildWorkloadKind::TaskRun { role },
        })
        .await
    }

    /// Return the retained permit for this exact admission key, in any domain.
    ///
    /// This is the domain-appropriate recovered-permit lookup: seeded and
    /// admitted permits are keyed by the full journal key, so a warm-build row
    /// is addressable with [`AdmissionDomain::WarmBuild`] while a task-run row
    /// uses [`AdmissionDomain::TaskObservation`]. Recovery and adoption use
    /// this to reach the permit seeded from a recovered row.
    pub async fn permit_for_key(
        &self,
        domain: AdmissionDomain,
        work_id: &str,
        generation: i64,
    ) -> Option<WarmAdmissionPermit> {
        let key = AdmissionJournalKey {
            domain,
            work_id: work_id.to_owned(),
            generation,
        };
        self.permits_by_key
            .lock()
            .await
            .get(&permit_key(&key))
            .cloned()
    }

    /// Return the retained permit for this exact task generation.
    pub async fn task_run_permit(
        &self,
        task_id: &str,
        generation: i64,
    ) -> Option<WarmAdmissionPermit> {
        self.permit_for_key(AdmissionDomain::TaskObservation, task_id, generation)
            .await
    }

    /// Bind a UID-bearing runtime task-run to a permit already made Live.
    pub async fn bind_task_run(&self, task_run_id: String, permit: WarmAdmissionPermit) {
        self.permits_by_task_run
            .lock()
            .await
            .insert(task_run_id, permit);
    }

    /// Return only the permit bound to this runtime task-run UID. There is no
    /// task-ID fallback because that could release a newer reopened generation.
    pub async fn task_run_permit_for_runtime_id(
        &self,
        task_run_id: &str,
    ) -> Option<WarmAdmissionPermit> {
        self.permits_by_task_run
            .lock()
            .await
            .get(task_run_id)
            .cloned()
    }

    async fn observe_unclassified(&self) {
        let mut count = self.unclassified_observations.lock().await;
        *count = count.saturating_add(1).min(1024);
        tracing::warn!(
            observations = *count,
            "build admission classification missing or unknown; denying dispatch"
        );
    }

    async fn transition_permit(
        &self,
        permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        let Some(state) = self.permits.lock().await.get(permit).cloned() else {
            return Err(WarmAdmissionError::UnknownPermit);
        };
        if !state.durable {
            return Ok(());
        }
        let terminal = matches!(
            transition,
            WarmAdmissionTransition::DefinitiveFailure { .. }
                | WarmAdmissionTransition::Terminal { .. }
        );
        let adopts_into_live = matches!(transition, WarmAdmissionTransition::Live { .. });
        let result = match transition {
            WarmAdmissionTransition::CreateStarted => self
                .journal
                .mark_create_started(&CreateStartedInput {
                    key: state.key.clone(),
                    creator_server_epoch: state.creator_server_epoch,
                    object_name: state.object_name,
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::Live { uid } => self
                .journal
                .mark_live(&UidFencedAdmissionInput {
                    key: state.key.clone(),
                    object_uid: uid,
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::CreateUnknown { .. } => self
                .journal
                .mark_create_unknown(&state.key)
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::DefinitiveFailure { .. } => self
                .journal
                .mark_definitive_create_failure(&state.key)
                .await
                .map(|_| ())
                .map_err(unavailable),
            WarmAdmissionTransition::Terminal { uid } => self
                .journal
                .mark_terminal(&TerminalAdmissionInput {
                    key: state.key.clone(),
                    object_uid: Some(uid),
                })
                .await
                .map(|_| ())
                .map_err(unavailable),
        };
        let transition_durable = result.is_ok();
        if let Err(error) = result {
            if self.mode != BuildAdmissionMode::Observe {
                return Err(error);
            }
            tracing::warn!(%error, "build admission observation transition unavailable; continuing without journal telemetry");
        }
        // A recovered CreateUnknown row stops occupying as unknown once it is
        // adopted into Live with the authoritative UID: clear its startup-gate
        // contribution exactly once so readiness can advance past
        // `CreateUnknownHealth`.
        if transition_durable && adopts_into_live {
            let cleared = {
                let mut permits = self.permits.lock().await;
                match permits.get_mut(permit) {
                    Some(state) if state.create_unknown_outstanding => {
                        state.create_unknown_outstanding = false;
                        true
                    }
                    Some(_) | None => false,
                }
            };
            if cleared {
                self.create_unknown_pending
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                        Some(value.saturating_sub(1))
                    })
                    .ok();
            }
        }
        if terminal {
            let newly_released = {
                let mut permits = self.permits.lock().await;
                match permits.get_mut(permit) {
                    Some(state) if !state.released => {
                        state.released = true;
                        true
                    }
                    Some(_) | None => false,
                }
            };
            if newly_released {
                // Retain one wakeup when the actor is currently handling the event
                // that performed this release and therefore has no `notified()`
                // future registered in its select loop.
                self.released.notify_one();
            }
            // A terminal release can bring seeded occupancy back within the
            // cap; refresh the over-cap gate from the durable journal rather
            // than trusting in-memory bookkeeping.
            if transition_durable && self.over_cap.load(Ordering::Acquire) {
                match self.journal.count_task_or_warm_occupancy().await {
                    Ok(occupancy) if occupancy <= self.cap => {
                        self.over_cap.store(false, Ordering::Release);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error, "build admission: failed to refresh over-cap gate after release; retaining it conservatively");
                    }
                }
            }
        }
        Ok(())
    }

    /// Recover the durable predecessor epoch and seed this controller from the
    /// active recovered rows.
    ///
    /// This is the single startup recovery primitive. It must run before any
    /// Kubernetes inventory or task/warm create can proceed under Enforce. It
    /// uses [`AdmissionJournalRepository::recover_predecessor_epoch`] to
    /// atomically retire predecessor Reserved rows, convert predecessor
    /// CreateInFlight rows to occupying CreateUnknown, and retain predecessor
    /// CreateUnknown/Live rows — then seeds in-memory permit bookkeeping from
    /// all active recovered rows without duplicating occupancy.
    ///
    /// Occupancy is never tracked by an in-memory permit count: the journal is
    /// the single source of truth. Seeds record one permit per recovered active
    /// row so that idempotent re-admission and lifecycle transitions remain
    /// consistent across the restart boundary.
    ///
    /// After seeding, the readiness gates are updated deterministically from
    /// the durable journal: `CreateUnknownHealth` while any recovered
    /// CreateUnknown row still occupies, `SeededOccupancyAboveCap` while
    /// task/warm occupancy exceeds the cap. Journal recovery alone NEVER marks
    /// Enforce healthy: the inventory and topology gates stay pending until
    /// their own production checks complete, so a recovered controller with no
    /// other degradation reports `InventoryPending`. Observe/Off ignore the
    /// gates for admission but still receive the report for telemetry.
    pub async fn recover_and_seed(
        &self,
        predecessor_epoch: &str,
    ) -> Result<AdmissionSeedReport, WarmAdmissionError> {
        self.recover_and_seed_with_filter(predecessor_epoch, |_| true)
            .await
    }

    /// Variant of [`Self::recover_and_seed`] that lets a caller restrict which
    /// recovered active rows become in-memory seeded permits. The durable
    /// journal recovery still processes every predecessor row; only the
    /// in-memory seeding bookkeeping is filtered. This is used by tests that
    /// need to simulate a replacement process whose initial Kubernetes
    /// inventory is empty (all rows recovered from the journal, none from
    /// inventory) while still validating the durable occupancy accounting.
    pub async fn recover_and_seed_with_filter(
        &self,
        predecessor_epoch: &str,
        mut seed_filter: impl FnMut(&AdmissionJournalRow) -> bool,
    ) -> Result<AdmissionSeedReport, WarmAdmissionError> {
        let recovery = self
            .journal
            .recover_predecessor_epoch(predecessor_epoch)
            .await
            .map_err(unavailable)?;
        self.seed_from_recovery(&recovery, &mut seed_filter).await
    }

    /// Recover every predecessor epoch and seed this controller from all active
    /// recovered rows.
    ///
    /// This is the cold-restart recovery entry point: a replacement process does
    /// not know the exact predecessor epoch string(s), so it recovers every row
    /// whose `creator_server_epoch` differs from this process's epoch. See
    /// [`AdmissionJournalRepository::recover_all_predecessors`].
    pub async fn recover_all_predecessors_and_seed(
        &self,
    ) -> Result<AdmissionSeedReport, WarmAdmissionError> {
        let recovery = self
            .journal
            .recover_all_predecessors(&self.creator_server_epoch)
            .await
            .map_err(unavailable)?;
        self.seed_from_recovery(&recovery, &mut |_| true).await
    }

    /// Seed in-memory permit bookkeeping from a pre-fetched recovery result.
    ///
    /// Exposed so callers that have already recovered (for example via a
    /// shared journal repository in tests) can seed without a second recovery
    /// call. The journal remains the authoritative occupancy source; this only
    /// populates the permit/key maps used for idempotent re-admission and
    /// lifecycle transitions.
    pub async fn seed_from_recovery(
        &self,
        recovery: &AdmissionRecoveryResult,
        seed_filter: &mut impl FnMut(&AdmissionJournalRow) -> bool,
    ) -> Result<AdmissionSeedReport, WarmAdmissionError> {
        let mut seeded = 0u64;
        // The CreateUnknown startup gate reflects every active recovered row,
        // not only the ones seeded into memory: an unseeded CreateUnknown row
        // still occupies durable capacity and still gates Enforce readiness.
        let create_unknown_rows = recovery
            .active_rows
            .iter()
            .filter(|row| row.state == AdmissionState::CreateUnknown)
            .count() as u64;
        {
            let mut permits = self.permits.lock().await;
            let mut by_key = self.permits_by_key.lock().await;
            for row in &recovery.active_rows {
                if !seed_filter(row) {
                    continue;
                }
                // Re-seeding the same key reuses the existing permit so a
                // repeated recovery never duplicates in-memory bookkeeping.
                let key = permit_key(&row.key);
                let permit = match by_key.get(&key) {
                    Some(existing) => existing.clone(),
                    None => {
                        let permit = WarmAdmissionPermit::new();
                        by_key.insert(key, permit.clone());
                        permit
                    }
                };
                permits.insert(
                    permit,
                    PermitState {
                        key: row.key.clone(),
                        creator_server_epoch: row.creator_server_epoch.clone(),
                        object_name: row.object_name.clone(),
                        durable: true,
                        released: false,
                        create_unknown_outstanding: row.state == AdmissionState::CreateUnknown,
                    },
                );
                seeded = seeded.saturating_add(1);
            }
        }
        // Occupancy is always read from the journal; it is not derived from the
        // in-memory permit count. This keeps the cap invariant durable across a
        // process loss that leaves permits uncommitted in memory.
        let occupancy = self
            .journal
            .count_task_or_warm_occupancy()
            .await
            .map_err(unavailable)?;
        if self.mode != BuildAdmissionMode::Off {
            // Journal recovery succeeded. Only the journal-derived gates are
            // updated here: the inventory and topology gates are deliberately
            // NOT touched, so Enforce remains fail-closed
            // (`InventoryPending`/`TopologyPending`) until the real Kubernetes
            // inventory LIST and the single-active topology check complete.
            self.journal_recovered.store(true, Ordering::Release);
            self.journal_healthy.store(true, Ordering::Release);
            self.create_unknown_pending
                .store(create_unknown_rows, Ordering::Release);
            self.over_cap.store(occupancy > self.cap, Ordering::Release);
        }
        let readiness = self.readiness();
        Ok(AdmissionSeedReport {
            retired_reserved: recovery.retired_reserved,
            marked_create_unknown: recovery.marked_create_unknown,
            seeded_rows: seeded,
            readiness,
        })
    }
}

/// Allocate a fresh, unique server epoch for this process.
///
/// The epoch is a time-ordered UUIDv7 string so a replacement process always
/// sorts after its predecessor and recovery can distinguish rows by creator.
#[must_use]
pub fn allocate_server_epoch() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn unavailable(error: impl std::fmt::Display) -> WarmAdmissionError {
    WarmAdmissionError::Unavailable {
        diagnostic: error.to_string(),
    }
}

fn permit_key(key: &AdmissionJournalKey) -> String {
    format!("{:?}:{}:{}", key.domain, key.work_id, key.generation)
}

#[async_trait]
impl WarmAdmission for BuildAdmissionController {
    async fn admit(
        &self,
        request: WarmAdmissionRequest,
    ) -> Result<WarmAdmissionPermit, WarmAdmissionError> {
        let decision = self
            .admit(BuildAdmissionRequest {
                domain: AdmissionDomain::WarmBuild,
                work_id: request.work_id,
                generation: request.generation,
                object_name: request.object_name,
                kind: BuildWorkloadKind::GraphWarmJob,
            })
            .await?;
        match decision {
            BuildAdmissionDecision::Permitted { permit, .. } => Ok(permit),
            BuildAdmissionDecision::Denied { occupancy, cap } => Err(WarmAdmissionError::Denied {
                diagnostic: format!("occupancy {occupancy} reached cap {cap}"),
            }),
            BuildAdmissionDecision::Unclassified => Err(WarmAdmissionError::Denied {
                diagnostic: "unclassified build workload".into(),
            }),
        }
    }

    async fn transition(
        &self,
        permit: &WarmAdmissionPermit,
        transition: WarmAdmissionTransition,
    ) -> Result<(), WarmAdmissionError> {
        self.transition_permit(permit, transition).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use djinn_core::events::EventBus;
    use djinn_db::{
        AdmissionState, Database, ImageRepository, ProjectRepository,
        test_support::reject_admission_create_started_for_test,
    };
    use djinn_k8s::{
        K8sGraphWarmer, KubernetesConfig, WarmJobDispatcher, WarmJobManifest, WarmJobWatcher,
        WarmTerminalOutcome,
    };
    use djinn_runtime::GraphWarmerService;
    use futures::FutureExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    fn controller(mode: BuildAdmissionMode, cap: i64) -> BuildAdmissionController {
        BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(
                Database::open_in_memory().unwrap(),
            )),
            mode,
            cap,
            "epoch",
        )
    }
    fn warm(id: &str) -> WarmAdmissionRequest {
        WarmAdmissionRequest {
            domain: "ignored".into(),
            work_id: id.into(),
            generation: 0,
            object_name: format!("job-{id}"),
        }
    }

    async fn seed_project_with_ready_image(db: &Database, name: &str) -> String {
        let projects = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = projects.create(name, "test", name).await.unwrap();
        let images = ImageRepository::new(db.clone());
        let image_id = format!("img-{name}");
        images.create(&image_id, name, None, "{}").await.unwrap();
        images
            .mark_ready(
                &image_id,
                &format!("reg.example:5000/djinn-project-{}:abc123", project.id),
                None,
            )
            .await
            .unwrap();
        images
            .set_project_image(&project.id, Some(&image_id))
            .await
            .unwrap();
        project.id
    }

    struct AdmissionStateRecordingDispatcher {
        journal: Arc<AdmissionJournalRepository>,
        work_id: String,
        posts: Arc<AtomicUsize>,
        posted: Arc<Notify>,
    }

    #[async_trait]
    impl WarmJobDispatcher for AdmissionStateRecordingDispatcher {
        async fn dispatch(
            &self,
            _namespace: &str,
            _job: WarmJobManifest,
        ) -> Result<String, String> {
            let history = self
                .journal
                .list_history(AdmissionDomain::WarmBuild, &self.work_id)
                .await
                .unwrap();
            assert_eq!(
                history[0].state,
                AdmissionState::CreateInFlight,
                "the concrete controller must durably record CreateStarted before POST"
            );
            self.posts.fetch_add(1, Ordering::SeqCst);
            self.posted.notify_one();
            Ok("warm-job".into())
        }
    }

    struct FencedTerminalWatcher;

    #[async_trait]
    impl WarmJobWatcher for FencedTerminalWatcher {
        async fn wait_terminal(&self, _namespace: &str, _job_name: &str) -> WarmTerminalOutcome {
            WarmTerminalOutcome::Succeeded
        }

        async fn job_uid(&self, _namespace: &str, _job_name: &str) -> Option<String> {
            Some("warm-uid".into())
        }
    }

    #[tokio::test]
    async fn concrete_k8s_warmer_shares_task_cap_and_retries_after_fenced_release() {
        let db = Database::open_in_memory().unwrap();
        let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
        let controller = Arc::new(BuildAdmissionController::new(
            Arc::clone(&journal),
            BuildAdmissionMode::Enforce,
            1,
            "epoch",
        ));
        let project_id = seed_project_with_ready_image(&db, "shared-cap").await;
        let work_id = format!("graph-warm:{project_id}:unknown");
        let posts = Arc::new(AtomicUsize::new(0));
        let posted = Arc::new(Notify::new());
        let warmer = K8sGraphWarmer::with_dispatcher(
            KubernetesConfig::for_testing(),
            db,
            Arc::new(AdmissionStateRecordingDispatcher {
                journal: Arc::clone(&journal),
                work_id: work_id.clone(),
                posts: Arc::clone(&posts),
                posted: Arc::clone(&posted),
            }),
            Arc::new(FencedTerminalWatcher),
        )
        .with_warm_admission(controller.clone());

        let task = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                1,
                "task-job".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: task, .. } = task else {
            panic!("the task must win the cap-one reservation");
        };

        warmer.trigger(&project_id).await;
        assert_eq!(posts.load(Ordering::SeqCst), 0, "denied warm must not POST");
        assert_eq!(journal.count_task_or_warm_occupancy().await.unwrap(), 1);
        assert!(
            journal
                .list_history(AdmissionDomain::WarmBuild, &work_id)
                .await
                .unwrap()
                .is_empty(),
            "the denied warm does not become completed or failed"
        );

        controller
            .transition(&task, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &task,
                WarmAdmissionTransition::Live {
                    uid: "task-uid".into(),
                },
            )
            .await
            .unwrap();
        let released = controller.release_notifier().notified();
        controller
            .transition(
                &task,
                WarmAdmissionTransition::Terminal {
                    uid: "task-uid".into(),
                },
            )
            .await
            .unwrap();
        released.await;

        let post = posted.notified();
        tokio::pin!(post);
        tokio::time::timeout(std::time::Duration::from_secs(3), post)
            .await
            .expect("pending warm should retry after the admission backoff");
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "released capacity retries the pending warm"
        );
    }

    #[tokio::test]
    async fn concrete_k8s_warmer_keeps_failed_create_started_pending_without_posting() {
        let db = Database::open_in_memory().unwrap();
        let journal = Arc::new(AdmissionJournalRepository::new(db.clone()));
        let controller = Arc::new(BuildAdmissionController::new(
            Arc::clone(&journal),
            BuildAdmissionMode::Enforce,
            1,
            "epoch",
        ));
        let project_id = seed_project_with_ready_image(&db, "create-started-failure").await;
        let work_id = format!("graph-warm:{project_id}:unknown");
        let posts = Arc::new(AtomicUsize::new(0));
        let posted = Arc::new(Notify::new());
        let warmer = K8sGraphWarmer::with_dispatcher(
            KubernetesConfig::for_testing(),
            db.clone(),
            Arc::new(AdmissionStateRecordingDispatcher {
                journal: Arc::clone(&journal),
                work_id: work_id.clone(),
                posts: Arc::clone(&posts),
                posted: Arc::clone(&posted),
            }),
            Arc::new(FencedTerminalWatcher),
        )
        .with_warm_admission(controller);

        // Fail the real controller durable state transition after it reserves
        // the warm row, rather than substituting a fake WarmAdmission.
        reject_admission_create_started_for_test(&db, true).await;

        warmer.trigger(&project_id).await;
        assert_eq!(
            posts.load(Ordering::SeqCst),
            0,
            "a real-controller CreateStarted failure must perform zero POSTs"
        );
        assert_eq!(
            journal
                .list_history(AdmissionDomain::WarmBuild, &work_id)
                .await
                .unwrap()[0]
                .state,
            AdmissionState::Reserved,
            "the failed transition retains the coalesced warm reservation"
        );
        warmer.trigger(&project_id).await;
        assert_eq!(
            posts.load(Ordering::SeqCst),
            0,
            "an immediate retrigger coalesces onto the pending warm"
        );

        reject_admission_create_started_for_test(&db, false).await;
        let post = posted.notified();
        tokio::pin!(post);
        tokio::time::timeout(std::time::Duration::from_secs(3), post)
            .await
            .expect("pending warm should retry after the admission backoff");
        assert_eq!(
            posts.load(Ordering::SeqCst),
            1,
            "the retained warm retries after the journal transition becomes durable"
        );
    }

    #[test]
    fn classification_covers_every_dispatch_role_and_rejects_unknown() {
        for role in [
            "worker",
            "reviewer",
            "lead",
            "planner",
            "architect",
            "advocate",
            "adversary",
            "judge",
        ] {
            assert!(TaskRunRole::parse(Some(role)).is_some());
        }
        assert_eq!(TaskRunRole::parse(None), None);
        assert_eq!(TaskRunRole::parse(Some("mystery")), None);
    }

    #[tokio::test]
    async fn off_is_noop_and_unknown_is_bounded() {
        let controller = controller(BuildAdmissionMode::Off, 0);
        let permit = WarmAdmission::admit(&controller, warm("off"))
            .await
            .unwrap();
        controller
            .transition(&permit, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            0
        );
        for _ in 0..1025 {
            let _ = controller
                .admit_task_run(
                    None,
                    AdmissionDomain::TaskObservation,
                    "x".into(),
                    0,
                    "x".into(),
                )
                .await;
        }
        assert_eq!(controller.unclassified_observation_count().await, 1024);
    }

    #[tokio::test]
    async fn observe_permits_when_journal_reservation_is_unavailable() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        db.pool().close().await;
        let controller = BuildAdmissionController::new(
            Arc::new(AdmissionJournalRepository::new(db)),
            BuildAdmissionMode::Observe,
            1,
            "epoch",
        );

        assert!(
            WarmAdmission::admit(&controller, warm("journal-down"))
                .await
                .is_ok(),
            "Observe journal failures are telemetry-only and must not defer dispatch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_records_serialized_would_defer_without_denial_and_enforce_combines_domains() {
        let observed = Arc::new(controller(BuildAdmissionMode::Observe, 1));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first = {
            let observed = Arc::clone(&observed);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                WarmAdmission::admit(observed.as_ref(), warm("a")).await
            })
        };
        let second = {
            let observed = Arc::clone(&observed);
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                WarmAdmission::admit(observed.as_ref(), warm("b")).await
            })
        };
        assert!(first.await.unwrap().is_ok());
        assert!(second.await.unwrap().is_ok());
        assert_eq!(observed.would_defer_observation_count().await, 1);
        let enforced = controller(BuildAdmissionMode::Enforce, 1);
        let _ = enforced
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                0,
                "task-job".into(),
            )
            .await
            .unwrap();
        assert!(matches!(
            WarmAdmission::admit(&enforced, warm("warm")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));
    }

    #[tokio::test]
    async fn permits_are_idempotent_and_terminal_notifies_and_is_uid_fenced() {
        let controller = controller(BuildAdmissionMode::Enforce, 2);
        let first = WarmAdmission::admit(&controller, warm("same"))
            .await
            .unwrap();
        let second = WarmAdmission::admit(&controller, warm("same"))
            .await
            .unwrap();
        assert_eq!(first, second);
        controller
            .transition(&first, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(&first, WarmAdmissionTransition::Live { uid: "uid".into() })
            .await
            .unwrap();
        assert!(
            controller
                .transition(
                    &first,
                    WarmAdmissionTransition::Terminal {
                        uid: "wrong".into()
                    }
                )
                .await
                .is_err()
        );
        let notified = controller.release_notifier().notified();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal { uid: "uid".into() },
            )
            .await
            .unwrap();
        notified.await;
        assert_eq!(
            controller
                .journal
                .list_history(AdmissionDomain::WarmBuild, "same")
                .await
                .unwrap()[0]
                .state,
            AdmissionState::Terminal
        );
    }

    #[tokio::test]
    async fn task_generations_and_runtime_uids_fence_terminal_release() {
        let controller = controller(BuildAdmissionMode::Enforce, 3);
        let first = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                1,
                "task-run-task-1".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: first, .. } = first else {
            panic!("task generation one must be admitted");
        };
        controller
            .transition(&first, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Live {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_some(),
            "generation one release must retain exactly one wakeup"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "generation one release must not retain a second wakeup"
        );

        // Repeating the matching terminal callback while generation one is
        // still current is idempotent and does not emit another wakeup.
        controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "duplicate generation-one terminal must not wake dispatch again"
        );

        let second = controller
            .admit_task_run(
                Some("worker"),
                AdmissionDomain::TaskObservation,
                "task".into(),
                2,
                "task-run-task-2".into(),
            )
            .await
            .unwrap();
        let BuildAdmissionDecision::Permitted { permit: second, .. } = second else {
            panic!("task generation two must be admitted");
        };
        controller
            .transition(&second, WarmAdmissionTransition::CreateStarted)
            .await
            .unwrap();
        controller
            .transition(
                &second,
                WarmAdmissionTransition::Live {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();

        // Once generation two exists, a delayed callback for the old
        // generation is stale and cannot release the newer row.
        let error = controller
            .transition(
                &first,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-one".into(),
                },
            )
            .await
            .expect_err("generation-one callback must be rejected as stale");
        assert_eq!(
            error,
            WarmAdmissionError::Unavailable {
                diagnostic: "invalid transition: stale admission generation 1 for task".into(),
            }
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "delayed old-generation callback must not wake dispatch"
        );
        let history = controller
            .journal
            .list_history(AdmissionDomain::TaskObservation, "task")
            .await
            .unwrap();
        assert_eq!(
            history
                .iter()
                .find(|row| row.key.generation == 2)
                .unwrap()
                .state,
            AdmissionState::Live
        );
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            1,
            "delayed old-generation duplicate must leave generation two occupied"
        );

        // A wrong UID and an unbound (UID-less) callback retain occupancy.
        assert!(
            controller
                .transition(
                    &second,
                    WarmAdmissionTransition::Terminal {
                        uid: "uid-one".into(),
                    },
                )
                .await
                .is_err()
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "wrong generation-two UID must not wake dispatch"
        );
        assert!(
            controller
                .task_run_permit_for_runtime_id("missing-uid")
                .await
                .is_none()
        );
        assert_eq!(
            controller
                .journal
                .count_task_or_warm_occupancy()
                .await
                .unwrap(),
            1,
            "UID-less terminal handling must retain generation-two occupancy"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "UID-less terminal handling must not wake dispatch"
        );

        controller
            .transition(
                &second,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_some(),
            "matching generation-two terminal must retain one wakeup"
        );
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "matching generation-two terminal must retain only one wakeup"
        );

        // A duplicate matching terminal callback is idempotent.
        controller
            .transition(
                &second,
                WarmAdmissionTransition::Terminal {
                    uid: "uid-two".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            controller
                .release_notifier()
                .notified()
                .now_or_never()
                .is_none(),
            "duplicate generation-two terminal must not wake dispatch again"
        );
    }
    #[tokio::test]
    async fn closed_enforce_controller_denies_until_recovery_marks_ready() {
        let controller = BuildAdmissionController::new_closed(
            Arc::new(AdmissionJournalRepository::new(
                Database::open_in_memory().unwrap(),
            )),
            1,
            "epoch",
        );
        assert!(!controller.is_ready());
        assert!(matches!(
            WarmAdmission::admit(&controller, warm("closed")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));
        controller.mark_ready();
        assert!(
            WarmAdmission::admit(&controller, warm("open"))
                .await
                .is_ok()
        );
    }

    fn predecessor_input(
        work_id: &str,
        generation: i64,
        epoch: &str,
    ) -> djinn_db::ReserveAdmissionInput {
        djinn_db::ReserveAdmissionInput {
            key: djinn_db::AdmissionJournalKey {
                domain: AdmissionDomain::WarmBuild,
                work_id: work_id.into(),
                generation,
            },
            workload_kind: djinn_db::AdmissionWorkloadKind::Warm,
            creator_server_epoch: epoch.into(),
            object_name: format!("warm-{work_id}-{generation}"),
        }
    }

    #[tokio::test]
    async fn enforce_recovery_alone_stays_closed_until_inventory_and_topology_complete() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 1, "replacement-epoch");
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::JournalRecoveryIncomplete,
            "Enforce starts fail-closed with the journal-recovery-incomplete gate"
        );
        assert!(matches!(
            WarmAdmission::admit(&controller, warm("denied-before-recovery")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));
        let report = controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        assert_eq!(report.retired_reserved, 0);
        assert_eq!(report.marked_create_unknown, 0);
        assert_eq!(report.seeded_rows, 0);
        // Journal recovery alone must NOT mark Enforce healthy: even with an
        // empty journal the inventory gate keeps admission fail-closed until
        // the real Kubernetes inventory completes.
        assert_eq!(
            report.readiness,
            BuildAdmissionReadiness::InventoryPending,
            "journal recovery advances to inventory-pending, never straight to healthy"
        );
        assert!(!controller.is_ready());
        assert!(
            matches!(
                WarmAdmission::admit(&controller, warm("denied-before-inventory")).await,
                Err(WarmAdmissionError::Denied { .. })
            ),
            "admission stays fail-closed while the inventory gate is pending"
        );

        controller.mark_inventory_ready();
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::TopologyPending,
            "completed inventory advances the gate to topology-pending"
        );
        assert!(
            matches!(
                WarmAdmission::admit(&controller, warm("denied-before-topology")).await,
                Err(WarmAdmissionError::Denied { .. })
            ),
            "admission stays fail-closed while the topology gate is pending"
        );

        controller.mark_topology_ready();
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
        assert!(controller.is_ready());
        assert!(
            WarmAdmission::admit(&controller, warm("after-all-gates"))
                .await
                .is_ok(),
            "admission opens only after journal + inventory + topology all complete"
        );
    }

    #[tokio::test]
    async fn recovery_retires_predecessor_reserved_and_seeds_occupancy_without_duplicates() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        // Predecessor rows from the old epoch.
        journal
            .reserve(&predecessor_input("reserved", 0, "old-epoch"), 5)
            .await
            .unwrap();
        journal
            .reserve(&predecessor_input("in-flight", 0, "old-epoch"), 5)
            .await
            .unwrap();
        journal
            .reserve(&predecessor_input("unknown", 0, "old-epoch"), 5)
            .await
            .unwrap();
        journal
            .reserve(&predecessor_input("live", 0, "old-epoch"), 5)
            .await
            .unwrap();
        // Mark in-flight and advance the others.
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "in-flight".into(),
                    generation: 0,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-in-flight-0".into(),
            })
            .await
            .unwrap();
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "unknown".into(),
                    generation: 0,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-unknown-0".into(),
            })
            .await
            .unwrap();
        journal
            .mark_create_unknown(&djinn_db::AdmissionJournalKey {
                domain: AdmissionDomain::WarmBuild,
                work_id: "unknown".into(),
                generation: 0,
            })
            .await
            .unwrap();
        journal
            .mark_create_started(&djinn_db::CreateStartedInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "live".into(),
                    generation: 0,
                },
                creator_server_epoch: "old-epoch".into(),
                object_name: "warm-live-0".into(),
            })
            .await
            .unwrap();
        journal
            .mark_live(&djinn_db::UidFencedAdmissionInput {
                key: djinn_db::AdmissionJournalKey {
                    domain: AdmissionDomain::WarmBuild,
                    work_id: "live".into(),
                    generation: 0,
                },
                object_uid: "uid-live".into(),
            })
            .await
            .unwrap();
        assert_eq!(
            journal.count_task_or_warm_occupancy().await.unwrap(),
            4,
            "four predecessor rows occupy before recovery"
        );

        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 64, "replacement-epoch");
        let report = controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        assert_eq!(report.retired_reserved, 1, "predecessor Reserved retired");
        assert_eq!(
            report.marked_create_unknown, 1,
            "predecessor CreateInFlight converted to CreateUnknown"
        );
        assert_eq!(
            report.seeded_rows, 3,
            "in-flight(now unknown), unknown, and live seeded"
        );
        // The predecessor Reserved row no longer occupies; the converted
        // in-flight row now occupies as CreateUnknown.
        assert_eq!(
            journal.count_task_or_warm_occupancy().await.unwrap(),
            3,
            "retired Reserved releases one slot; CreateUnknown still occupies"
        );
        assert_eq!(
            report.readiness,
            BuildAdmissionReadiness::CreateUnknownHealth,
            "CreateUnknown rows gate readiness"
        );
        assert!(!controller.is_ready());

        // The seeded permits are addressable through the domain-appropriate
        // recovered-permit lookup: these are WarmBuild rows, so the lookup
        // must key on `AdmissionDomain::WarmBuild` (the task-run accessor
        // deliberately filters to `AdmissionDomain::TaskObservation`).
        let live_permit = controller
            .permit_for_key(AdmissionDomain::WarmBuild, "live", 0)
            .await
            .expect("seeded live warm permit is addressable");
        let mut unknown_permits = Vec::new();
        for work in ["in-flight", "unknown"] {
            unknown_permits.push(
                controller
                    .permit_for_key(AdmissionDomain::WarmBuild, work, 0)
                    .await
                    .expect("seeded CreateUnknown warm permit is addressable"),
            );
        }
        assert!(
            controller.task_run_permit("live", 0).await.is_none(),
            "the task-run accessor must not return warm-build rows"
        );

        // Adopting each recovered CreateUnknown row into Live (authoritative
        // GET/UID proof) clears the CreateUnknown startup gate; readiness then
        // falls through to the still-pending inventory gate.
        for (index, permit) in unknown_permits.iter().enumerate() {
            controller
                .transition(
                    permit,
                    WarmAdmissionTransition::Live {
                        uid: format!("adopted-uid-{index}"),
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::InventoryPending,
            "adopting every CreateUnknown row advances the gate to inventory-pending"
        );
        assert!(!controller.is_ready());
        controller.mark_inventory_ready();
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::TopologyPending
        );
        controller.mark_topology_ready();
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);

        // The seeded permits are idempotent: re-admitting the same key returns
        // the seeded permit without consuming a new slot.
        let retry = WarmAdmission::admit(&controller, warm("live"))
            .await
            .unwrap();
        assert_eq!(
            retry, live_permit,
            "re-admission returns the seeded permit without duplicating occupancy"
        );
        assert_eq!(
            journal.count_task_or_warm_occupancy().await.unwrap(),
            3,
            "idempotent re-admission does not add occupancy"
        );
    }

    #[tokio::test]
    async fn seeded_occupancy_above_cap_gates_readiness_fail_closed() {
        let journal = Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        ));
        // Two predecessor Live rows under a cap of one.
        for work in ["over-a", "over-b"] {
            journal
                .reserve(&predecessor_input(work, 0, "old-epoch"), 5)
                .await
                .unwrap();
            journal
                .mark_create_started(&djinn_db::CreateStartedInput {
                    key: djinn_db::AdmissionJournalKey {
                        domain: AdmissionDomain::WarmBuild,
                        work_id: work.into(),
                        generation: 0,
                    },
                    creator_server_epoch: "old-epoch".into(),
                    object_name: format!("warm-{work}-0"),
                })
                .await
                .unwrap();
            journal
                .mark_live(&djinn_db::UidFencedAdmissionInput {
                    key: djinn_db::AdmissionJournalKey {
                        domain: AdmissionDomain::WarmBuild,
                        work_id: work.into(),
                        generation: 0,
                    },
                    object_uid: format!("uid-{work}"),
                })
                .await
                .unwrap();
        }
        let controller =
            BuildAdmissionController::new_closed(Arc::clone(&journal), 1, "replacement-epoch");
        let report = controller
            .recover_all_predecessors_and_seed()
            .await
            .unwrap();
        assert_eq!(report.seeded_rows, 2);
        assert_eq!(
            report.readiness,
            BuildAdmissionReadiness::SeededOccupancyAboveCap,
            "seeded occupancy above cap must gate readiness"
        );
        assert!(!controller.is_ready());
        assert!(matches!(
            WarmAdmission::admit(&controller, warm("denied-over-cap")).await,
            Err(WarmAdmissionError::Denied { .. })
        ));

        // Terminal releases bring durable occupancy back within the cap; the
        // over-cap gate clears from the journal count and readiness falls
        // through to the still-pending inventory gate.
        for (work, uid) in [("over-a", "uid-over-a"), ("over-b", "uid-over-b")] {
            let permit = controller
                .permit_for_key(AdmissionDomain::WarmBuild, work, 0)
                .await
                .expect("seeded over-cap permit is addressable");
            controller
                .transition(
                    &permit,
                    WarmAdmissionTransition::Terminal { uid: uid.into() },
                )
                .await
                .unwrap();
        }
        assert_eq!(journal.count_task_or_warm_occupancy().await.unwrap(), 0);
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::InventoryPending,
            "clearing the over-cap gate does not skip the inventory gate"
        );
        controller.mark_inventory_ready();
        controller.mark_topology_ready();
        assert_eq!(controller.readiness(), BuildAdmissionReadiness::Healthy);
        assert!(
            WarmAdmission::admit(&controller, warm("after-drain"))
                .await
                .is_ok(),
            "admission opens once occupancy is within the cap and all gates complete"
        );
    }

    #[tokio::test]
    async fn observe_and_off_do_not_gate_admission_on_readiness() {
        // Observe records degradation but never denies; the readiness value is
        // inspectable for telemetry.
        let observe = controller(BuildAdmissionMode::Observe, 1);
        observe.mark_journal_unhealthy();
        assert_eq!(
            observe.readiness(),
            BuildAdmissionReadiness::JournalUnhealthy
        );
        assert!(
            WarmAdmission::admit(&observe, warm("observe-degraded"))
                .await
                .is_ok(),
            "Observe must not deny even when readiness is degraded"
        );

        // Off has no readiness coupling and never touches the journal.
        let off = controller(BuildAdmissionMode::Off, 0);
        off.mark_inventory_pending();
        assert!(
            WarmAdmission::admit(&off, warm("off-uncoupled"))
                .await
                .is_ok(),
            "Off has no readiness coupling"
        );
    }

    #[tokio::test]
    async fn shutdown_draining_blocks_new_enforce_reservations() {
        let controller = controller(BuildAdmissionMode::Enforce, 1);
        // A ready controller that begins draining must block every new
        // reservation, regardless of prior occupancy. The drain gate is checked
        // before any journal reservation, so this is independent of DB state.
        controller.mark_ready();
        assert!(controller.is_ready());
        controller.begin_draining();
        assert!(controller.is_draining());
        assert_eq!(
            controller.readiness(),
            BuildAdmissionReadiness::ShutdownDraining
        );
        assert!(
            matches!(
                WarmAdmission::admit(&controller, warm("during-drain")).await,
                Err(WarmAdmissionError::Denied { .. })
            ),
            "draining blocks new Enforce reservations"
        );
    }

    #[test]
    fn allocate_server_epoch_is_unique() {
        let a = allocate_server_epoch();
        let b = allocate_server_epoch();
        assert!(!a.is_empty());
        assert!(!b.is_empty());
        assert_ne!(a, b, "each allocated epoch is unique");
    }
}
