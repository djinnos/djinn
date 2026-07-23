//! Seeded taxonomy-v1 coverage for the elected-leader retrieval seam.

use super::*;
use crate::doctor::{register_retrieval_health_checks, retrieval_health::RetrievalHealthSource};
use djinn_core::doctor::{DoctorRegistry, INJECTION_STARVATION_NAME, RETRIEVAL_ZERO_RESULT_NAME};
use djinn_core::events::EventBus;
use djinn_core::models::KnowledgeInjectionConfig;
use djinn_db::{
    Database, DoctorFindingRepository, ProjectRepository,
    repositories::retrieval_trace::{
        CreateRetrievalTraceParams, CreateRetrievalTraceTerminalParams, DEFAULT_CANDIDATE_CAP,
        KnowledgeTraceDispositionCounts, KnowledgeTraceTerminalState, RetrievalTraceEntryPoint,
        RetrievalTraceOutcome, RetrievalTraceRepository,
    },
};
use serde_json::json;
use std::sync::Arc;
use time::{OffsetDateTime, format_description::well_known::Iso8601};
use tokio::sync::broadcast;

fn eager_alarm_config() -> KnowledgeInjectionConfig {
    KnowledgeInjectionConfig {
        injection_starvation_threshold_percent: 1,
        injection_starvation_query_floor: 1,
        ..KnowledgeInjectionConfig::default()
    }
}

async fn terminal(
    repository: &RetrievalTraceRepository,
    project_id: &str,
    entry_point: RetrievalTraceEntryPoint,
    outcome: RetrievalTraceOutcome,
    candidate_count: i32,
    injected_count: i32,
) {
    let terminal_at = OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .expect("format terminal timestamp");
    let candidates = json!([]);
    let durations = json!({});
    repository
        .insert_terminal(CreateRetrievalTraceTerminalParams {
            trace: CreateRetrievalTraceParams {
                project_id,
                session_id: None,
                task_run_id: None,
                task_id: None,
                entry_point,
                trigger: None,
                candidates: &candidates,
                candidate_cap: DEFAULT_CANDIDATE_CAP,
                candidate_cap_exceeded: false,
                sampling_metadata: None,
                durations_ms: &durations,
                estimated_injected_tokens: injected_count,
            },
            rollout_label: "cohort:leader-retrieval-seed",
            outcome,
            terminal_state: KnowledgeTraceTerminalState::Success,
            terminal_at: &terminal_at,
            candidate_count: Some(candidate_count),
            injected_count: Some(injected_count),
            dispositions: Some(KnowledgeTraceDispositionCounts {
                confidence_filtered: 0,
                not_top_k: candidate_count - injected_count,
                oversized_skipped: 0,
                injected: injected_count,
                budget_pruned: 0,
            }),
        })
        .await
        .expect("persist taxonomy-v1 terminal");
}

async fn project_id(db: &Database, name: &str) -> String {
    ProjectRepository::new(db.clone(), EventBus::noop())
        .create(name, "retrieval-test", name)
        .await
        .expect("create retrieval test project")
        .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elected_leader_refreshes_seeded_taxonomy_and_persists_only_eligible_alarms() {
    let _ = djinn_telemetry::init();
    let db = Database::open_in_memory().expect("open in-memory database");
    let traces = RetrievalTraceRepository::new(db.clone());
    let zero_project = project_id(&db, "zero-project").await;
    let starvation_project = project_id(&db, "starvation-project").await;
    let counter_only_project = project_id(&db, "counter-only-project").await;

    // Distinct identities prove keyed persistence for each resolver. Dispatch
    // and jit_pitfalls have starved-looking counters but remain counters only.
    terminal(
        &traces,
        &zero_project,
        RetrievalTraceEntryPoint::Dispatch,
        RetrievalTraceOutcome::Empty,
        0,
        0,
    )
    .await;
    terminal(
        &traces,
        &starvation_project,
        RetrievalTraceEntryPoint::LoadKnowledgeContext,
        RetrievalTraceOutcome::Empty,
        1,
        0,
    )
    .await;
    for entry_point in [
        RetrievalTraceEntryPoint::Dispatch,
        RetrievalTraceEntryPoint::JitPitfalls,
    ] {
        terminal(
            &traces,
            &counter_only_project,
            entry_point,
            RetrievalTraceOutcome::Empty,
            1,
            0,
        )
        .await;
    }

    let source = Arc::new(RetrievalHealthSource::new(db.clone(), eager_alarm_config()));
    let registry = DoctorRegistry::new();
    assert!(
        register_retrieval_health_checks(&registry, Arc::clone(&source)).is_empty(),
        "a fresh process registers all source-backed checks before any refresh"
    );
    let registered: Vec<_> = registry
        .enumerate()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        registered,
        vec![
            INJECTION_STARVATION_NAME,
            "memory.retrieval_health_refresh",
            RETRIEVAL_ZERO_RESULT_NAME,
        ],
    );

    let (events_tx, _events_rx) = broadcast::channel(16);
    let selected = vec![RETRIEVAL_ZERO_RESULT_NAME.to_owned()];
    let manual_runs = run_manual_retrieval_refresh_and_checks(
        Some(&source),
        &registry,
        &db,
        &events_tx,
        Some("seeded-manual"),
        &selected,
    )
    .await
    .expect("run named manual retrieval check");
    assert_eq!(
        manual_runs
            .iter()
            .map(|run| run.check_name)
            .collect::<Vec<_>>(),
        vec![RETRIEVAL_ZERO_RESULT_NAME],
        "manual callers receive only the named check result"
    );

    // The manual call shares a source but cannot gate the subsequent elected
    // leader tick: the leader refreshes independently and runs every Cheap
    // retrieval check.
    let runs = run_elected_retrieval_refresh_and_cheap_checks(
        true,
        Some(&source),
        &registry,
        &db,
        &events_tx,
        Some("seeded-elected-leader"),
    )
    .await;
    assert!(
        source.has_attempted_refresh(),
        "the elected leader owns refresh"
    );
    assert_eq!(
        runs.iter()
            .map(|run| (run.check_name, run.findings.len()))
            .collect::<Vec<_>>(),
        vec![
            (INJECTION_STARVATION_NAME, 1),
            ("memory.retrieval_health_refresh", 0),
            (RETRIEVAL_ZERO_RESULT_NAME, 1),
        ],
    );

    let rows = DoctorFindingRepository::new(db)
        .list_recent(Default::default())
        .await
        .expect("load persisted findings");
    assert_eq!(rows.len(), 2, "only eligible retrieval alarms persist");
    let zero = rows
        .iter()
        .find(|row| row.check_name == RETRIEVAL_ZERO_RESULT_NAME)
        .expect("persisted zero-result alarm");
    assert_eq!(zero.entity_ids["project_id"], zero_project);
    assert_eq!(zero.entity_ids["entry_point"], "dispatch");
    assert_eq!(zero.evidence["query_counters"]["zero_candidate_queries"], 1);
    assert_eq!(zero.evidence["configured_query_floor"], 1);
    let starvation = rows
        .iter()
        .find(|row| row.check_name == INJECTION_STARVATION_NAME)
        .expect("persisted starvation alarm");
    assert_eq!(starvation.entity_ids["project_id"], starvation_project);
    assert_eq!(
        starvation.entity_ids["entry_point"],
        "load_knowledge_context"
    );
    assert_eq!(starvation.evidence["query_counters"]["starved_queries"], 1);
    assert_eq!(starvation.evidence["candidate_total"], 1);
    assert!(
        rows.iter()
            .all(|row| row.entity_ids["project_id"] != counter_only_project)
    );
}
