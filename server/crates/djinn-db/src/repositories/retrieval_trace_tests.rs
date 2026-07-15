//! Tests for the retrieval trace repository (split from `retrieval_trace.rs`
//! to keep the main module under the Server Size Guard byte limit).

use serde_json::json;

use crate::database::Database;
use crate::repositories::retrieval_trace::{
    CANDIDATE_OUTCOME_VALUES, CandidateOutcome, CreateRetrievalTraceParams,
    CreateRetrievalTraceWithSemanticsParams, DEFAULT_CANDIDATE_CAP, DEFAULT_RETRIEVAL_TRACE_LIMIT,
    DurationStageSummary, ENTRY_POINT_VALUES, MAX_RETRIEVAL_TRACE_OFFSET,
    RETRIEVAL_TRACE_OUTCOME_VALUES, RETRIEVAL_TRACE_SCHEMA_VERSION, RetrievalTraceEntryPoint,
    RetrievalTraceListFilter, RetrievalTraceOutcome, RetrievalTraceRepository, RetrievalTraceRow,
    SKIPPED_REASON_VALUES, SkippedReason, TraceCandidate, WORKLOAD_ENTRY_POINTS,
    classify_legacy_trace_outcome, validate_candidates,
};

#[cfg(test)]
#[path = "retrieval_trace_semantics_tests.rs"]
mod retrieval_trace_semantics_tests;

fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

/// Seed a project so FK constraints pass.
async fn seed_project(db: &Database, project_id: &str) {
    db.ensure_initialized().await.unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo)
         VALUES ($1, $2, 'test-owner', $2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(project_id)
    .bind(format!("proj-{project_id}"))
    .execute(db.pool())
    .await
    .unwrap();
}

fn injected_candidate(note_id: &str, rank: i32, confidence: f64) -> TraceCandidate {
    TraceCandidate {
        note_id: note_id.to_string(),
        permalink: None,
        title: None,
        outcome: CandidateOutcome::Injected,
        rank: Some(rank),
        confidence: Some(confidence),
        skipped_reason: None,
        source: Some("scope_overlap".to_string()),
        scope: None,
    }
}

fn skipped_candidate(
    note_id: &str,
    rank: i32,
    confidence: f64,
    reason: SkippedReason,
) -> TraceCandidate {
    TraceCandidate {
        note_id: note_id.to_string(),
        permalink: None,
        title: None,
        outcome: CandidateOutcome::Skipped,
        rank: Some(rank),
        confidence: Some(confidence),
        skipped_reason: Some(reason),
        source: Some("scope_overlap".to_string()),
        scope: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_creates_retrieval_traces_table() {
    let db = test_db();
    db.ensure_initialized().await.unwrap();

    let count: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM retrieval_traces")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0, "fresh table should be empty");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_and_get_by_id_round_trips_fields() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000001";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    let candidates = serde_json::to_value(vec![
        TraceCandidate {
            note_id: "note-a".to_string(),
            permalink: Some("notes/note-a".to_string()),
            title: Some("Injected Note A".to_string()),
            outcome: CandidateOutcome::Injected,
            rank: Some(1),
            confidence: Some(0.95),
            skipped_reason: None,
            source: Some("scope_overlap".to_string()),
            scope: Some(json!({"matched_scopes": ["backend"], "query_scope": "server"})),
        },
        TraceCandidate {
            note_id: "note-b".to_string(),
            permalink: Some("notes/note-b".to_string()),
            title: Some("Skipped Note B".to_string()),
            outcome: CandidateOutcome::Skipped,
            rank: Some(2),
            confidence: Some(0.30),
            skipped_reason: Some(SkippedReason::NotTopK),
            source: Some("scope_overlap".to_string()),
            scope: Some(json!({"matched_scopes": ["frontend"], "query_scope": "ui"})),
        },
    ])
    .unwrap();
    let durations = json!({"retrieval_ms": 12, "cap_ms": 3});

    let row = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: Some("sess-1"),
            task_run_id: Some("run-1"),
            task_id: Some("task-1"),
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: Some(&json!({"query": "test query"})),
            candidates: &candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &durations,
            estimated_injected_tokens: 512,
        })
        .await
        .unwrap();

    assert_eq!(row.schema_version, RETRIEVAL_TRACE_SCHEMA_VERSION);
    assert_eq!(row.rollout_label, "enabled");
    assert_eq!(row.outcome, RetrievalTraceOutcome::Injected);
    assert_eq!(row.project_id, project_id);
    assert_eq!(row.session_id.as_deref(), Some("sess-1"));
    assert_eq!(row.task_run_id.as_deref(), Some("run-1"));
    assert_eq!(row.task_id.as_deref(), Some("task-1"));
    assert_eq!(row.entry_point, "dispatch");
    assert_eq!(
        row.entry_point_enum(),
        Some(RetrievalTraceEntryPoint::Dispatch)
    );
    assert_eq!(row.candidate_cap, 50);
    assert!(!row.candidate_cap_exceeded);
    assert!(row.sampling_metadata.is_none());
    assert_eq!(row.estimated_injected_tokens, 512);
    assert!(row.trigger.is_some());

    // get_by_id returns the same row.
    let fetched = repo
        .get_by_id(&row.id)
        .await
        .unwrap()
        .expect("row must exist");
    assert_eq!(fetched.id, row.id);
    assert_eq!(fetched.entry_point, "dispatch");
    assert_eq!(fetched.estimated_injected_tokens, 512);

    let typed = fetched.candidates_typed();
    assert_eq!(typed.len(), 2);

    let injected = &typed[0];
    assert_eq!(injected.note_id, "note-a");
    assert_eq!(injected.permalink.as_deref(), Some("notes/note-a"));
    assert_eq!(injected.title.as_deref(), Some("Injected Note A"));
    assert_eq!(injected.outcome, CandidateOutcome::Injected);
    assert_eq!(injected.rank, Some(1));
    assert_eq!(injected.confidence, Some(0.95));
    assert_eq!(injected.skipped_reason, None);
    assert_eq!(injected.source.as_deref(), Some("scope_overlap"));
    assert_eq!(
        injected.scope.as_ref(),
        Some(&json!({"matched_scopes": ["backend"], "query_scope": "server"}))
    );

    let skipped = &typed[1];
    assert_eq!(skipped.note_id, "note-b");
    assert_eq!(skipped.permalink.as_deref(), Some("notes/note-b"));
    assert_eq!(skipped.title.as_deref(), Some("Skipped Note B"));
    assert_eq!(skipped.outcome, CandidateOutcome::Skipped);
    assert_eq!(skipped.rank, Some(2));
    assert_eq!(skipped.confidence, Some(0.30));
    assert_eq!(skipped.skipped_reason, Some(SkippedReason::NotTopK));
    assert_eq!(skipped.source.as_deref(), Some("scope_overlap"));
    assert_eq!(
        skipped.scope.as_ref(),
        Some(&json!({"matched_scopes": ["frontend"], "query_scope": "ui"}))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_with_capped_candidates_and_cap_exceeded() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000002";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    // Simulate 60 candidates capped to 50.
    let candidates: Vec<TraceCandidate> = (0..50)
        .map(|i| injected_candidate(&format!("note-{i}"), i + 1, 0.5))
        .collect();
    let candidates_json = serde_json::to_value(&candidates).unwrap();

    let row = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
            trigger: None,
            candidates: &candidates_json,
            candidate_cap: 50,
            candidate_cap_exceeded: true,
            sampling_metadata: Some(&json!({"sample_rate": 1.0})),
            durations_ms: &json!({}),
            estimated_injected_tokens: 2000,
        })
        .await
        .unwrap();

    assert!(row.candidate_cap_exceeded);
    assert_eq!(row.candidate_cap, 50);
    assert_eq!(row.candidates_typed().len(), 50);
    assert!(row.sampling_metadata.is_some());
    assert_eq!(
        row.entry_point_enum(),
        Some(RetrievalTraceEntryPoint::LoadKnowledgeContext)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_by_project_returns_recent_traces_desc() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000003";
    let other_project = "019f4900-0000-7000-8000-000000000099";
    seed_project(&db, project_id).await;
    seed_project(&db, other_project).await;
    let repo = RetrievalTraceRepository::new(db);

    let candidates = json!([]);

    let r1 = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: Some("sess-a"),
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

    let r2 = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: Some("sess-b"),
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

    // Insert into another project — should not appear.
    repo.insert(CreateRetrievalTraceParams {
        project_id: other_project,
        session_id: None,
        task_run_id: None,
        task_id: None,
        entry_point: RetrievalTraceEntryPoint::Dispatch,
        trigger: None,
        candidates: &candidates,
        candidate_cap: 50,
        candidate_cap_exceeded: false,
        sampling_metadata: None,
        durations_ms: &json!({}),
        estimated_injected_tokens: 0,
    })
    .await
    .unwrap();

    let all = repo
        .list_by_project(project_id, RetrievalTraceListFilter::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    // DESC ordering: r2 (inserted later) should come first.
    assert_eq!(all[0].id, r2.id);
    assert_eq!(all[1].id, r1.id);

    // Filter by session_id.
    let filtered = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                session_id: Some("sess-a"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, r1.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_by_project_filters_by_entry_point_and_task() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000004";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);
    let candidates = json!([]);

    repo.insert(CreateRetrievalTraceParams {
        project_id,
        session_id: None,
        task_run_id: None,
        task_id: Some("task-x"),
        entry_point: RetrievalTraceEntryPoint::Dispatch,
        trigger: None,
        candidates: &candidates,
        candidate_cap: 50,
        candidate_cap_exceeded: false,
        sampling_metadata: None,
        durations_ms: &json!({}),
        estimated_injected_tokens: 0,
    })
    .await
    .unwrap();

    repo.insert(CreateRetrievalTraceParams {
        project_id,
        session_id: None,
        task_run_id: None,
        task_id: Some("task-y"),
        entry_point: RetrievalTraceEntryPoint::JitPitfalls,
        trigger: None,
        candidates: &candidates,
        candidate_cap: 50,
        candidate_cap_exceeded: false,
        sampling_metadata: None,
        durations_ms: &json!({}),
        estimated_injected_tokens: 0,
    })
    .await
    .unwrap();

    // Filter by entry point.
    let by_ep = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                entry_point: Some(RetrievalTraceEntryPoint::JitPitfalls),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_ep.len(), 1);
    assert_eq!(by_ep[0].entry_point, "jit_pitfalls");

    // Filter by task_id.
    let by_task = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                task_id: Some("task-x"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_task.len(), 1);
    assert_eq!(by_task[0].task_id.as_deref(), Some("task-x"));

    // Filter by task_run_id.
    let by_run = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                task_run_id: Some("run-z"),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(by_run.is_empty(), "no rows match run-z");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_by_project_respects_limit() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000005";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);
    let candidates = json!([]);

    for _ in 0..5 {
        repo.insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &candidates,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();
    }

    let limited = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(limited.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_by_project_filters_by_outcome_and_skipped_reason() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000025";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    let injected_only = serde_json::to_value(vec![injected_candidate("n1", 1, 0.9)]).unwrap();
    let skipped_not_top_k = serde_json::to_value(vec![skipped_candidate(
        "n2",
        2,
        0.3,
        SkippedReason::NotTopK,
    )])
    .unwrap();
    let skipped_min_conf = serde_json::to_value(vec![skipped_candidate(
        "n3",
        3,
        0.2,
        SkippedReason::MinConfidence,
    )])
    .unwrap();
    let mixed = serde_json::to_value(vec![
        injected_candidate("n4", 1, 0.95),
        skipped_candidate("n5", 2, 0.1, SkippedReason::NotTopK),
    ])
    .unwrap();

    let trace_injected = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &injected_only,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

    let trace_not_top_k = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::JitPitfalls,
            trigger: None,
            candidates: &skipped_not_top_k,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

    let _trace_min_conf = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::LoadKnowledgeContext,
            trigger: None,
            candidates: &skipped_min_conf,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

    let trace_mixed = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &mixed,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();

    // Outcome = Injected should match traces with at least one injected candidate.
    let by_injected = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                outcome: Some(CandidateOutcome::Injected),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_injected.len(), 2);
    let injected_ids: std::collections::HashSet<String> =
        by_injected.iter().map(|r| r.id.clone()).collect();
    assert!(injected_ids.contains(&trace_injected.id));
    assert!(injected_ids.contains(&trace_mixed.id));

    // Skipped reason = NotTopK should match rows with at least one such skipped candidate.
    let by_not_top_k = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                skipped_reason: Some(SkippedReason::NotTopK),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_not_top_k.len(), 2);
    let not_top_k_ids: std::collections::HashSet<String> =
        by_not_top_k.iter().map(|r| r.id.clone()).collect();
    assert!(not_top_k_ids.contains(&trace_not_top_k.id));
    assert!(not_top_k_ids.contains(&trace_mixed.id));

    // Combining entry_point with skipped_reason.
    let combined = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                entry_point: Some(RetrievalTraceEntryPoint::Dispatch),
                skipped_reason: Some(SkippedReason::NotTopK),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(combined.len(), 1);
    assert_eq!(combined[0].id, trace_mixed.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_by_project_validates_offset_and_limit_bounds() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000029";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    let res = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                offset: Some(-1),
                ..Default::default()
            },
        )
        .await;
    assert!(res.is_err());
    let err_msg = format!("{}", res.unwrap_err());
    assert!(
        err_msg.contains("offset must be non-negative"),
        "expected non-negative offset error, got: {err_msg}"
    );

    let res = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                offset: Some(MAX_RETRIEVAL_TRACE_OFFSET + 1),
                ..Default::default()
            },
        )
        .await;
    assert!(res.is_err());
    let err_msg = format!("{}", res.unwrap_err());
    assert!(
        err_msg.contains("offset cannot exceed"),
        "expected bounded offset error, got: {err_msg}"
    );

    let res = repo
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                limit: Some(-1),
                ..Default::default()
            },
        )
        .await;
    assert!(res.is_err());
    let err_msg = format!("{}", res.unwrap_err());
    assert!(
        err_msg.contains("limit must be non-negative"),
        "expected non-negative limit error, got: {err_msg}"
    );
}

#[test]
fn default_retrieval_trace_limit_is_100() {
    assert_eq!(DEFAULT_RETRIEVAL_TRACE_LIMIT, 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_by_id_returns_none_for_missing() {
    let db = test_db();
    db.ensure_initialized().await.unwrap();
    let repo = RetrievalTraceRepository::new(db);
    let result = repo.get_by_id("nonexistent-id").await.unwrap();
    assert!(result.is_none());
}

#[test]
fn skipped_reason_vocabulary_is_exact() {
    // Ensure the vocabulary matches the proposal requirement.
    let mut actual: Vec<&str> = SkippedReason::ALL_VARIANTS
        .iter()
        .map(|r| r.as_str())
        .collect();
    actual.sort();
    let mut expected: Vec<&str> = SKIPPED_REASON_VALUES.to_vec();
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(SKIPPED_REASON_VALUES.len(), 6);
}

#[test]
fn entry_point_vocabulary_matches_migration_check() {
    let mut actual: Vec<&str> = RetrievalTraceEntryPoint::ALL_VARIANTS
        .iter()
        .map(|e| e.as_str())
        .collect();
    actual.sort();
    let mut expected: Vec<&str> = ENTRY_POINT_VALUES.to_vec();
    expected.sort();
    assert_eq!(actual, expected);
}

// ── Cap/sampling metadata round-trip tests (qmel) ─────────────────────────

/// Insert helper with explicit fields for cap/sampling tests.
async fn insert_trace(
    repo: &RetrievalTraceRepository,
    project_id: &str,
    candidates: &serde_json::Value,
    candidate_cap: i32,
    candidate_cap_exceeded: bool,
    sampling_metadata: Option<&serde_json::Value>,
) -> RetrievalTraceRow {
    repo.insert(CreateRetrievalTraceParams {
        project_id,
        session_id: None,
        task_run_id: None,
        task_id: None,
        entry_point: RetrievalTraceEntryPoint::Dispatch,
        trigger: None,
        candidates,
        candidate_cap,
        candidate_cap_exceeded,
        sampling_metadata,
        durations_ms: &json!({}),
        estimated_injected_tokens: 0,
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candidate_cap_and_exceeded_round_trip() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000010";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    // Explicit cap of 30, not exceeded.
    let row = insert_trace(&repo, project_id, &json!([]), 30, false, None).await;
    assert_eq!(row.candidate_cap, 30);
    assert!(!row.candidate_cap_exceeded);

    // Fetch back by id to confirm persistence.
    let fetched = repo
        .get_by_id(&row.id)
        .await
        .unwrap()
        .expect("row must exist");
    assert_eq!(fetched.candidate_cap, 30);
    assert!(!fetched.candidate_cap_exceeded);

    // Cap exceeded case.
    let row2 = insert_trace(
        &repo,
        project_id,
        &json!([injected_candidate("n1", 1, 0.9)]),
        DEFAULT_CANDIDATE_CAP,
        true,
        None,
    )
    .await;
    assert_eq!(row2.candidate_cap, DEFAULT_CANDIDATE_CAP);
    assert!(row2.candidate_cap_exceeded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sampling_metadata_round_trips_when_present_and_absent() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000011";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    // No sampling metadata → NULL in DB.
    let row_none = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;
    assert!(row_none.sampling_metadata.is_none());

    // With sampling metadata.
    let sampling = json!({
        "enabled": true,
        "sample_rate": 0.25,
        "method": "top_k_reservoir",
        "seed": 42
    });
    let row_some = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        Some(&sampling),
    )
    .await;
    assert!(row_some.sampling_metadata.is_some());
    // Round-trip the JSONB value exactly.
    assert_eq!(row_some.sampling_metadata.unwrap(), sampling);
}

// ── Retention pruning tests (qmel) ────────────────────────────────────────

/// Backdate a trace row's `created_at` to a fixed ISO-8601 timestamp so
/// pruning tests can control which rows are old vs. new.
async fn backdate_created_at(db: &Database, trace_id: &str, created_at: &str) {
    sqlx::query("UPDATE retrieval_traces SET created_at = $1 WHERE id = $2")
        .bind(created_at)
        .bind(trace_id)
        .execute(db.pool())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_older_than_deletes_old_rows_and_reports_count() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000012";
    let other_project = "019f4900-0000-7000-8000-000000000013";
    seed_project(&db, project_id).await;
    seed_project(&db, other_project).await;
    let repo = RetrievalTraceRepository::new(db.clone());

    // Two "old" rows in the target project.
    let old1 = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;
    let old2 = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;
    // One "new" row that should survive.
    let keep = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;
    // An old row in a *different* project — must NOT be pruned by this call.
    let other_old = insert_trace(
        &repo,
        other_project,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;

    // Backdate: old rows → 2026-01-01, keep row → 2026-12-01.
    backdate_created_at(&db, &old1.id, "2026-01-01T00:00:00.000Z").await;
    backdate_created_at(&db, &old2.id, "2026-06-01T00:00:00.000Z").await;
    backdate_created_at(&db, &keep.id, "2026-12-01T00:00:00.000Z").await;
    backdate_created_at(&db, &other_old.id, "2026-01-01T00:00:00.000Z").await;

    // Cutoff: prune everything strictly before 2026-07-01.
    let pruned = repo
        .prune_older_than(project_id, "2026-07-01T00:00:00.000Z")
        .await
        .unwrap();

    // old1 and old2 are before the cutoff → 2 pruned.
    assert_eq!(pruned, 2);

    // The "keep" row survives.
    let remaining = repo
        .list_by_project(project_id, RetrievalTraceListFilter::default())
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, keep.id);

    // The other-project old row is untouched.
    let other_remaining = repo
        .list_by_project(other_project, RetrievalTraceListFilter::default())
        .await
        .unwrap();
    assert_eq!(other_remaining.len(), 1);
    assert_eq!(other_remaining[0].id, other_old.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_older_than_deletes_nothing_when_all_newer() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000014";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());

    let r1 = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;
    let r2 = insert_trace(
        &repo,
        project_id,
        &json!([]),
        DEFAULT_CANDIDATE_CAP,
        false,
        None,
    )
    .await;
    backdate_created_at(&db, &r1.id, "2026-11-01T00:00:00.000Z").await;
    backdate_created_at(&db, &r2.id, "2026-12-01T00:00:00.000Z").await;

    let pruned = repo
        .prune_older_than(project_id, "2026-01-01T00:00:00.000Z")
        .await
        .unwrap();
    assert_eq!(pruned, 0);

    let remaining = repo
        .list_by_project(project_id, RetrievalTraceListFilter::default())
        .await
        .unwrap();
    assert_eq!(remaining.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prune_older_than_empty_project_prunes_zero() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000015";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    let pruned = repo
        .prune_older_than(project_id, "2026-07-01T00:00:00.000Z")
        .await
        .unwrap();
    assert_eq!(pruned, 0);
}

// ── Candidate validation invariants (qmel) ────────────────────────────────

#[test]
fn validate_candidates_accepts_injected_and_valid_skipped() {
    let candidates = vec![
        injected_candidate("n1", 1, 0.9),
        skipped_candidate("n2", 2, 0.3, SkippedReason::NotTopK),
        skipped_candidate("n3", 3, 0.2, SkippedReason::MinConfidence),
        skipped_candidate("n4", 4, 0.1, SkippedReason::BudgetPruned),
        skipped_candidate("n5", 5, 0.05, SkippedReason::SupersededPruned),
        skipped_candidate("n6", 6, 0.04, SkippedReason::Dedupe),
        skipped_candidate("n7", 7, 0.01, SkippedReason::SearchError),
    ];
    // All combinations are valid: injected has None, skipped have valid reasons.
    assert!(validate_candidates(&candidates).is_ok());
}

#[test]
fn validate_candidates_accepts_empty_set() {
    assert!(validate_candidates(&[]).is_ok());
}

#[test]
fn default_candidate_cap_is_documented_as_50() {
    // The proposal default is 50 unless benchmarks justify a lower value
    // (see design/5wdh-roadmap). This documents and locks the default.
    assert_eq!(DEFAULT_CANDIDATE_CAP, 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candidate_invariants_survive_round_trip() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000016";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    // Candidate set covering the full skipped_reason vocabulary + injected.
    let candidates = json!([
        injected_candidate("inj-1", 1, 0.95),
        skipped_candidate("skip-not-top-k", 2, 0.30, SkippedReason::NotTopK),
        skipped_candidate("skip-min-conf", 3, 0.20, SkippedReason::MinConfidence),
        skipped_candidate("skip-budget", 4, 0.15, SkippedReason::BudgetPruned),
        skipped_candidate("skip-superseded", 5, 0.10, SkippedReason::SupersededPruned),
        skipped_candidate("skip-dedupe", 6, 0.08, SkippedReason::Dedupe),
        skipped_candidate("skip-search-err", 7, 0.01, SkippedReason::SearchError),
    ]);

    let row = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: None,
            candidates: &candidates,
            candidate_cap: DEFAULT_CANDIDATE_CAP,
            candidate_cap_exceeded: false,
            sampling_metadata: Some(&json!({"sample_rate": 1.0})),
            durations_ms: &json!({}),
            estimated_injected_tokens: 128,
        })
        .await
        .unwrap();

    let typed = row.candidates_typed();
    assert_eq!(typed.len(), 7);

    // The first candidate is injected (skipped_reason == None).
    assert!(typed[0].skipped_reason.is_none());

    // The remaining six each carry a distinct, valid skipped_reason.
    let reasons: Vec<SkippedReason> = typed[1..]
        .iter()
        .map(|c| c.skipped_reason.unwrap())
        .collect();
    assert_eq!(
        reasons,
        vec![
            SkippedReason::NotTopK,
            SkippedReason::MinConfidence,
            SkippedReason::BudgetPruned,
            SkippedReason::SupersededPruned,
            SkippedReason::Dedupe,
            SkippedReason::SearchError,
        ]
    );

    // Every round-tripped candidate passes the invariant check.
    assert!(validate_candidates(&typed).is_ok());
}

// ── Outcome-based invariant tests (qmel) ────────────────────────────────

#[test]
fn validate_candidates_rejects_skipped_candidate_without_reason() {
    // A candidate marked as skipped but missing its skipped_reason.
    let candidate = TraceCandidate {
        note_id: "bad-1".to_string(),
        permalink: None,
        title: None,
        outcome: CandidateOutcome::Skipped,
        rank: Some(1),
        confidence: Some(0.5),
        skipped_reason: None, // malformed: skipped candidate must have a reason
        source: None,
        scope: None,
    };
    let result = validate_candidates(&[candidate]);
    assert!(
        result.is_err(),
        "skipped candidate without skipped_reason must be rejected"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("outcome 'skipped' but no skipped_reason"),
        "error should describe the invariant violation: {err_msg}"
    );
}

#[test]
fn validate_candidates_rejects_injected_candidate_with_reason() {
    // A candidate marked as injected but carrying a skipped_reason.
    let candidate = TraceCandidate {
        note_id: "bad-2".to_string(),
        permalink: None,
        title: None,
        outcome: CandidateOutcome::Injected,
        rank: Some(1),
        confidence: Some(0.5),
        skipped_reason: Some(SkippedReason::NotTopK), // malformed: injected must not have a reason
        source: None,
        scope: None,
    };
    let result = validate_candidates(&[candidate]);
    assert!(
        result.is_err(),
        "injected candidate with skipped_reason must be rejected"
    );
    let err_msg = format!("{}", result.unwrap_err());
    assert!(
        err_msg.contains("outcome 'injected' but also has skipped_reason"),
        "error should describe the invariant violation: {err_msg}"
    );
}

#[test]
fn candidate_outcome_vocabulary_matches_constants() {
    // Ensure the vocabulary matches the constants.
    assert_eq!(CANDIDATE_OUTCOME_VALUES, &["injected", "skipped"]);
    assert_eq!(CandidateOutcome::Injected.as_str(), "injected");
    assert_eq!(CandidateOutcome::Skipped.as_str(), "skipped");
}

#[test]
fn outcome_round_trips_through_serde_json() {
    let candidate = injected_candidate("note-x", 1, 0.9);
    let json = serde_json::to_string(&candidate).unwrap();
    let deserialized: TraceCandidate = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.outcome, CandidateOutcome::Injected);
    assert!(deserialized.skipped_reason.is_none());

    let candidate2 = skipped_candidate("note-y", 2, 0.3, SkippedReason::Dedupe);
    let json2 = serde_json::to_string(&candidate2).unwrap();
    let deserialized2: TraceCandidate = serde_json::from_str(&json2).unwrap();
    assert_eq!(deserialized2.outcome, CandidateOutcome::Skipped);
    assert_eq!(deserialized2.skipped_reason, Some(SkippedReason::Dedupe));
}

#[test]
fn outcome_defaults_to_skipped_when_absent_from_json() {
    // Simulate legacy JSONB data without an "outcome" field.
    let json_str = r#"{"note_id":"legacy","rank":1,"confidence":0.5,"skipped_reason":"not_top_k","source":null,"scope":null}"#;
    let candidate: TraceCandidate = serde_json::from_str(json_str).unwrap();
    assert_eq!(
        candidate.outcome,
        CandidateOutcome::Skipped,
        "absent outcome should default to Skipped"
    );
    assert_eq!(candidate.permalink, None);
    assert_eq!(candidate.title, None);
    assert_eq!(candidate.skipped_reason, Some(SkippedReason::NotTopK));
    // Should pass validation because skipped + reason is consistent.
    assert!(candidate.validate_invariants().is_ok());
}

// ── Health rollup tests (m4uk) ──────────────────────────────────────────────

use RetrievalTraceEntryPoint::*;

async fn insert_workload_trace(
    repo: &RetrievalTraceRepository,
    db: &Database,
    project_id: &str,
    entry_point: RetrievalTraceEntryPoint,
    candidates: &serde_json::Value,
    durations_ms: &serde_json::Value,
    created_at: &str,
) -> RetrievalTraceRow {
    let row = repo
        .insert(CreateRetrievalTraceParams {
            project_id,
            session_id: None,
            task_run_id: None,
            task_id: None,
            entry_point,
            trigger: None,
            candidates,
            candidate_cap: DEFAULT_CANDIDATE_CAP,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms,
            estimated_injected_tokens: 0,
        })
        .await
        .unwrap();
    backdate_created_at(db, &row.id, created_at).await;
    row
}

macro_rules! wl {
    ($repo:expr, $db:expr, $proj:expr, $ep:expr, $ts:expr) => {
        insert_workload_trace($repo, $db, $proj, $ep, &json!([]), &json!({}), $ts).await
    };
    ($repo:expr, $db:expr, $proj:expr, $ep:expr, $cand:expr, $ts:expr) => {
        insert_workload_trace($repo, $db, $proj, $ep, &$cand, &json!({}), $ts).await
    };
    ($repo:expr, $db:expr, $proj:expr, $ep:expr, $cand:expr, $dur:expr, $ts:expr) => {
        insert_workload_trace($repo, $db, $proj, $ep, &$cand, &$dur, $ts).await
    };
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_rollup_honors_exact_half_open_bounds() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000100";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());
    wl!(&repo, &db, project_id, Dispatch, "2026-07-01T00:00:00.000Z");
    wl!(&repo, &db, project_id, Dispatch, "2026-07-01T00:30:00.000Z");
    wl!(&repo, &db, project_id, Dispatch, "2026-07-01T01:00:00.000Z");

    let rollup = repo
        .health_rollup(
            project_id,
            "2026-07-01T00:00:00.000Z",
            "2026-07-01T01:00:00.000Z",
        )
        .await
        .unwrap();
    assert_eq!(
        rollup.combined.trace_count, 2,
        "until boundary is exclusive"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_rollup_counts_all_rows_regardless_of_list_cap() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000101";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());
    let total = DEFAULT_RETRIEVAL_TRACE_LIMIT + 10;

    for i in 0..total {
        wl!(
            &repo,
            &db,
            project_id,
            Dispatch,
            &format!("2026-07-01T{:02}:00:00.000Z", i / 10)
        );
    }

    let rollup = repo
        .health_rollup(
            project_id,
            "2026-07-01T00:00:00.000Z",
            "2026-07-01T11:00:00.000Z",
        )
        .await
        .unwrap();
    assert_eq!(
        rollup.combined.trace_count,
        i64::from(total),
        "rollup must count every row, not the list pagination limit"
    );
    assert!(
        rollup.combined.trace_count > i64::from(DEFAULT_RETRIEVAL_TRACE_LIMIT),
        "rollup should exceed the list cap to prove it is bypassed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_rollup_returns_empty_window() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000102";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db);

    let rollup = repo
        .health_rollup(
            project_id,
            "2026-07-01T00:00:00.000Z",
            "2026-07-01T01:00:00.000Z",
        )
        .await
        .unwrap();
    assert_eq!(rollup.combined.trace_count, 0);
    assert_eq!(rollup.combined.candidate_count, 0);
    assert_eq!(rollup.combined.injected_count, 0);
    assert_eq!(rollup.combined.skipped_count, 0);
    assert!(rollup.combined.duration_stage_summaries.is_empty());
    assert!(rollup.combined.estimated_injected_tokens_avg.is_none());
    assert!(rollup.per_entry_point.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_rollup_isolates_multiple_projects() {
    let db = test_db();
    let project_a = "019f4900-0000-7000-8000-000000000103";
    let project_b = "019f4900-0000-7000-8000-000000000104";
    seed_project(&db, project_a).await;
    seed_project(&db, project_b).await;
    let repo = RetrievalTraceRepository::new(db.clone());

    for _ in 0..3 {
        wl!(&repo, &db, project_a, Dispatch, "2026-07-01T00:00:00.000Z");
    }
    wl!(&repo, &db, project_b, Dispatch, "2026-07-01T00:00:00.000Z");

    let rollup = repo
        .health_rollup(
            project_a,
            "2026-07-01T00:00:00.000Z",
            "2026-07-01T01:00:00.000Z",
        )
        .await
        .unwrap();
    assert_eq!(rollup.combined.trace_count, 3);
    assert_eq!(rollup.per_entry_point.len(), 1);
    let ep_evidence = rollup
        .per_entry_point
        .get(&Dispatch)
        .expect("dispatch evidence present");
    assert_eq!(ep_evidence.trace_count, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_rollup_aggregates_mixed_outcomes_and_skip_reasons() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000105";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());

    let candidates = json!([
        injected_candidate("n1", 1, 0.95),
        skipped_candidate("n2", 2, 0.30, SkippedReason::NotTopK),
        skipped_candidate("n3", 3, 0.20, SkippedReason::MinConfidence),
    ]);

    let row = wl!(
        &repo,
        &db,
        project_id,
        Dispatch,
        candidates,
        "2026-07-01T00:00:00.000Z"
    );
    sqlx::query("UPDATE retrieval_traces SET estimated_injected_tokens = $1 WHERE id = $2")
        .bind(100)
        .bind(&row.id)
        .execute(db.pool())
        .await
        .unwrap();
    // A second trace with no candidates proves this is a trace-level count,
    // rather than a derivation from aggregate injected candidates.
    wl!(&repo, &db, project_id, Dispatch, "2026-07-01T00:30:00.000Z");

    let rollup = repo
        .health_rollup(
            project_id,
            "2026-07-01T00:00:00.000Z",
            "2026-07-01T01:00:00.000Z",
        )
        .await
        .unwrap();

    assert_eq!(rollup.combined.trace_count, 2);
    assert_eq!(rollup.combined.zero_result_trace_count, 1);
    assert_eq!(rollup.combined.candidate_count, 3);
    assert_eq!(rollup.combined.injected_count, 1);
    assert_eq!(rollup.combined.skipped_count, 2);
    assert_eq!(rollup.combined.skip_reason_counts.not_top_k, 1);
    assert_eq!(rollup.combined.skip_reason_counts.min_confidence, 1);
    assert_eq!(rollup.combined.skip_reason_counts.budget_pruned, 0);
    assert_eq!(rollup.combined.estimated_injected_tokens_sum, 100);
    assert_eq!(rollup.combined.estimated_injected_tokens_avg, Some(50.0));

    let score = &rollup.combined.candidate_score_summary;
    assert_eq!(score.count, 3);
    assert!((score.min.unwrap() - 0.20).abs() < 1e-9);
    assert!((score.max.unwrap() - 0.95).abs() < 1e-9);
    assert!(score.avg.is_some());

    let ep = rollup
        .per_entry_point
        .get(&Dispatch)
        .expect("dispatch evidence present");
    assert_eq!(ep.trace_count, 2);
    assert_eq!(ep.zero_result_trace_count, 1);
    assert_eq!(ep.candidate_count, 3);
    assert_eq!(ep.injected_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_rollup_reports_duration_stage_summaries_independently() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000106";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());
    let c = json!([injected_candidate("n1", 1, 0.9)]);

    wl!(
        &repo,
        &db,
        project_id,
        Dispatch,
        c,
        &json!({"lexical_ms": 10, "semantic_ms": 20}),
        "2026-07-01T00:00:00.000Z"
    );
    wl!(
        &repo,
        &db,
        project_id,
        JitPitfalls,
        c,
        &json!({"lexical_ms": 30}),
        "2026-07-01T00:00:00.000Z"
    );
    wl!(
        &repo,
        &db,
        project_id,
        LoadKnowledgeContext,
        c,
        &json!({}),
        "2026-07-01T00:00:00.000Z"
    );

    let rollup = repo
        .health_rollup(
            project_id,
            "2026-07-01T00:00:00.000Z",
            "2026-07-01T01:00:00.000Z",
        )
        .await
        .unwrap();

    let by_name: std::collections::HashMap<String, &DurationStageSummary> = rollup
        .combined
        .duration_stage_summaries
        .iter()
        .map(|s| (s.stage_name.clone(), s))
        .collect();
    assert_eq!(by_name.len(), 2);
    let lexical = by_name.get("lexical_ms").expect("lexical stage present");
    assert_eq!(lexical.count, 2);
    assert!((lexical.min.unwrap() - 10.0).abs() < 1e-9);
    assert!((lexical.max.unwrap() - 30.0).abs() < 1e-9);
    assert!((lexical.sum.unwrap() - 40.0).abs() < 1e-9);
    let semantic = by_name.get("semantic_ms").expect("semantic stage present");
    assert_eq!(semantic.count, 1);
    assert!((semantic.min.unwrap() - 20.0).abs() < 1e-9);

    let load_ctx = rollup
        .per_entry_point
        .get(&LoadKnowledgeContext)
        .expect("load_knowledge_context evidence present");
    assert!(load_ctx.duration_stage_summaries.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_rollup_excludes_non_workload_entry_points() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000107";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());

    wl!(&repo, &db, project_id, Dispatch, "2026-07-01T00:00:00.000Z");
    wl!(
        &repo,
        &db,
        project_id,
        MemoryRecallTrace,
        "2026-07-01T00:00:00.000Z"
    );

    let rollup = repo
        .health_rollup(
            project_id,
            "2026-07-01T00:00:00.000Z",
            "2026-07-01T01:00:00.000Z",
        )
        .await
        .unwrap();

    assert_eq!(rollup.combined.trace_count, 1);
    assert!(rollup.per_entry_point.contains_key(&Dispatch));
    assert!(!rollup.per_entry_point.contains_key(&MemoryRecallTrace));
    assert!(!WORKLOAD_ENTRY_POINTS.contains(&"memory_recall_trace"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_rollup_propagates_sql_errors() {
    let db = test_db();
    let project_id = "019f4900-0000-7000-8000-000000000108";
    seed_project(&db, project_id).await;
    let repo = RetrievalTraceRepository::new(db.clone());

    sqlx::query("DROP TABLE retrieval_traces")
        .execute(db.pool())
        .await
        .unwrap();
    let result = repo
        .health_rollup(
            project_id,
            "2026-07-01T00:00:00.000Z",
            "2026-07-01T01:00:00.000Z",
        )
        .await;
    assert!(
        result.is_err(),
        "health_rollup must return an error when the underlying SQL fails"
    );
}
