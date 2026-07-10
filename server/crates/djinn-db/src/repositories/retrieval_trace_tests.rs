//! Tests for the retrieval trace repository (split from `retrieval_trace.rs`
//! to keep the main module under the Server Size Guard byte limit).

use serde_json::json;

use crate::database::Database;
use crate::repositories::retrieval_trace::{
    CANDIDATE_OUTCOME_VALUES, CandidateOutcome, CreateRetrievalTraceParams, DEFAULT_CANDIDATE_CAP,
    ENTRY_POINT_VALUES, RETRIEVAL_TRACE_SCHEMA_VERSION, RetrievalTraceEntryPoint,
    RetrievalTraceListFilter, RetrievalTraceRepository, RetrievalTraceRow, SKIPPED_REASON_VALUES,
    SkippedReason, TraceCandidate, validate_candidates,
};

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
