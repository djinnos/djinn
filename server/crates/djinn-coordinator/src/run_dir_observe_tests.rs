//! Tests for the observe-mode disk wiring.
//!
//! These drive the SAME functions the coordinator's startup path composes
//! ([`arm_disk_observation`] and [`reconcile_run_dirs_at_startup`]) against a
//! real temporary volume, a real ephemeral Postgres, and a real
//! [`BuildAdmissionController`]. Only the capacity probe and the clock are
//! substituted, because a test cannot make a real filesystem report critical
//! pressure or age a monotonic instant.

use std::collections::HashMap;
use std::os::unix::fs::symlink;
use std::sync::Arc;

use djinn_db::{
    AdmissionJournalRepository, BuildLeaseConsumerKind, BuildLeaseKey, BuildLeaseRepository,
    BuildLeaseState, Database, GrantNextBuildLeaseResult, QueueBuildLeaseInput,
    QueueBuildLeaseResult, RunDirRepository, RunDirState,
};

use super::*;
use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionDecision, BuildAdmissionMode, BuildAdmissionRequest,
    BuildWorkloadKind, LIGHT_ROLE_AUDIT_REASON, TaskRunRole,
};

const GIB: u64 = 1024 * 1024 * 1024;
const VOLUME: &str = "test-volume";

const LIVE_RUN: &str = "11111111-1111-1111-1111-111111111111";
const UNRESOLVED_RUN: &str = "33333333-3333-3333-3333-333333333333";
const MALFORMED_DIR: &str = "not-a-uuid";

// ── Fakes for the two seams a test cannot drive for real ────────────────────

struct FixedCapacity(Result<CapacitySnapshot, String>);

impl FilesystemCapacity for FixedCapacity {
    fn capacity(&self, _path: &Path) -> Result<CapacitySnapshot, String> {
        self.0.clone()
    }
}

/// A clock that reports a fixed offset past its construction instant, so a
/// fallback sample can be aged deterministically.
struct OffsetClock {
    base: Instant,
    offset: Duration,
}

impl ObserveClock for OffsetClock {
    fn now(&self) -> Instant {
        self.base + self.offset
    }
}

fn config() -> DiskAdmissionConfig {
    DiskAdmissionConfig {
        cache_budget_bytes: 100 * GIB,
        critical_free_bytes: 20 * GIB,
        warning_free_bytes: 40 * GIB,
        emergency_headroom_bytes: 10 * GIB,
        per_lease_growth_bytes: 4 * GIB,
        max_sample_age: Duration::from_secs(90),
    }
}

/// One volume with an authoritative run dir, an unresolved run dir, a malformed
/// directory name, a symlink, and top-level debris.
fn fixture_volume() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in [LIVE_RUN, UNRESOLVED_RUN, MALFORMED_DIR] {
        std::fs::create_dir(root.join(name)).unwrap();
        std::fs::write(root.join(name).join("payload"), vec![7_u8; 4096]).unwrap();
    }
    symlink(root.join(LIVE_RUN), root.join("dangling-link")).unwrap();
    std::fs::write(root.join("debris.log"), vec![1_u8; 2048]).unwrap();
    dir
}

fn controller(db: &Database) -> BuildAdmissionController {
    BuildAdmissionController::new(
        Arc::new(AdmissionJournalRepository::new(db.clone())),
        BuildAdmissionMode::Observe,
        4,
        "epoch",
    )
}

fn worker_request(id: &str) -> BuildAdmissionRequest {
    BuildAdmissionRequest {
        domain: djinn_db::AdmissionDomain::TaskObservation,
        work_id: id.to_owned(),
        generation: 0,
        object_name: format!("job-{id}"),
        kind: BuildWorkloadKind::TaskRun {
            role: TaskRunRole::Worker,
        },
    }
}

/// Drive a task-invocation lease all the way to pod-bound through the real
/// repository API, so the resolver reads exactly what the production `bind`
/// path writes.
async fn bind_lease(db: &Database, task_run_id: &str, pod_uid: &str) -> BuildLeaseKey {
    let repo = BuildLeaseRepository::new(db.clone());
    let key = BuildLeaseKey {
        consumer_kind: BuildLeaseConsumerKind::TaskInvocation,
        consumer_id: format!("inv-{task_run_id}"),
    };
    let queued = repo
        .queue(&QueueBuildLeaseInput {
            key: key.clone(),
            immutable_identity: format!("task:task-1:{task_run_id}:inv-{task_run_id}"),
            queue_deadline: None,
            launch_deadline: None,
        })
        .await
        .unwrap();
    assert!(matches!(queued, QueueBuildLeaseResult::Queued { .. }));
    let granted = repo
        .grant_next(4, "2026-07-25T00:00:00Z", None)
        .await
        .unwrap();
    let GrantNextBuildLeaseResult::Granted(row) = granted else {
        panic!("the queued lease must be grantable");
    };
    let token = row.fencing_token.unwrap();
    let bound = repo.bind(&key, token, pod_uid, None).await.unwrap();
    assert_eq!(bound.state, BuildLeaseState::Bound);
    assert_eq!(bound.bound_pod_uid.as_deref(), Some(pod_uid));
    key
}

// ── Inventory ───────────────────────────────────────────────────────────────

#[test]
fn inventory_classifies_run_dirs_symlinks_and_malformed_names() {
    let volume = fixture_volume();
    let inventory = FilesystemRunDirInventory
        .inventory(volume.path())
        .expect("a readable volume inventories");

    let by_name: HashMap<&str, &ReconcileInventoryEntry> = inventory
        .entries
        .iter()
        .map(|entry| (entry.dir_name.as_str(), entry))
        .collect();

    assert!(
        !by_name[LIVE_RUN].malformed,
        "a UUID run dir is well formed"
    );
    assert!(by_name[LIVE_RUN].measured_bytes > 0, "bytes are measured");
    assert!(
        by_name[MALFORMED_DIR].malformed,
        "a non-UUID directory name is malformed"
    );
    assert!(
        by_name["dangling-link"].malformed,
        "a top-level symlink is never a run dir"
    );
    assert!(
        inventory.loose_file_bytes > 0,
        "top-level debris is accounted as unowned bytes, not dropped"
    );
}

#[test]
fn inventory_of_a_missing_root_is_an_error_not_an_empty_volume() {
    let missing = tempfile::tempdir().unwrap();
    let root = missing.path().join("never-created");
    assert!(FilesystemRunDirInventory.inventory(&root).is_err());
}

// ── Ownership resolution ────────────────────────────────────────────────────

#[test]
fn immutable_identity_parsing_extracts_only_the_task_run_id() {
    assert_eq!(
        task_run_id_from_immutable_identity("task:t-1:run-9:inv-3"),
        Some("run-9")
    );
    assert_eq!(task_run_id_from_immutable_identity("warm:p:w:g"), None);
    assert_eq!(task_run_id_from_immutable_identity("task:t-1"), None);
    assert_eq!(task_run_id_from_immutable_identity("task:t-1::inv"), None);
}

// ── Startup reconciliation against the production path ──────────────────────

#[tokio::test]
async fn startup_reconciliation_resolves_authoritative_quarantines_the_rest_and_deletes_nothing() {
    let db = Database::open_in_memory().unwrap();
    let volume = fixture_volume();
    bind_lease(&db, LIVE_RUN, "pod-live-uid").await;

    let report = reconcile_run_dirs_at_startup(
        &db,
        volume.path(),
        VOLUME,
        &FilesystemRunDirInventory as &dyn RunDirInventorySource,
    )
    .await;

    assert!(!report.inventory_failed);
    assert_eq!(report.upsert_errors, 0);
    assert_eq!(report.resolved, 1, "only the bound lease resolves");
    assert_eq!(
        report.quarantined, 3,
        "unresolved UUID, malformed name, and symlink all quarantine"
    );
    assert!(report.resolved_bytes > 0);
    assert!(report.unowned_bytes() > 0);

    let ledger = RunDirRepository::new(db.clone());
    // The authoritative row is keyed by the POD UID from the durable lease, not
    // by the untrusted directory name.
    let live = ledger
        .get(&djinn_db::RunDirKey {
            volume_id: VOLUME.into(),
            pod_uid: "pod-live-uid".into(),
        })
        .await
        .unwrap()
        .expect("the authoritative run dir is reconciled");
    assert_eq!(live.state, RunDirState::ReadyActive);
    assert_eq!(live.task_run_id.as_deref(), Some(LIVE_RUN));
    assert_eq!(live.reserved_bytes, 0, "observe reserves no bytes");
    assert!(live.quota_id.is_none(), "observe assigns no quota");
    assert!(
        live.temp_path.is_none(),
        "observe creates no temp directory"
    );

    for unowned in [UNRESOLVED_RUN, MALFORMED_DIR, "dangling-link"] {
        let row = ledger
            .get(&djinn_db::RunDirKey {
                volume_id: VOLUME.into(),
                pod_uid: unowned.into(),
            })
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{unowned} must be recorded"));
        assert_eq!(row.state, RunDirState::QuarantinedUnowned);
        assert_eq!(row.task_run_id, None, "no untrusted task-run binding");
        assert_eq!(row.reserved_bytes, 0);
    }

    // Nothing on the volume was touched: every directory, the symlink, and the
    // debris file all survive reconciliation.
    for name in [
        LIVE_RUN,
        UNRESOLVED_RUN,
        MALFORMED_DIR,
        "dangling-link",
        "debris.log",
    ] {
        assert!(
            std::fs::symlink_metadata(volume.path().join(name)).is_ok(),
            "{name} must be left untouched on disk"
        );
    }
}

#[tokio::test]
async fn startup_reconciliation_is_idempotent_across_reruns() {
    let db = Database::open_in_memory().unwrap();
    let volume = fixture_volume();
    bind_lease(&db, LIVE_RUN, "pod-live-uid").await;

    let first = reconcile_run_dirs_at_startup(
        &db,
        volume.path(),
        VOLUME,
        &FilesystemRunDirInventory as &dyn RunDirInventorySource,
    )
    .await;
    // Growing the directory between runs must not rewrite the committed row.
    std::fs::write(
        volume.path().join(LIVE_RUN).join("more"),
        vec![3_u8; 65_536],
    )
    .unwrap();
    let second = reconcile_run_dirs_at_startup(
        &db,
        volume.path(),
        VOLUME,
        &FilesystemRunDirInventory as &dyn RunDirInventorySource,
    )
    .await;

    assert_eq!(first.resolved, second.resolved);
    assert_eq!(first.quarantined, second.quarantined);
    assert_eq!(second.upsert_errors, 0);
    let rows = RunDirRepository::new(db.clone())
        .list_by_volume(VOLUME)
        .await
        .unwrap();
    assert_eq!(rows.len(), 4, "a rerun adds no duplicate rows");
    let live = rows
        .iter()
        .find(|row| row.key.pod_uid == "pod-live-uid")
        .unwrap();
    assert_eq!(
        live.measured_bytes as u64, first.resolved_bytes,
        "the committed row is preserved, not overwritten"
    );
}

#[tokio::test]
async fn a_terminal_lease_reconciles_as_reclaimable_never_deleted() {
    let db = Database::open_in_memory().unwrap();
    let volume = fixture_volume();
    let key = bind_lease(&db, LIVE_RUN, "pod-terminal-uid").await;
    let repo = BuildLeaseRepository::new(db.clone());
    let row = repo.get(&key).await.unwrap().unwrap();
    repo.release(&key, row.fencing_token.unwrap(), None)
        .await
        .unwrap();

    reconcile_run_dirs_at_startup(
        &db,
        volume.path(),
        VOLUME,
        &FilesystemRunDirInventory as &dyn RunDirInventorySource,
    )
    .await;

    let reclaimable = RunDirRepository::new(db.clone())
        .get(&djinn_db::RunDirKey {
            volume_id: VOLUME.into(),
            pod_uid: "pod-terminal-uid".into(),
        })
        .await
        .unwrap()
        .expect("terminal pod proof reconciles");
    assert_eq!(reclaimable.state, RunDirState::Reclaimable);
    assert!(
        volume.path().join(LIVE_RUN).exists(),
        "reclaimable is a ledger state, not a deletion"
    );
}

#[tokio::test]
async fn an_unreadable_volume_leaves_the_ledger_untouched() {
    let db = Database::open_in_memory().unwrap();
    let root = std::path::Path::new("/nonexistent/djinn/run-dirs");
    let report = reconcile_run_dirs_at_startup(
        &db,
        root,
        VOLUME,
        &FilesystemRunDirInventory as &dyn RunDirInventorySource,
    )
    .await;
    assert!(report.inventory_failed);
    assert_eq!(report.scanned, 0);
    assert!(
        RunDirRepository::new(db)
            .list_by_volume(VOLUME)
            .await
            .unwrap()
            .is_empty()
    );
}

// ── Capacity sampling ───────────────────────────────────────────────────────

fn source_with(
    db: &Database,
    capacity: Result<CapacitySnapshot, String>,
    offset: Duration,
) -> CoordinatorDiskCapacitySource {
    CoordinatorDiskCapacitySource::new(
        VOLUME.to_owned(),
        std::path::PathBuf::from("/cache/cargo-target-runs"),
        config(),
        Arc::new(FixedCapacity(capacity)),
        Arc::new(RunDirRepository::new(db.clone())),
        Arc::new(OffsetClock {
            base: Instant::now(),
            offset,
        }),
        8 * GIB,
    )
}

fn snapshot(available: u64) -> CapacitySnapshot {
    CapacitySnapshot {
        total_bytes: 500 * GIB,
        available_bytes: available,
    }
}

#[tokio::test]
async fn live_samples_classify_healthy_warning_and_critical() {
    let db = Database::open_in_memory().unwrap();
    for (available, expected) in [
        (400 * GIB, DiskCapacityState::Healthy),
        (30 * GIB, DiskCapacityState::Warning),
        (5 * GIB, DiskCapacityState::Critical),
    ] {
        let sample = source_with(&db, Ok(snapshot(available)), Duration::ZERO).sample();
        assert_eq!(sample.state, expected, "available={available}");
        assert_eq!(sample.age, Duration::ZERO, "a live probe is fresh");
    }
}

#[tokio::test]
async fn a_failed_probe_with_no_history_is_unknown_and_would_defer() {
    let db = Database::open_in_memory().unwrap();
    let source = source_with(&db, Err("statvfs failed".into()), Duration::ZERO);
    let sample = source.sample();
    assert_eq!(sample.state, DiskCapacityState::Unknown);

    let observation = source
        .observe(&worker_request("unknown-sample"))
        .await
        .expect("a build workload always yields an observation");
    assert_eq!(
        observation.would_defer,
        Some(crate::disk_admission::DiskQueueReason::DiskCapacityUnknown)
    );
}

#[tokio::test]
async fn a_stale_fallback_sample_ages_past_the_freshness_bound() {
    let db = Database::open_in_memory().unwrap();
    // Alternate a good probe then a failing one: the source keeps the good
    // snapshot and reports its true age.
    struct FlakyCapacity {
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FilesystemCapacity for FlakyCapacity {
        fn capacity(&self, _path: &Path) -> Result<CapacitySnapshot, String> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(CapacitySnapshot {
                    total_bytes: 500 * GIB,
                    available_bytes: 400 * GIB,
                })
            } else {
                Err("statvfs failed".into())
            }
        }
    }
    struct SteppingClock {
        base: Instant,
        step: std::sync::atomic::AtomicUsize,
    }
    impl ObserveClock for SteppingClock {
        fn now(&self) -> Instant {
            let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u64;
            self.base + Duration::from_secs(step * 200)
        }
    }

    let source = CoordinatorDiskCapacitySource::new(
        VOLUME.to_owned(),
        std::path::PathBuf::from("/cache/cargo-target-runs"),
        config(),
        Arc::new(FlakyCapacity {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }),
        Arc::new(RunDirRepository::new(db.clone())),
        Arc::new(SteppingClock {
            base: Instant::now(),
            step: std::sync::atomic::AtomicUsize::new(0),
        }),
        8 * GIB,
    );

    let fresh = source.sample();
    assert_eq!(fresh.state, DiskCapacityState::Healthy);
    assert_eq!(fresh.age, Duration::ZERO);

    let stale = source.sample();
    assert_eq!(
        stale.state,
        DiskCapacityState::Healthy,
        "the retained snapshot keeps its classification"
    );
    assert!(
        stale.age > config().max_sample_age,
        "the retained snapshot is aged past the freshness bound"
    );

    // A stale sample that would reserve new bytes is a typed unknown-capacity
    // defer, not an optimistic grant.
    let observation = source
        .observe(&worker_request("stale-sample"))
        .await
        .unwrap();
    assert_eq!(
        observation.would_defer,
        Some(crate::disk_admission::DiskQueueReason::DiskCapacityUnknown)
    );
}

#[tokio::test]
async fn critical_pressure_would_defer_with_the_disk_pressure_reason() {
    let db = Database::open_in_memory().unwrap();
    let source = source_with(&db, Ok(snapshot(5 * GIB)), Duration::ZERO);
    let observation = source.observe(&worker_request("critical")).await.unwrap();
    assert_eq!(
        observation.would_defer,
        Some(crate::disk_admission::DiskQueueReason::DiskPressure)
    );
    assert!(observation.projected_reservation_bytes > 0);
}

#[tokio::test]
async fn a_healthy_sample_within_budget_would_not_defer() {
    let db = Database::open_in_memory().unwrap();
    let source = source_with(&db, Ok(snapshot(400 * GIB)), Duration::ZERO);
    let observation = source.observe(&worker_request("healthy")).await.unwrap();
    assert_eq!(observation.would_defer, None);
}

#[tokio::test]
async fn light_and_non_build_workloads_never_consult_disk() {
    let db = Database::open_in_memory().unwrap();
    let source = source_with(&db, Ok(snapshot(5 * GIB)), Duration::ZERO);
    for role in [
        TaskRunRole::Planner,
        TaskRunRole::Reviewer,
        TaskRunRole::Lead,
        TaskRunRole::Advocate,
        TaskRunRole::Adversary,
        TaskRunRole::Judge,
    ] {
        assert!(is_light_role(role), "{role:?} must classify as light");
        let mut request = worker_request("light");
        request.kind = BuildWorkloadKind::TaskRun { role };
        assert!(
            source.observe(&request).await.is_none(),
            "{role:?} must never consult disk admission"
        );
    }
    let mut non_build = worker_request("non-build");
    non_build.kind = BuildWorkloadKind::NonBuild {
        audit_reason: LIGHT_ROLE_AUDIT_REASON,
    };
    assert!(source.observe(&non_build).await.is_none());
}

// ── Quota probe ─────────────────────────────────────────────────────────────

#[test]
fn an_available_quota_permits_a_future_enforce() {
    // The production probe reads /proc/mounts, which a test cannot make
    // advertise `prjquota`; drive the exact answer the parser would return.
    let mounts = "/dev/sdb1 /cache xfs rw,relatime,prjquota 0 0\n";
    let support = crate::disk_admission::parse_project_quota_support(
        mounts,
        Path::new("/cache/cargo-target-runs"),
    );
    assert_eq!(support, QuotaSupport::Available);
    let report = record_quota_support(Path::new("/cache/cargo-target-runs"), support);
    assert!(
        !report.enforce_prohibited,
        "a working project quota is the prerequisite enforce needs"
    );
}

#[test]
fn a_quota_answer_of_unavailable_prohibits_enforce() {
    let report = record_quota_support(
        Path::new("/cache/cargo-target-runs"),
        QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoQuotaMountOption,
        },
    );
    assert!(report.enforce_prohibited);
}

#[test]
fn an_unavailable_quota_probe_prohibits_enforce_without_failing_observe() {
    // A tmpfs/ext4 test volume carries no project-quota mount option, which is
    // exactly the production condition today.
    let volume = tempfile::tempdir().unwrap();
    let report = report_quota_probe(volume.path());
    match report.support {
        QuotaSupport::Available => assert!(!report.enforce_prohibited),
        QuotaSupport::Unavailable { .. } => assert!(
            report.enforce_prohibited,
            "an unavailable probe must prohibit a future enforce"
        ),
    }
}

// ── Full production composition ─────────────────────────────────────────────

fn seams_for(
    volume: &tempfile::TempDir,
    capacity: Result<CapacitySnapshot, String>,
) -> RunDirObserveSeams {
    RunDirObserveSeams {
        root: volume.path().to_path_buf(),
        volume_id: VOLUME.to_owned(),
        config: config(),
        inventory: Arc::new(FilesystemRunDirInventory),
        capacity: Arc::new(FixedCapacity(capacity)),
        clock: Arc::new(SystemObserveClock),
    }
}

#[tokio::test]
async fn arming_observation_records_pressure_without_changing_any_grant() {
    let db = Database::open_in_memory().unwrap();
    let volume = fixture_volume();
    bind_lease(&db, LIVE_RUN, "pod-live-uid").await;
    let controller = controller(&db);

    // Baseline: the same request is permitted with the disk dimension dark.
    let before = controller.admit(worker_request("grant-a")).await.unwrap();
    assert!(matches!(before, BuildAdmissionDecision::Permitted { .. }));
    assert_eq!(controller.disk_would_defer_observation_count().await, 0);

    let report =
        arm_disk_observation(&db, &controller, seams_for(&volume, Ok(snapshot(GIB)))).await;
    assert_eq!(report.reconcile.resolved, 1);
    assert!(
        report.projected_seed_bytes > 0,
        "history feeds the projection"
    );

    // Armed, under critical pressure: the grant is IDENTICAL and only the
    // observe counter advances.
    let after = controller.admit(worker_request("grant-b")).await.unwrap();
    assert!(
        matches!(after, BuildAdmissionDecision::Permitted { .. }),
        "disk observation must never change a grant outcome"
    );
    assert_eq!(
        controller.disk_would_defer_observation_count().await,
        1,
        "critical pressure is recorded as a would-defer"
    );

    // And no reservation, quota, temp dir, or deletion resulted.
    for row in RunDirRepository::new(db.clone())
        .list_by_volume(VOLUME)
        .await
        .unwrap()
    {
        assert_eq!(row.reserved_bytes, 0);
        assert!(row.quota_id.is_none());
        assert!(row.temp_path.is_none());
    }
    assert!(volume.path().join(LIVE_RUN).exists());
    assert!(volume.path().join(UNRESOLVED_RUN).exists());
    assert!(volume.path().join(MALFORMED_DIR).exists());
}

#[tokio::test]
async fn arming_observation_on_a_healthy_volume_records_nothing() {
    let db = Database::open_in_memory().unwrap();
    let volume = fixture_volume();
    let controller = controller(&db);
    arm_disk_observation(
        &db,
        &controller,
        seams_for(&volume, Ok(snapshot(400 * GIB))),
    )
    .await;
    let decision = controller.admit(worker_request("healthy")).await.unwrap();
    assert!(matches!(decision, BuildAdmissionDecision::Permitted { .. }));
    assert_eq!(controller.disk_would_defer_observation_count().await, 0);
}

#[tokio::test]
async fn arming_observation_survives_a_missing_volume() {
    let db = Database::open_in_memory().unwrap();
    let controller = controller(&db);
    let seams = RunDirObserveSeams {
        root: std::path::PathBuf::from("/nonexistent/djinn/run-dirs"),
        volume_id: VOLUME.to_owned(),
        config: config(),
        inventory: Arc::new(FilesystemRunDirInventory),
        capacity: Arc::new(FixedCapacity(Err("no such volume".into()))),
        clock: Arc::new(SystemObserveClock),
    };
    let report = arm_disk_observation(&db, &controller, seams).await;
    assert!(report.reconcile.inventory_failed);
    // Observe-mode startup is not a boot hazard: admission still works.
    let decision = controller.admit(worker_request("missing")).await.unwrap();
    assert!(matches!(decision, BuildAdmissionDecision::Permitted { .. }));
}

// ── Bounded telemetry ───────────────────────────────────────────────────────

#[test]
fn state_labels_are_the_closed_telemetry_family() {
    let labels: Vec<&'static str> = RunDirState::ALL
        .iter()
        .copied()
        .map(metric_state_label)
        .collect();
    assert_eq!(labels.len(), 8);
    for (state, label) in RunDirState::ALL.iter().zip(labels.iter()) {
        assert_eq!(*label, state.as_str(), "label must match the ledger state");
        assert!(
            !label.contains('-') && label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "bounded label `{label}` must be a plain snake_case constant"
        );
    }
    // No identifier ever becomes a label value.
    assert!(!labels.contains(&LIVE_RUN));
}

#[test]
fn quota_labels_are_bounded() {
    for label in [
        quota_metric_label(&QuotaSupport::Available),
        quota_metric_label(&QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoMatchingMount,
        }),
        quota_metric_label(&QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::NoQuotaMountOption,
        }),
        quota_metric_label(&QuotaSupport::Unavailable {
            reason: QuotaUnavailableReason::MountsUnreadable,
        }),
    ] {
        assert!(label.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }
    assert_eq!(
        QuotaUnavailableReason::NoQuotaMountOption.as_metric(),
        "probe_unavailable",
        "every unavailable reason collapses to one bounded metric label"
    );
}

#[tokio::test]
async fn telemetry_publish_reads_only_bounded_ledger_totals() {
    let db = Database::open_in_memory().unwrap();
    let volume = fixture_volume();
    bind_lease(&db, LIVE_RUN, "pod-live-uid").await;
    let report = reconcile_run_dirs_at_startup(
        &db,
        volume.path(),
        VOLUME,
        &FilesystemRunDirInventory as &dyn RunDirInventorySource,
    )
    .await;
    publish_run_dir_telemetry(&db, VOLUME, report.loose_file_bytes)
        .await
        .expect("bounded telemetry publishes from ledger totals");
}

#[test]
fn volume_id_defaults_to_the_single_configured_volume() {
    // The default must be stable: it is the bounded `volume` dimension value.
    assert_eq!(DEFAULT_VOLUME_ID, "cargo-target-runs");
}
