use super::*;
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

/// Deterministic source/repository seam for retrieval lifecycle activity.
/// Phase 0 alarms, phase 1 is a whole-source failure, and phase 2 is a
/// healthy non-alarming refresh.
struct LifecycleRetrievalCheck {
    name: &'static str,
    phase: Arc<AtomicUsize>,
    resolver_invocations: Arc<AtomicUsize>,
}

impl LifecycleRetrievalCheck {
    fn new(
        name: &'static str,
        phase: Arc<AtomicUsize>,
        resolver_invocations: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            name,
            phase,
            resolver_invocations,
        }
    }
}

impl DoctorCheck for LifecycleRetrievalCheck {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "Injected retrieval source lifecycle test check"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let phase = self.phase.load(Ordering::SeqCst);
        if self.name == "memory.retrieval_health_refresh" {
            return if phase == 1 {
                let evidence = json!({
                    "error_class": "retrieval_health_refresh_failed",
                    "attempted_at": "2026-01-01T01:02:00Z",
                    "last_success_at": "2026-01-01T01:00:00Z",
                    "last_success_age_seconds": 120,
                    "detail": "injected repository refresh failure",
                });
                Ok(vec![
                    Finding::new(
                        FindingSeverity::Error,
                        self.name,
                        ResolverSnapshot::new(
                            "retrieval_health_refresh",
                            evidence.clone(),
                            json!({"healthy": false}),
                        ),
                        "injected repository refresh failure",
                    )
                    .with_evidence(evidence),
                ])
            } else {
                Ok(Vec::new())
            };
        }

        // A whole-source failure skips both retrieval resolvers.
        if phase == 1 {
            return Ok(Vec::new());
        }
        self.resolver_invocations.fetch_add(1, Ordering::SeqCst);
        if phase == 2 {
            return Ok(Vec::new());
        }

        let (finding_key, entry_point) = if self.name == "memory.retrieval_zero_result" {
            ("project-a:dispatch", "dispatch")
        } else {
            ("project-a:load_knowledge_context", "load_knowledge_context")
        };
        Ok(vec![
            Finding::new(
                FindingSeverity::Error,
                self.name,
                ResolverSnapshot::new(
                    "retrieval_alarm",
                    json!({"refresh_timestamp": "2026-01-01T01:00:00Z"}),
                    json!({"alarming": true}),
                ),
                format!("{entry_point} retrieval alarm"),
            )
            .with_entity_id("finding_key", finding_key)
            .with_entity_id("project_id", "project-a")
            .with_entity_id("entry_point", entry_point)
            .with_evidence(json!({
                "refresh_timestamp": "2026-01-01T01:00:00Z",
                "entry_point": entry_point,
            })),
        ])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_refresh_failure_preserves_keyed_alarms_and_recovers() {
    let _ = djinn_telemetry::init();
    let db = fresh_db();
    let (events_tx, _events_rx) = broadcast::channel(16);
    let phase = Arc::new(AtomicUsize::new(0));
    let zero_resolver_invocations = Arc::new(AtomicUsize::new(0));
    let starvation_resolver_invocations = Arc::new(AtomicUsize::new(0));
    let registry = DoctorRegistry::new();
    djinn_core::doctor::register(
        &registry,
        LifecycleRetrievalCheck::new(
            "memory.retrieval_zero_result",
            Arc::clone(&phase),
            Arc::clone(&zero_resolver_invocations),
        ),
    );
    djinn_core::doctor::register(
        &registry,
        LifecycleRetrievalCheck::new(
            "memory.injection_starvation",
            Arc::clone(&phase),
            Arc::clone(&starvation_resolver_invocations),
        ),
    );
    djinn_core::doctor::register(
        &registry,
        LifecycleRetrievalCheck::new(
            "memory.retrieval_health_refresh",
            Arc::clone(&phase),
            Arc::new(AtomicUsize::new(0)),
        ),
    );

    // Initial findings create lifecycle activity; repeated findings update it.
    run_cheap_doctor_checks(&registry, &db, &events_tx, Some("healthy-alarming")).await;
    run_cheap_doctor_checks(&registry, &db, &events_tx, Some("healthy-update")).await;
    let repo = DoctorFindingRepository::new(db.clone());
    let task_repo =
        djinn_db::TaskRepository::new(db.clone(), crate::events::event_bus_for(&events_tx));
    let activity = task_repo
        .query_activity(djinn_db::ActivityQuery {
            event_type: Some(DOCTOR_FINDING_ACTIVITY.to_owned()),
            ..Default::default()
        })
        .await
        .expect("query retrieval activity");
    let lifecycle_payloads: Vec<serde_json::Value> = activity
        .iter()
        .filter_map(|entry| serde_json::from_str(&entry.payload).ok())
        .filter(|payload: &serde_json::Value| {
            payload["check"].as_str().is_some_and(is_retrieval_check)
        })
        .collect();
    assert_eq!(
        lifecycle_payloads
            .iter()
            .filter(|payload| payload["lifecycle"] == "created")
            .count(),
        2,
        "initial keyed retrieval alarms must emit create activity",
    );
    assert_eq!(
        lifecycle_payloads
            .iter()
            .filter(|payload| payload["lifecycle"] == "updated")
            .count(),
        2,
        "repeated keyed retrieval alarms must emit update activity",
    );
    let created = lifecycle_payloads
        .iter()
        .find(|payload| payload["lifecycle"] == "created")
        .expect("created retrieval activity");
    assert!(created["evidence"].is_object());
    assert!(created["resolver_snapshot"].is_object());
    let zero_before = repo
        .latest_for_check("memory.retrieval_zero_result")
        .await
        .expect("zero-result row")
        .expect("zero-result created");
    let starvation_before = repo
        .latest_for_check("memory.injection_starvation")
        .await
        .expect("starvation row")
        .expect("starvation created");
    assert_eq!(zero_resolver_invocations.load(Ordering::SeqCst), 2);
    assert_eq!(starvation_resolver_invocations.load(Ordering::SeqCst), 2);

    // Failure preserves existing alarms while the refresh error is created.
    phase.store(1, Ordering::SeqCst);
    run_cheap_doctor_checks_with_preserved_retrieval_keys_inner(
        &registry,
        &db,
        &events_tx,
        Some("refresh-failure"),
        &[],
        None,
        Some(RetrievalRefreshOutcome::Failed),
    )
    .await;
    assert_eq!(zero_resolver_invocations.load(Ordering::SeqCst), 2);
    assert_eq!(starvation_resolver_invocations.load(Ordering::SeqCst), 2);
    assert_eq!(
        repo.get(&zero_before.id).await.expect("reload zero"),
        Some(zero_before.clone())
    );
    assert_eq!(
        repo.get(&starvation_before.id)
            .await
            .expect("reload starvation"),
        Some(starvation_before.clone())
    );
    let refresh = repo
        .latest_for_check("memory.retrieval_health_refresh")
        .await
        .expect("refresh row")
        .expect("refresh error created");
    assert_eq!(refresh.severity, "error");

    // Healthy absence resolves both alarms and the refresh error, and activity
    // is driven by persisted lifecycle rows rather than current findings.
    phase.store(2, Ordering::SeqCst);
    run_cheap_doctor_checks_with_preserved_retrieval_keys_inner(
        &registry,
        &db,
        &events_tx,
        Some("healthy-recovery"),
        &[],
        None,
        Some(RetrievalRefreshOutcome::Healthy),
    )
    .await;
    assert_eq!(zero_resolver_invocations.load(Ordering::SeqCst), 3);
    assert_eq!(starvation_resolver_invocations.load(Ordering::SeqCst), 3);
    for finding in [&zero_before, &starvation_before, &refresh] {
        assert_eq!(
            repo.get(&finding.id)
                .await
                .expect("reloaded resolved finding")
                .expect("finding retained")
                .status,
            "resolved"
        );
    }
    let resolved = task_repo
        .query_activity(djinn_db::ActivityQuery {
            event_type: Some(DOCTOR_FINDING_ACTIVITY.to_owned()),
            ..Default::default()
        })
        .await
        .expect("query resolved retrieval activity");
    let resolved_payloads: Vec<serde_json::Value> = resolved
        .iter()
        .filter_map(|entry| serde_json::from_str(&entry.payload).ok())
        .filter(|payload: &serde_json::Value| payload["lifecycle"] == "resolved")
        .collect();
    assert_eq!(resolved_payloads.len(), 3);
    assert!(resolved_payloads.iter().all(|payload| {
        payload["evidence"].is_object() && payload["resolver_snapshot"].is_object()
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_manual_failure_preserves_alarms_and_reconciles_hidden_refresh_check() {
    let _ = djinn_telemetry::init();
    let db = fresh_db();
    let (events_tx, _events_rx) = broadcast::channel(16);
    let phase = Arc::new(AtomicUsize::new(0));
    let zero_invocations = Arc::new(AtomicUsize::new(0));
    let starvation_invocations = Arc::new(AtomicUsize::new(0));
    let registry = DoctorRegistry::new();
    for (name, invocations) in [
        (
            "memory.retrieval_zero_result",
            Arc::clone(&zero_invocations),
        ),
        (
            "memory.injection_starvation",
            Arc::clone(&starvation_invocations),
        ),
        (
            "memory.retrieval_health_refresh",
            Arc::new(AtomicUsize::new(0)),
        ),
    ] {
        djinn_core::doctor::register(
            &registry,
            LifecycleRetrievalCheck::new(name, Arc::clone(&phase), invocations),
        );
    }

    run_cheap_doctor_checks(&registry, &db, &events_tx, Some("initial-alarms")).await;
    let repo = DoctorFindingRepository::new(db.clone());
    let zero_before = repo
        .latest_for_check("memory.retrieval_zero_result")
        .await
        .expect("load zero alarm")
        .expect("zero alarm exists");
    let starvation_before = repo
        .latest_for_check("memory.injection_starvation")
        .await
        .expect("load starvation alarm")
        .expect("starvation alarm exists");
    let selected = vec!["memory.retrieval_zero_result".to_owned()];

    // The public result is selected-only, while the failed outcome makes the
    // unselected refresh check participate in reconciliation internally.
    phase.store(1, Ordering::SeqCst);
    let failed_runs = run_cheap_doctor_checks_with_preserved_retrieval_keys_inner(
        &registry,
        &db,
        &events_tx,
        Some("named-failure"),
        &[],
        Some(&selected),
        Some(RetrievalRefreshOutcome::Failed),
    )
    .await;
    assert_eq!(
        failed_runs
            .iter()
            .map(|run| run.check_name)
            .collect::<Vec<_>>(),
        vec!["memory.retrieval_zero_result"],
        "the internal refresh diagnostic must not leak into the named response",
    );
    assert_eq!(
        repo.get(&zero_before.id).await.expect("reload zero"),
        Some(zero_before.clone()),
        "a failed refresh must preserve the selected alarm byte-for-byte",
    );
    assert_eq!(
        repo.get(&starvation_before.id)
            .await
            .expect("reload starvation"),
        Some(starvation_before.clone()),
        "unselected retrieval alarms must remain preserved",
    );
    let refresh = repo
        .latest_for_check("memory.retrieval_health_refresh")
        .await
        .expect("load refresh error")
        .expect("refresh error created");
    assert_eq!(refresh.severity, "error");
    assert_eq!(zero_invocations.load(Ordering::SeqCst), 1);
    assert_eq!(starvation_invocations.load(Ordering::SeqCst), 1);

    // A later successful request owns and resolves the refresh diagnostic, but
    // continues to preserve the unselected starvation row.
    phase.store(2, Ordering::SeqCst);
    let healthy_runs = run_cheap_doctor_checks_with_preserved_retrieval_keys_inner(
        &registry,
        &db,
        &events_tx,
        Some("named-recovery"),
        &[],
        Some(&selected),
        Some(RetrievalRefreshOutcome::Healthy),
    )
    .await;
    assert_eq!(
        healthy_runs
            .iter()
            .map(|run| run.check_name)
            .collect::<Vec<_>>(),
        vec!["memory.retrieval_zero_result"],
    );
    assert_eq!(
        repo.get(&zero_before.id)
            .await
            .expect("reload resolved zero")
            .expect("zero row retained")
            .status,
        "resolved",
    );
    assert_eq!(
        repo.get(&refresh.id)
            .await
            .expect("reload resolved refresh")
            .expect("refresh row retained")
            .status,
        "resolved",
    );
    assert_eq!(
        repo.get(&starvation_before.id)
            .await
            .expect("reload preserved starvation"),
        Some(starvation_before),
    );
}
