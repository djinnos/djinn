use super::{run_cheap_doctor_checks, run_cheap_doctor_checks_with_preserved_retrieval_keys};
use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorRegistry, DoctorResult, Finding, FindingSeverity,
    ResolverSnapshot,
};
use djinn_db::{Database, DoctorFindingRepository};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::broadcast;

fn fresh_db() -> Database {
    Database::open_in_memory().expect("in-memory db")
}

/// A two-resolver fixture with a malformed sibling seam. Phase zero seeds
/// active rows, phase one models a successful snapshot with one malformed
/// group plus healthy create/update/resolve work, and phase two is fully
/// healthy (no alarms).
struct MixedSnapshotRetrievalCheck {
    name: &'static str,
    phase: Arc<AtomicUsize>,
}

impl MixedSnapshotRetrievalCheck {
    fn new(name: &'static str, phase: Arc<AtomicUsize>) -> Self {
        Self { name, phase }
    }

    fn finding(&self, key: &str, generation: &str) -> Finding {
        let (_, project_and_entry_point) = key
            .split_once(':')
            .expect("fixture retrieval key has check, project, and entry point");
        let (project_id, entry_point) = project_and_entry_point
            .split_once(':')
            .expect("fixture retrieval key has check, project, and entry point");
        Finding::new(
            FindingSeverity::Error,
            self.name,
            ResolverSnapshot::new(
                "retrieval_alarm",
                json!({
                    "generation": generation,
                    "refresh_timestamp": format!("2026-01-01T01:00:0{}Z", generation),
                }),
                json!({"alarming": true}),
            ),
            format!("{generation} retrieval alarm"),
        )
        .with_entity_id("finding_key", key)
        .with_entity_id("project_id", project_id)
        .with_entity_id("entry_point", entry_point)
        .with_evidence(json!({
            "generation": generation,
            "refresh_timestamp": format!("2026-01-01T01:00:0{}Z", generation),
        }))
    }
}

impl DoctorCheck for MixedSnapshotRetrievalCheck {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "Injected mixed valid and malformed retrieval snapshot check"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let phase = self.phase.load(Ordering::SeqCst);
        let findings = match (self.name, phase) {
            ("memory.retrieval_zero_result", 0) => vec![
                self.finding("memory.retrieval_zero_result:malformed:dispatch", "0"),
                self.finding(
                    "memory.retrieval_zero_result:healthy-update:load_knowledge_context",
                    "0",
                ),
            ],
            ("memory.injection_starvation", 0) => vec![self.finding(
                "memory.injection_starvation:healthy-resolve:load_knowledge_context",
                "0",
            )],
            ("memory.retrieval_zero_result", 1) => vec![
                self.finding(
                    "memory.retrieval_zero_result:healthy-update:load_knowledge_context",
                    "1",
                ),
                self.finding("memory.retrieval_zero_result:healthy-create:dispatch", "1"),
            ],
            _ => Vec::new(),
        };
        Ok(findings)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_group_preservation_is_selective_and_recovers_on_healthy_refresh() {
    let _ = djinn_telemetry::init();
    let db = fresh_db();
    let (events_tx, _events_rx) = broadcast::channel(16);
    let phase = Arc::new(AtomicUsize::new(0));
    let registry = DoctorRegistry::new();
    djinn_core::doctor::register(
        &registry,
        MixedSnapshotRetrievalCheck::new("memory.retrieval_zero_result", Arc::clone(&phase)),
    );
    djinn_core::doctor::register(
        &registry,
        MixedSnapshotRetrievalCheck::new("memory.injection_starvation", Arc::clone(&phase)),
    );
    let repo = DoctorFindingRepository::new(db.clone());

    // Seed one alarm in the malformed identity plus healthy rows that the
    // mixed snapshot will update and resolve. The other malformed alarm key is
    // deliberately absent before reconciliation.
    run_cheap_doctor_checks(&registry, &db, &events_tx, Some("mixed-seed")).await;
    let seeded = repo
        .list_recent(Default::default())
        .await
        .expect("list seeded retrieval rows");
    let malformed_before = seeded
        .iter()
        .find(|row| {
            row.entity_ids["finding_key"] == "memory.retrieval_zero_result:malformed:dispatch"
        })
        .cloned()
        .expect("seed malformed zero-result row");
    let healthy_update_before = seeded
        .iter()
        .find(|row| {
            row.entity_ids["finding_key"]
                == "memory.retrieval_zero_result:healthy-update:load_knowledge_context"
        })
        .cloned()
        .expect("seed healthy update row");
    let healthy_resolve_before = seeded
        .iter()
        .find(|row| {
            row.entity_ids["finding_key"]
                == "memory.injection_starvation:healthy-resolve:load_knowledge_context"
        })
        .cloned()
        .expect("seed healthy resolve row");

    phase.store(1, Ordering::SeqCst);
    let malformed_keys = vec![
        "memory.injection_starvation:malformed:dispatch".to_owned(),
        "memory.retrieval_zero_result:malformed:dispatch".to_owned(),
    ];
    run_cheap_doctor_checks_with_preserved_retrieval_keys(
        &registry,
        &db,
        &events_tx,
        Some("mixed-snapshot"),
        &malformed_keys,
    )
    .await;

    // The malformed identity does not receive an upsert, resolution, or
    // synthesized counterpart: every persisted field stays byte-for-byte
    // equal while its absent starvation key remains absent.
    assert_eq!(
        repo.get(&malformed_before.id)
            .await
            .expect("reload malformed row"),
        Some(malformed_before.clone()),
        "malformed group's existing row must be preserved byte-for-byte",
    );
    let after_mixed = repo
        .list_recent(Default::default())
        .await
        .expect("list mixed retrieval rows");
    assert!(
        after_mixed.iter().all(|row| {
            row.entity_ids["finding_key"] != "memory.injection_starvation:malformed:dispatch"
        }),
        "preserving both malformed keys must not synthesize the absent alarm",
    );

    // Healthy siblings are unaffected by another group's malformed state: one
    // is updated, one resolves by healthy absence, and one is created.
    let healthy_updated = repo
        .get(&healthy_update_before.id)
        .await
        .expect("reload updated healthy row")
        .expect("healthy update row remains");
    assert_eq!(healthy_updated.status, "active");
    assert_eq!(healthy_updated.run_id.as_deref(), Some("mixed-snapshot"));
    assert_eq!(healthy_updated.evidence["generation"], "1");
    assert_ne!(healthy_updated, healthy_update_before);
    assert_eq!(
        repo.get(&healthy_resolve_before.id)
            .await
            .expect("reload resolved healthy row")
            .expect("healthy resolve row remains")
            .status,
        "resolved"
    );
    let healthy_created = after_mixed
        .iter()
        .find(|row| {
            row.entity_ids["finding_key"] == "memory.retrieval_zero_result:healthy-create:dispatch"
        })
        .cloned()
        .expect("healthy sibling created");
    assert_eq!(healthy_created.status, "active");
    assert_eq!(healthy_created.run_id.as_deref(), Some("mixed-snapshot"));

    // Once every group is healthy, normal absence reconciliation resumes for
    // the formerly malformed identity as well as every healthy row.
    phase.store(2, Ordering::SeqCst);
    run_cheap_doctor_checks_with_preserved_retrieval_keys(
        &registry,
        &db,
        &events_tx,
        Some("fully-healthy"),
        &[],
    )
    .await;
    for row in [
        malformed_before,
        healthy_updated,
        healthy_resolve_before,
        healthy_created,
    ] {
        assert_eq!(
            repo.get(&row.id)
                .await
                .expect("reload fully healthy row")
                .expect("reconciled row remains")
                .status,
            "resolved",
            "fully healthy refresh must reconcile {}",
            row.entity_ids["finding_key"],
        );
    }
}
