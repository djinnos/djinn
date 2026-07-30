//! Production wiring for the observe-only disk dimension (proposal nquz,
//! phase 1 — OBSERVE).
//!
//! Phase 1 shipped three pure pieces and no call sites: the run-dir ledger
//! ([`djinn_db::RunDirRepository`]), the reconciliation planner
//! ([`crate::run_dir_reconcile`]), and the disk evaluator plus quota probe
//! ([`crate::disk_admission`]). This module is the executor that composes them
//! at coordinator startup.
//!
//! Kueue cutover (o53p): it used to also install the capacity adapter on the
//! production `BuildAdmissionController`. That pre-create reservation authority
//! is deleted, so [`CoordinatorDiskCapacitySource::observe`] is now an
//! unattached seam — see [`arm_disk_observation`]. Proposal nquz owns giving it
//! a decision to feed.
//!
//! # What this module may NOT do
//!
//! Observe mode is read-only with respect to the filesystem and to admission:
//!
//! - it never creates, renames, deletes, or truncates anything on the volume —
//!   the only writes it performs at all are `run_dirs` ledger rows;
//! - it never reserves bytes, assigns a quota, or allocates a temp/final path;
//! - it never changes a grant. [`CoordinatorDiskCapacitySource::observe`] feeds
//!   telemetry alone and always has.
//!
//! Unknown, malformed, symlinked, unreadable, and unresolved entries become
//! [`RunDirState::QuarantinedUnowned`] and are left untouched on disk. The whole
//! executor is idempotent: [`djinn_db::RunDirRepository::upsert_reconciled`]
//! leaves a pre-existing `(volume_id, pod_uid)` row alone, so a rerun observes
//! the same ledger.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use djinn_core::clock::Clock;
use djinn_db::{
    BuildLeaseRepository, BuildLeaseRow, BuildLeaseState, Database, RunDirRepository, RunDirState,
};

use crate::build_admission::{BuildWorkloadKind, TaskRunRole};
use crate::cargo_warm_base_gc::{CapacitySnapshot, FilesystemCapacity, StatvfsFilesystemCapacity};
use crate::disk_admission::{
    CapacitySample, DiskAdmissionConfig, DiskAdmissionRequest, DiskCapacityState, DiskObservation,
    LedgerTotals, QuotaSupport, QuotaUnavailableReason, evaluate_disk_admission_observe,
    probe_project_quota,
};
use crate::run_dir_reconcile::{
    ReconcileInventoryEntry, ResolvedOwnership, RunDirOwnershipResolver, plan_reconciliation,
};

/// Single configured volume ID for the pre-`8ixk` (single-volume) phase. When
/// node-keyed pools land, the same ledger activates with `volume_id =
/// node_volume_id`; until then every row on this coordinator shares this ID.
pub const DEFAULT_VOLUME_ID: &str = "cargo-target-runs";
pub const VOLUME_ID_ENV: &str = "DJINN_RUN_DIR_VOLUME_ID";

/// The configured single-volume ID, defaulting to [`DEFAULT_VOLUME_ID`].
#[must_use]
pub fn volume_id_from_env() -> String {
    std::env::var(VOLUME_ID_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_VOLUME_ID.to_owned())
}

// ── Filesystem inventory seam ───────────────────────────────────────────────

/// One volume scan: the per-directory entries the planner consumes, plus the
/// bytes held by top-level non-directory debris. Debris is not a run dir and
/// therefore has no ledger row, but it DOES occupy the volume, so it is
/// reported as unowned bytes rather than silently dropped from accounting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunDirInventory {
    pub entries: Vec<ReconcileInventoryEntry>,
    pub loose_file_bytes: u64,
}

/// Read-only inventory of the configured runs root.
///
/// A seam so startup tests can drive an unreadable or adversarial volume
/// deterministically. Implementations MUST NOT mutate the filesystem.
pub trait RunDirInventorySource: Send + Sync {
    fn inventory(&self, root: &Path) -> Result<RunDirInventory, String>;
}

/// The production inventory: [`djinn_core::cargo_target_runs`] measurement,
/// `st_blocks`-based, inode-deduplicated, never following symlinks.
#[derive(Clone, Copy, Debug, Default)]
pub struct FilesystemRunDirInventory;

/// A well-formed run dir is named by a task-run UUID. Anything else — debris,
/// a rename, an operator scratch dir — is malformed and quarantined.
fn is_task_run_dir_name(name: &str) -> bool {
    uuid::Uuid::parse_str(name).is_ok()
}

impl RunDirInventorySource for FilesystemRunDirInventory {
    fn inventory(&self, root: &Path) -> Result<RunDirInventory, String> {
        let scan = djinn_core::cargo_target_runs::inventory_cargo_target_runs(root)
            .map_err(|error| error.to_string())?;
        let mut entries: Vec<ReconcileInventoryEntry> = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        // Measurable, contained directories. A non-UUID name is malformed even
        // though the inventory accepted it as a candidate.
        for candidate in &scan.candidates {
            let Ok(name) = std::str::from_utf8(&candidate.name) else {
                continue;
            };
            let malformed = !is_task_run_dir_name(name);
            let measured =
                djinn_core::cargo_target_runs::measure_run_dir_allocated_bytes(root, name)
                    .ok()
                    .flatten()
                    .unwrap_or(0);
            seen.push(name.to_owned());
            entries.push(ReconcileInventoryEntry {
                dir_name: name.to_owned(),
                measured_bytes: measured,
                final_path: root.join(name).to_string_lossy().into_owned(),
                malformed,
            });
        }

        // Symlinked, malformed-name, and unreadable top-level entries. These are
        // ALWAYS quarantined; an unreadable subtree measures as zero tracked
        // bytes rather than being mistaken for a small directory.
        for issue in scan.protected.iter().chain(scan.errors.iter()) {
            let Some(raw) = issue.top_level_name.as_deref() else {
                continue;
            };
            let Ok(name) = std::str::from_utf8(raw) else {
                continue;
            };
            if seen.iter().any(|existing| existing == name) {
                continue;
            }
            seen.push(name.to_owned());
            entries.push(ReconcileInventoryEntry {
                dir_name: name.to_owned(),
                measured_bytes: 0,
                final_path: root.join(name).to_string_lossy().into_owned(),
                malformed: true,
            });
        }

        entries.sort_by(|left, right| left.dir_name.cmp(&right.dir_name));
        Ok(RunDirInventory {
            entries,
            loose_file_bytes: scan.top_level_non_directory_allocated_bytes,
        })
    }
}

// ── Authoritative ownership resolution ──────────────────────────────────────

/// Resolve run directories from durable pod-UID evidence.
///
/// `build_leases.bound_pod_uid` is the only pod UID this database durably
/// records: the schema constraint admits it solely in the pod-bound and terminal
/// states and only the fencing-token `bind` path writes it. A directory with no
/// matching bound lease has NO authoritative owner and is quarantined — which is
/// exactly the mixed-version behaviour the rollout section requires while
/// workers still eager-seed.
#[derive(Clone, Debug, Default)]
pub struct LeaseOwnershipResolver {
    by_task_run: HashMap<String, ResolvedOwnership>,
}

/// A task-invocation lease's `immutable_identity` is `task:<task>:<run>:<inv>`.
/// Extract the task-run ID, which is also the run directory's name.
#[must_use]
pub fn task_run_id_from_immutable_identity(identity: &str) -> Option<&str> {
    let mut parts = identity.split(':');
    if parts.next()? != "task" {
        return None;
    }
    let _task_id = parts.next()?;
    let task_run_id = parts.next()?;
    (!task_run_id.is_empty()).then_some(task_run_id)
}

impl LeaseOwnershipResolver {
    /// Build the resolver from pod-bound task-invocation leases.
    ///
    /// Reconciliation asserts only two of the three authoritative states:
    /// terminal leases become [`RunDirState::Reclaimable`] (the pod-UID terminal
    /// proof landed) and every other pod-bound state becomes
    /// [`RunDirState::ReadyActive`]. [`RunDirState::ReadyIdle`] is deliberately
    /// never asserted here: the durable ledger carries no lease-recency evidence
    /// across a restart, and `ready_active` is the strictly more protected of
    /// the two.
    #[must_use]
    pub fn from_lease_rows(rows: &[BuildLeaseRow], project_ids: &HashMap<String, String>) -> Self {
        let mut by_task_run: HashMap<String, ResolvedOwnership> = HashMap::new();
        for row in rows {
            let Some(pod_uid) = row.bound_pod_uid.as_deref() else {
                continue;
            };
            if pod_uid.trim().is_empty() {
                continue;
            }
            let Some(task_run_id) = task_run_id_from_immutable_identity(&row.immutable_identity)
            else {
                continue;
            };
            let state = match row.state {
                BuildLeaseState::Terminal => RunDirState::Reclaimable,
                BuildLeaseState::Bound | BuildLeaseState::Active | BuildLeaseState::Suspect => {
                    RunDirState::ReadyActive
                }
                // Queued/Granted/Launching cannot carry a bound pod UID; if the
                // constraint ever loosened, refuse to claim ownership.
                BuildLeaseState::Queued | BuildLeaseState::Granted | BuildLeaseState::Launching => {
                    continue;
                }
            };
            by_task_run.insert(
                task_run_id.to_owned(),
                ResolvedOwnership {
                    pod_uid: pod_uid.to_owned(),
                    task_run_id: task_run_id.to_owned(),
                    project_id: project_ids.get(task_run_id).cloned(),
                    base_fingerprint: None,
                    state,
                },
            );
        }
        Self { by_task_run }
    }
}

impl RunDirOwnershipResolver for LeaseOwnershipResolver {
    fn resolve(&self, dir_name: &str) -> Option<ResolvedOwnership> {
        self.by_task_run.get(dir_name).cloned()
    }
}

// ── Startup reconciliation executor ─────────────────────────────────────────

/// Bounded outcome of one startup reconciliation. Every field is a count or a
/// byte total — no task-run, pod-UID, or project identifier appears here, so the
/// report is safe to publish as telemetry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunDirReconcileReport {
    pub scanned: usize,
    pub resolved: usize,
    pub quarantined: usize,
    pub resolved_bytes: u64,
    pub quarantined_bytes: u64,
    /// Bytes held by top-level non-directory debris; unowned, never deleted.
    pub loose_file_bytes: u64,
    /// Ledger upserts that failed. Reconciliation is best effort: a failed row
    /// is retried on the next startup, never worked around by a deletion.
    pub upsert_errors: usize,
    /// The volume could not be inventoried at all (missing root, unreadable).
    pub inventory_failed: bool,
}

impl RunDirReconcileReport {
    /// Total bytes on the volume that reconciliation could not attribute to an
    /// authoritative owner. Enforce activation requires this to be zero.
    #[must_use]
    pub fn unowned_bytes(&self) -> u64 {
        self.quarantined_bytes.saturating_add(self.loose_file_bytes)
    }
}

/// Inventory the configured volume, resolve ownership from durable evidence,
/// and apply the reconciliation plan to the run-dir ledger.
///
/// Idempotent and non-destructive: no directory is created, moved, or removed,
/// and a rerun re-observes the same rows because `upsert_reconciled` never
/// overwrites an existing `(volume_id, pod_uid)`.
pub async fn reconcile_run_dirs_at_startup(
    db: &Database,
    root: &Path,
    volume_id: &str,
    inventory_source: &dyn RunDirInventorySource,
) -> RunDirReconcileReport {
    let inventory = match inventory_source.inventory(root) {
        Ok(inventory) => inventory,
        Err(error) => {
            tracing::warn!(
                %error,
                root = %root.display(),
                "run_dir_observe: volume inventory unavailable; ledger left unchanged"
            );
            return RunDirReconcileReport {
                inventory_failed: true,
                ..RunDirReconcileReport::default()
            };
        }
    };

    let resolver = build_lease_resolver(db, &inventory.entries).await;
    let plan = plan_reconciliation(volume_id, &inventory.entries, &resolver);

    let ledger = RunDirRepository::new(db.clone());
    let mut upsert_errors = 0_usize;
    for upsert in &plan.upserts {
        if let Err(error) = ledger.upsert_reconciled(upsert).await {
            upsert_errors += 1;
            tracing::warn!(
                %error,
                state = upsert.state.as_str(),
                "run_dir_observe: reconciled run-dir upsert failed; retried at next startup"
            );
        }
    }

    let report = RunDirReconcileReport {
        scanned: inventory.entries.len(),
        resolved: plan.resolved_count,
        quarantined: plan.quarantined_count,
        resolved_bytes: plan.resolved_bytes,
        quarantined_bytes: plan.quarantined_bytes,
        loose_file_bytes: inventory.loose_file_bytes,
        upsert_errors,
        inventory_failed: false,
    };
    tracing::info!(
        root = %root.display(),
        scanned = report.scanned,
        resolved = report.resolved,
        quarantined = report.quarantined,
        resolved_bytes = report.resolved_bytes,
        unowned_bytes = report.unowned_bytes(),
        upsert_errors = report.upsert_errors,
        "run_dir_observe: startup reconciliation complete (observe-only; nothing deleted)"
    );
    report
}

/// Compose the authoritative resolver from durable lease evidence, enriched with
/// each resolved run's project ID. A failed read yields an empty resolver, which
/// quarantines everything — the fail-safe direction.
async fn build_lease_resolver(
    db: &Database,
    entries: &[ReconcileInventoryEntry],
) -> LeaseOwnershipResolver {
    let rows = match BuildLeaseRepository::new(db.clone())
        .list_pod_bound_task_invocations()
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                %error,
                "run_dir_observe: pod-bound lease evidence unavailable; every dir stays unowned"
            );
            return LeaseOwnershipResolver::default();
        }
    };
    let task_runs = djinn_db::TaskRunRepository::new(db.clone());
    let mut project_ids: HashMap<String, String> = HashMap::new();
    for entry in entries {
        if entry.malformed {
            continue;
        }
        if let Ok(Some(record)) = task_runs.get(&entry.dir_name).await {
            project_ids.insert(entry.dir_name.clone(), record.project_id);
        }
    }
    LeaseOwnershipResolver::from_lease_rows(&rows, &project_ids)
}

/// Publish the bounded run-dir gauges for one volume.
///
/// Labels are the closed [`RunDirState`] family only; the reconciliation report
/// contributes unowned debris bytes that have no ledger row.
pub async fn publish_run_dir_telemetry(
    db: &Database,
    volume_id: &str,
    loose_file_bytes: u64,
) -> Result<(), String> {
    let totals = RunDirRepository::new(db.clone())
        .volume_state_totals(volume_id)
        .await
        .map_err(|error| error.to_string())?;
    let mut reserved_total = 0_u64;
    let mut unowned_total = loose_file_bytes;
    for state in RunDirState::ALL {
        let row = totals.iter().find(|row| row.state == state);
        let count = row.map_or(0, |row| row.count.max(0) as u64);
        let measured = row.map_or(0, |row| row.measured_bytes.max(0) as u64);
        reserved_total =
            reserved_total.saturating_add(row.map_or(0, |row| row.reserved_bytes.max(0) as u64));
        if state == RunDirState::QuarantinedUnowned {
            unowned_total = unowned_total.saturating_add(measured);
        }
        djinn_telemetry::run_dir::set_state_count(metric_state_label(state), count);
        djinn_telemetry::run_dir::set_state_allocated_bytes(metric_state_label(state), measured);
    }
    djinn_telemetry::run_dir::set_reserved_bytes(reserved_total);
    djinn_telemetry::run_dir::set_unowned_bytes(unowned_total);
    Ok(())
}

/// Map a ledger state onto its bounded telemetry label. The `&'static str`
/// constants come from `djinn-telemetry`, keeping the label domain closed.
#[must_use]
pub fn metric_state_label(state: RunDirState) -> &'static str {
    use djinn_telemetry::run_dir as m;
    match state {
        RunDirState::Absent => m::STATE_ABSENT,
        RunDirState::Reserved => m::STATE_RESERVED,
        RunDirState::Seeding => m::STATE_SEEDING,
        RunDirState::ReadyActive => m::STATE_READY_ACTIVE,
        RunDirState::ReadyIdle => m::STATE_READY_IDLE,
        RunDirState::Reclaimable => m::STATE_RECLAIMABLE,
        RunDirState::Reclaiming => m::STATE_RECLAIMING,
        RunDirState::QuarantinedUnowned => m::STATE_QUARANTINED_UNOWNED,
    }
}

// ── Quota probe ─────────────────────────────────────────────────────────────

/// The startup quota-probe result and what it permits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaProbeReport {
    pub support: QuotaSupport,
    /// True when this volume may never be armed for `enforce`. Observe mode is
    /// unaffected: an unavailable probe is recorded, not a startup failure.
    pub enforce_prohibited: bool,
}

/// Probe the volume for filesystem project-quota support and record the result.
///
/// Read-only. Enforce mode is prohibited without working project quotas, but
/// observe-mode startup MUST NOT fail on an unavailable probe — the whole point
/// of observe is to run on volumes that cannot yet be enforced.
pub fn report_quota_probe(root: &Path) -> QuotaProbeReport {
    record_quota_support(root, probe_project_quota(root))
}

/// Classify and record an already-obtained quota answer.
///
/// Split out from [`report_quota_probe`] so both branches are reachable in a
/// test: the live probe reads `/proc/mounts`, which a test cannot make report
/// `prjquota` on demand.
#[must_use]
pub fn record_quota_support(root: &Path, support: QuotaSupport) -> QuotaProbeReport {
    match &support {
        QuotaSupport::Available => {
            tracing::info!(
                root = %root.display(),
                "run_dir_observe: project quotas available; a future enforce is permitted"
            );
            QuotaProbeReport {
                support,
                enforce_prohibited: false,
            }
        }
        QuotaSupport::Unavailable { reason } => {
            djinn_telemetry::run_dir::increment_quota_failure(reason.as_metric());
            tracing::warn!(
                root = %root.display(),
                reason = ?reason,
                "run_dir_observe: project quotas unavailable; observe continues, enforce prohibited"
            );
            QuotaProbeReport {
                support,
                enforce_prohibited: true,
            }
        }
    }
}

/// Bounded log/diagnostic label for a quota outcome.
#[must_use]
pub fn quota_metric_label(support: &QuotaSupport) -> &'static str {
    match support {
        QuotaSupport::Available => "available",
        QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoMatchingMount,
        } => "no_matching_mount",
        QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoQuotaMountOption,
        } => "no_quota_mount_option",
        QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::MountsUnreadable,
        } => "mounts_unreadable",
    }
}

// ── Live capacity adapter ───────────────────────────────────────────────────

/// Monotonic clock seam so sample-staleness is deterministically testable.
pub trait ObserveClock: Send + Sync {
    fn now(&self) -> Instant;
}

/// Production clock, delegating to the shared [`djinn_core::clock::Clock`]
/// abstraction rather than reading the wall clock directly.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemObserveClock;

impl ObserveClock for SystemObserveClock {
    fn now(&self) -> Instant {
        djinn_core::clock::SystemClock::new().now_instant()
    }
}

/// Adapts the coordinator's existing [`FilesystemCapacity`] snapshot to an
/// observe-only disk sample.
///
/// The adapter is read-only and never denies. A live probe produces a fresh
/// sample; a failed probe falls back to the last good snapshot with its true
/// age (which the evaluator treats as stale once it passes `max_sample_age`);
/// with no history at all the sample is `Unknown`.
pub struct CoordinatorDiskCapacitySource {
    volume_id: String,
    root: PathBuf,
    config: DiskAdmissionConfig,
    capacity: Arc<dyn FilesystemCapacity>,
    ledger: Arc<RunDirRepository>,
    clock: Arc<dyn ObserveClock>,
    /// Conservative per-request seed projection: the largest authoritative run
    /// dir observed at startup reconciliation, or 0 with no history. The
    /// evaluator rounds it up by 10% before adding the growth allowance.
    projected_seed_bytes: u64,
    last_good: Mutex<Option<(CapacitySnapshot, Instant)>>,
}

impl CoordinatorDiskCapacitySource {
    #[must_use]
    pub fn new(
        volume_id: String,
        root: PathBuf,
        config: DiskAdmissionConfig,
        capacity: Arc<dyn FilesystemCapacity>,
        ledger: Arc<RunDirRepository>,
        clock: Arc<dyn ObserveClock>,
        projected_seed_bytes: u64,
    ) -> Self {
        Self {
            volume_id,
            root,
            config,
            capacity,
            ledger,
            clock,
            projected_seed_bytes,
            last_good: Mutex::new(None),
        }
    }

    /// The production adapter over `statvfs` and the durable ledger.
    #[must_use]
    pub fn production(
        volume_id: String,
        root: PathBuf,
        config: DiskAdmissionConfig,
        db: &Database,
        projected_seed_bytes: u64,
    ) -> Self {
        Self::new(
            volume_id,
            root,
            config,
            Arc::new(StatvfsFilesystemCapacity),
            Arc::new(RunDirRepository::new(db.clone())),
            Arc::new(SystemObserveClock),
            projected_seed_bytes,
        )
    }

    /// Take (or age) a capacity sample for the volume.
    pub fn sample(&self) -> CapacitySample {
        let now = self.clock.now();
        match self.capacity.capacity(&self.root) {
            Ok(snapshot) => {
                if let Ok(mut last) = self.last_good.lock() {
                    *last = Some((snapshot, now));
                }
                CapacitySample {
                    free_bytes: snapshot.available_bytes,
                    total_bytes: snapshot.total_bytes,
                    state: DiskCapacityState::classify(
                        snapshot.available_bytes,
                        self.config.critical_free_bytes,
                        self.config.effective_warning_free_bytes(),
                    ),
                    age: Duration::ZERO,
                }
            }
            Err(error) => {
                let previous = self.last_good.lock().ok().and_then(|last| *last);
                match previous {
                    Some((snapshot, at)) => {
                        tracing::debug!(
                            %error,
                            "run_dir_observe: capacity probe failed; ageing the last good sample"
                        );
                        CapacitySample {
                            free_bytes: snapshot.available_bytes,
                            total_bytes: snapshot.total_bytes,
                            state: DiskCapacityState::classify(
                                snapshot.available_bytes,
                                self.config.critical_free_bytes,
                                self.config.effective_warning_free_bytes(),
                            ),
                            age: now.saturating_duration_since(at),
                        }
                    }
                    None => {
                        tracing::debug!(
                            %error,
                            "run_dir_observe: capacity probe failed with no history; sample unknown"
                        );
                        CapacitySample {
                            free_bytes: 0,
                            total_bytes: 0,
                            state: DiskCapacityState::Unknown,
                            age: Duration::ZERO,
                        }
                    }
                }
            }
        }
    }

    async fn ledger_totals(&self) -> Option<LedgerTotals> {
        match self.ledger.volume_state_totals(&self.volume_id).await {
            Ok(totals) => {
                let mut tracked_measured_bytes = 0_u64;
                let mut outstanding_reserved_bytes = 0_u64;
                for row in totals {
                    tracked_measured_bytes =
                        tracked_measured_bytes.saturating_add(row.measured_bytes.max(0) as u64);
                    outstanding_reserved_bytes =
                        outstanding_reserved_bytes.saturating_add(row.reserved_bytes.max(0) as u64);
                }
                Some(LedgerTotals {
                    tracked_measured_bytes,
                    outstanding_reserved_bytes,
                })
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "run_dir_observe: ledger totals unavailable; skipping this observation"
                );
                None
            }
        }
    }
}

/// Whether a workload actually runs the project's compile/test toolchain.
///
/// Light and explicitly non-build workloads never consult disk admission and
/// never seed. This is the guard that keeps a future routing change from
/// silently starting to charge orchestration-only work against a disk budget.
#[must_use]
pub fn workload_consults_disk(kind: BuildWorkloadKind) -> bool {
    match kind {
        BuildWorkloadKind::GraphWarmJob => true,
        BuildWorkloadKind::TaskRun { role } => {
            role.resource_class() != djinn_runtime::RoleResourceClass::Light
        }
        BuildWorkloadKind::NonBuild { .. } => false,
    }
}

/// Light roles, restated for the guard test so a role reclassification cannot
/// silently change what consults disk.
#[must_use]
pub fn is_light_role(role: TaskRunRole) -> bool {
    role.resource_class() == djinn_runtime::RoleResourceClass::Light
}

impl CoordinatorDiskCapacitySource {
    /// Observe-only disk pricing for one build workload.
    ///
    /// Kueue cutover (o53p): this used to implement `build_admission`'s
    /// `DiskCapacitySource` trait, whose only consumer was the deleted
    /// pre-create admission controller. Kueue's ClusterQueue quota does not
    /// model bytes of a shared PVC, so this observation still has to be made by
    /// hand — proposal nquz owns re-attaching it to a decision. Until then it is
    /// an inherent method taking the classification directly, so the seam
    /// survives the ledger's deletion without a trait that nothing implements.
    pub async fn observe(&self, kind: BuildWorkloadKind) -> Option<DiskObservation> {
        if !workload_consults_disk(kind) {
            return None;
        }
        let ledger = self.ledger_totals().await?;
        let sample = self.sample();
        // Phase 1 has no run-dir lease binding yet, so no request can claim a
        // zero-growth reuse of an already-ready directory. `false` is the
        // conservative reading: every observation is priced as a new
        // reservation.
        let disk_request = DiskAdmissionRequest {
            projected_seed_bytes: self.projected_seed_bytes,
            reuses_ready_dir: false,
        };
        Some(evaluate_disk_admission_observe(
            &self.config,
            &sample,
            &ledger,
            &disk_request,
        ))
    }
}

// ── Composition ─────────────────────────────────────────────────────────────

/// Everything one coordinator startup needs to arm the observe-only disk
/// dimension. The seams exist so the production composition below can be driven
/// deterministically in tests without a second implementation.
pub struct RunDirObserveSeams {
    pub root: PathBuf,
    pub volume_id: String,
    pub config: DiskAdmissionConfig,
    pub inventory: Arc<dyn RunDirInventorySource>,
    pub capacity: Arc<dyn FilesystemCapacity>,
    pub clock: Arc<dyn ObserveClock>,
}

impl RunDirObserveSeams {
    /// The production composition: the server pod's mount of the shared cache
    /// PVC, a real `st_blocks` inventory, and a real `statvfs` capacity probe.
    #[must_use]
    pub fn production() -> Self {
        Self {
            root: djinn_core::paths::cargo_target_runs_root(),
            volume_id: volume_id_from_env(),
            config: DiskAdmissionConfig::from_env(),
            inventory: Arc::new(FilesystemRunDirInventory),
            capacity: Arc::new(StatvfsFilesystemCapacity),
            clock: Arc::new(SystemObserveClock),
        }
    }
}

/// Bounded outcome of arming the observe-only disk dimension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunDirObserveReport {
    pub reconcile: RunDirReconcileReport,
    pub quota: QuotaProbeReport,
    /// The conservative per-request seed projection installed on the capacity
    /// source (bytes, pre-rounding).
    pub projected_seed_bytes: u64,
}

/// Run startup reconciliation, probe quotas and publish bounded telemetry.
///
/// This is the whole observe-mode activation. It performs no filesystem
/// mutation and cannot change an admission outcome.
///
/// # Kueue cutover (o53p): this no longer installs a capacity source
///
/// It used to end by handing a [`CoordinatorDiskCapacitySource`] to the
/// pre-create `BuildAdmissionController` via `set_disk_capacity_source`. That
/// controller was the reservation authority Kueue replaces, and it is deleted,
/// so there is nothing left to install onto — Kueue's ClusterQueue quota does
/// not model bytes of a shared PVC.
///
/// The reconciliation, quota probe, telemetry and seed projection below are all
/// still real and still run. What is gone is the (already observe-only) hookup
/// of a source that could never change an outcome. Proposal **nquz** owns
/// re-attaching disk observation to an actual decision; the seam it must attach
/// is [`CoordinatorDiskCapacitySource::observe`], which survives with its tests.
pub async fn arm_disk_observation(db: &Database, seams: RunDirObserveSeams) -> RunDirObserveReport {
    let reconcile =
        reconcile_run_dirs_at_startup(db, &seams.root, &seams.volume_id, seams.inventory.as_ref())
            .await;
    let quota = report_quota_probe(&seams.root);

    if let Err(error) =
        publish_run_dir_telemetry(db, &seams.volume_id, reconcile.loose_file_bytes).await
    {
        tracing::warn!(%error, "run_dir_observe: run-dir telemetry publish failed");
    }

    // Conservative seed projection: the largest authoritative run dir seen this
    // startup. With no authoritative history the projection is zero and the
    // reservation is the growth allowance alone.
    let projected_seed_bytes = largest_authoritative_run_dir_bytes(db, &seams.volume_id).await;

    tracing::info!(
        volume_id = %seams.volume_id,
        root = %seams.root.display(),
        resolved = reconcile.resolved,
        quarantined = reconcile.quarantined,
        unowned_bytes = reconcile.unowned_bytes(),
        quota = quota_metric_label(&quota.support),
        enforce_prohibited = quota.enforce_prohibited,
        projected_seed_bytes,
        "run_dir_observe: disk observation armed (observe-only; grants unaffected)"
    );

    RunDirObserveReport {
        reconcile,
        quota,
        projected_seed_bytes,
    }
}

/// The largest measured bytes among authoritative ready rows on a volume.
async fn largest_authoritative_run_dir_bytes(db: &Database, volume_id: &str) -> u64 {
    match RunDirRepository::new(db.clone())
        .list_by_volume(volume_id)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter(|row| {
                matches!(
                    row.state,
                    RunDirState::ReadyActive | RunDirState::ReadyIdle | RunDirState::Reclaimable
                )
            })
            .map(|row| row.measured_bytes.max(0) as u64)
            .max()
            .unwrap_or(0),
        Err(error) => {
            tracing::warn!(
                %error,
                "run_dir_observe: seed projection unavailable; defaulting to growth allowance only"
            );
            0
        }
    }
}

#[cfg(test)]
#[path = "run_dir_observe_tests.rs"]
mod tests;
