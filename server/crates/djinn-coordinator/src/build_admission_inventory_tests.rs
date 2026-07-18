//! Deterministic inventory/adoption and conservative proof-rule tests.
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use djinn_db::{
    AdmissionDomain, AdmissionJournalKey, AdmissionJournalRepository, AdmissionState,
    AdmissionWorkloadKind, AdoptLiveAdmissionInput, CreateStartedInput, Database,
    ReserveAdmissionInput,
};
use djinn_k8s::{
    LABEL_ADMISSION_DOMAIN, LABEL_ADMISSION_GENERATION, LABEL_ADMISSION_WORK_ID, UidGetResult,
    WorkloadInventory, WorkloadObjectKind, WorkloadRecord,
};
use futures::FutureExt;
use tokio::sync::RwLock;

use crate::{
    build_admission::{BuildAdmissionController, BuildAdmissionMode, BuildAdmissionReadiness},
    build_admission_inventory::BuildAdmissionReconciler,
};

struct FakeInventory {
    records: RwLock<Result<Vec<WorkloadRecord>, String>>,
    gets: RwLock<HashMap<(String, String), UidGetResult>>,
    list_calls: std::sync::atomic::AtomicUsize,
}

impl FakeInventory {
    fn new(records: Vec<WorkloadRecord>) -> Self {
        Self {
            records: RwLock::new(Ok(records)),
            gets: RwLock::new(HashMap::new()),
            list_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    async fn replace(&self, records: Vec<WorkloadRecord>) {
        *self.records.write().await = Ok(records);
    }

    async fn get_returns(&self, name: &str, uid: &str, result: UidGetResult) {
        self.gets
            .write()
            .await
            .insert((name.into(), uid.into()), result);
    }
}

#[async_trait]
impl WorkloadInventory for FakeInventory {
    async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
        self.list_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.records.read().await.clone()
    }

    async fn get_uid(&self, _kind: WorkloadObjectKind, name: &str, uid: &str) -> UidGetResult {
        self.gets
            .read()
            .await
            .get(&(name.into(), uid.into()))
            .copied()
            .unwrap_or(UidGetResult::Uncertain)
    }
}

fn record(name: &str, uid: Option<&str>, labels: &[(&str, &str)]) -> WorkloadRecord {
    WorkloadRecord {
        kind: WorkloadObjectKind::Job,
        name: name.into(),
        uid: uid.map(str::to_owned),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
        terminal: false,
        images: vec!["djinn:test".into()],
        commands: vec![],
    }
}

fn labeled(name: &str, uid: &str, work_id: &str, generation: &str) -> WorkloadRecord {
    record(
        name,
        Some(uid),
        &[
            (LABEL_ADMISSION_DOMAIN, "task_observation"),
            (LABEL_ADMISSION_WORK_ID, work_id),
            (LABEL_ADMISSION_GENERATION, generation),
        ],
    )
}

fn controller(mode: BuildAdmissionMode, cap: i64) -> Arc<BuildAdmissionController> {
    Arc::new(BuildAdmissionController::new(
        Arc::new(AdmissionJournalRepository::new(
            Database::open_in_memory().unwrap(),
        )),
        mode,
        cap,
        "inventory-test",
    ))
}

async fn adopt_live(
    controller: &BuildAdmissionController,
    key: AdmissionJournalKey,
    name: &str,
    uid: &str,
) {
    controller
        .journal()
        .adopt_live(&AdoptLiveAdmissionInput {
            key,
            workload_kind: AdmissionWorkloadKind::Task,
            creator_server_epoch: "old".into(),
            object_name: name.into(),
            object_uid: uid.into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn mixed_current_and_unlabeled_legacy_inventory_adopts_stable_identities() {
    let current = labeled("current", "uid-current", "current-work", "3");
    let legacy_task = record(
        "djinn-taskrun-old",
        Some("uid-task"),
        &[("djinn.app/task-run-id", "session-task")],
    );
    let legacy_warm = record(
        "djinn-warm-old",
        Some("uid-warm"),
        &[("djinn.app/warm", "true")],
    );
    let inventory = Arc::new(FakeInventory::new(vec![current, legacy_task, legacy_warm]));
    let controller = controller(BuildAdmissionMode::Enforce, 4);
    let report = BuildAdmissionReconciler::new(controller.clone(), inventory)
        .reconcile()
        .await;

    assert!(report.blockers.is_empty());
    assert_eq!(report.adopted, 3);
    let rows = controller.journal().list_active_rows().await.unwrap();
    assert!(
        rows.iter()
            .any(|r| r.key.work_id == "current-work" && r.key.generation == 3)
    );
    assert!(
        rows.iter()
            .any(|r| r.key.work_id == "session-task" && r.key.generation == 0)
    );
    assert!(
        rows.iter()
            .any(|r| r.key.work_id == "legacy-warm:uid-warm" && r.key.generation == 0)
    );
}

#[tokio::test]
async fn duplicate_missing_and_unstable_identities_block_enforce_readiness() {
    let inventory = Arc::new(FakeInventory::new(vec![
        labeled("duplicate-a", "uid-a", "same", "0"),
        labeled("duplicate-b", "uid-b", "same", "0"),
        record("djinn-taskrun-unstable", None, &[]),
        record(
            "missing",
            Some("uid-missing"),
            &[(LABEL_ADMISSION_DOMAIN, "task_observation")],
        ),
    ]));
    let controller = controller(BuildAdmissionMode::Enforce, 8);
    let report = BuildAdmissionReconciler::new(controller.clone(), inventory)
        .reconcile()
        .await;

    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.contains("duplicate identity"))
    );
    assert!(report.blockers.iter().any(|b| b.contains("unstable UID")));
    assert!(
        report
            .blockers
            .iter()
            .any(|b| b.contains("missing identity"))
    );
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::InventoryPending
    );
}

#[tokio::test]
async fn cap_exceeding_seed_stays_closed_and_off_does_not_inventory() {
    let inventory = Arc::new(FakeInventory::new(vec![
        labeled("one", "uid-one", "one", "0"),
        labeled("two", "uid-two", "two", "0"),
    ]));
    let enforce = controller(BuildAdmissionMode::Enforce, 1);
    BuildAdmissionReconciler::new(enforce.clone(), inventory.clone())
        .reconcile()
        .await;
    assert_eq!(
        enforce.readiness(),
        BuildAdmissionReadiness::SeededOccupancyAboveCap
    );

    BuildAdmissionReconciler::new(controller(BuildAdmissionMode::Off, 1), inventory.clone())
        .reconcile()
        .await;
    assert_eq!(
        inventory
            .list_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn absence_retains_create_unknown_then_late_same_name_create_is_adopted() {
    let controller = controller(BuildAdmissionMode::Enforce, 3);
    let key = AdmissionJournalKey {
        domain: AdmissionDomain::TaskObservation,
        work_id: "late".into(),
        generation: 7,
    };
    controller
        .journal()
        .reserve(
            &ReserveAdmissionInput {
                key: key.clone(),
                workload_kind: AdmissionWorkloadKind::Task,
                creator_server_epoch: "old".into(),
                object_name: "late-job".into(),
            },
            3,
        )
        .await
        .unwrap();
    controller
        .journal()
        .mark_create_started(&CreateStartedInput {
            key: key.clone(),
            creator_server_epoch: "old".into(),
            object_name: "late-job".into(),
        })
        .await
        .unwrap();
    controller
        .journal()
        .recover_predecessor_epoch("old")
        .await
        .unwrap();
    let inventory = Arc::new(FakeInventory::new(vec![]));
    let reconciler = BuildAdmissionReconciler::new(controller.clone(), inventory.clone());
    reconciler.reconcile().await;
    assert_eq!(
        controller.journal().list_active_rows().await.unwrap()[0].state,
        AdmissionState::CreateUnknown
    );

    inventory
        .replace(vec![labeled("late-job", "late-uid", "late", "7")])
        .await;
    reconciler.reconcile().await;
    let rows = controller.journal().list_active_rows().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, AdmissionState::Live);
    assert_eq!(rows[0].object_uid.as_deref(), Some("late-uid"));
}

#[tokio::test]
async fn uid_and_generation_mismatch_or_uncertain_get_retain_live_occupancy() {
    let controller = controller(BuildAdmissionMode::Enforce, 4);
    let key = AdmissionJournalKey {
        domain: AdmissionDomain::TaskObservation,
        work_id: "proof".into(),
        generation: 2,
    };
    adopt_live(&controller, key, "proof-job", "expected-uid").await;
    let mut mismatch = labeled("proof-job", "other-uid", "proof", "3");
    mismatch.terminal = true;
    let inventory = Arc::new(FakeInventory::new(vec![mismatch]));
    inventory
        .get_returns("proof-job", "expected-uid", UidGetResult::Uncertain)
        .await;
    let reconciler = BuildAdmissionReconciler::new(controller.clone(), inventory.clone());
    assert_eq!(reconciler.reconcile().await.released, 0);
    // A terminal object with a different generation is not adopted and cannot
    // prove the observed Live row terminal; only the original row occupies.
    assert_eq!(
        controller.journal().list_active_rows().await.unwrap().len(),
        1
    );

    inventory.replace(vec![]).await;
    assert_eq!(reconciler.reconcile().await.released, 0);
    assert_eq!(
        controller.journal().list_active_rows().await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn authoritative_uid_not_found_releases_and_emits_wakeup() {
    let controller = controller(BuildAdmissionMode::Enforce, 4);
    let key = AdmissionJournalKey {
        domain: AdmissionDomain::TaskObservation,
        work_id: "gone".into(),
        generation: 1,
    };
    adopt_live(&controller, key, "gone-job", "gone-uid").await;
    let inventory = Arc::new(FakeInventory::new(vec![]));
    inventory
        .get_returns("gone-job", "gone-uid", UidGetResult::NotFound)
        .await;
    let notified = controller.release_notifier().notified();
    let report = BuildAdmissionReconciler::new(controller.clone(), inventory)
        .reconcile()
        .await;

    assert_eq!(report.released, 1);
    assert!(notified.now_or_never().is_some());
    assert!(
        controller
            .journal()
            .list_active_rows()
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn journal_snapshot_failure_degrades_and_keeps_enforce_closed() {
    djinn_telemetry::init().unwrap();
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let controller = Arc::new(BuildAdmissionController::new(
        Arc::new(AdmissionJournalRepository::new(db.clone())),
        BuildAdmissionMode::Enforce,
        1,
        "failed-reconcile",
    ));
    controller.mark_ready();
    db.pool().close().await;
    let report =
        BuildAdmissionReconciler::new(controller.clone(), Arc::new(FakeInventory::new(vec![])))
            .reconcile()
            .await;
    assert!(!report.blockers.is_empty());
    assert_eq!(
        controller.readiness(),
        BuildAdmissionReadiness::JournalUnhealthy
    );
    assert!(djinn_telemetry::render().unwrap().lines().any(|line| {
        line.starts_with("djinn_build_admission_journal_degraded")
            && line.contains("effective_mode=\"enforce\"")
            && line.ends_with(" 1")
    }));
}
