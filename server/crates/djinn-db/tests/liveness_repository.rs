//! Integration tests for [`LivenessRepository`] — round-trip persistence,
//! legacy-row readability, claim-extension recording, and idempotent outcome
//! recording for terminal task races.

use serde_json::json;
use sqlx::Row;

use djinn_db::database::Database;
use djinn_db::repositories::liveness::{
    ClaimExtensionRecord, LivenessEvidenceSnapshot, LivenessRepository,
};
use djinn_db::test_support::{
    UsageTestSessionSeed, UsageTestTaskSeed, seed_project, seed_session_row_with_id, seed_task_row,
};

/// Helper: seed a project + task + running session + task_run, returning
/// (db, project_id, task_id, session_id, task_run_id).
async fn seed_full_fixture(db: &Database) -> (String, String, String, String) {
    db.ensure_initialized().await.unwrap();

    let project_id = uuid::Uuid::now_v7().to_string();
    seed_project(db, &project_id, "liveness-test").await;

    let task_id = seed_task_row(
        db,
        UsageTestTaskSeed {
            project_id: &project_id,
            status: "in_progress",
            close_reason: None,
            total_reopen_count: 0,
        },
    )
    .await;

    let session_id = uuid::Uuid::now_v7().to_string();
    seed_session_row_with_id(
        db,
        &session_id,
        UsageTestSessionSeed {
            project_id: &project_id,
            model_id: "test-model",
            agent_type: "worker",
            started_at: "2025-06-01T00:00:00.000Z",
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: None,
            cost_basis: "unpriced",
            task_id: Some(&task_id),
        },
    )
    .await;

    // Mark the session as running (seed helper sets it to completed).
    sqlx::query("UPDATE sessions SET status = 'running' WHERE id = $1")
        .bind(&session_id)
        .execute(db.pool())
        .await
        .unwrap();

    // Create a task_run.
    let task_run_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO task_runs
            (id, project_id, task_id, trigger_type, status, workspace_path, mirror_ref)
         VALUES ($1, $2, $3, 'new_task', 'running', NULL, NULL)",
    )
    .bind(&task_run_id)
    .bind(&project_id)
    .bind(&task_id)
    .execute(db.pool())
    .await
    .unwrap();

    (project_id, task_id, session_id, task_run_id)
}

// ─── Test: round-trip evidence persistence ───────────────────────────────────

#[tokio::test]
async fn persist_evidence_round_trip() {
    let db = Database::open_in_memory().unwrap();
    let (_project_id, task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    let snapshot = LivenessEvidenceSnapshot {
        session_id: session_id.clone(),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        verdict: "dead".to_string(),
        outcome_kind: Some("dead_reclaimed".to_string()),
        outcome_reason: None,
        evidence: json!({
            "pod_phase": "absent",
            "activity": "idle",
            "exit_code": null,
        }),
    };

    let evidence_id = repo.persist_evidence(&snapshot).await.unwrap();
    assert!(!evidence_id.is_empty());

    // Verify the liveness_evidence row was inserted.
    let row = sqlx::query(
        "SELECT verdict, outcome_kind, outcome_reason, session_id, task_id, task_run_id
         FROM liveness_evidence WHERE id = $1",
    )
    .bind(&evidence_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("verdict"), "dead");
    assert_eq!(
        row.get::<Option<String>, _>("outcome_kind"),
        Some("dead_reclaimed".to_string())
    );
    assert_eq!(row.get::<Option<String>, _>("outcome_reason"), None);
    assert_eq!(row.get::<String, _>("session_id"), session_id);
    assert_eq!(
        row.get::<Option<String>, _>("task_id"),
        Some(task_id.clone())
    );
    assert_eq!(
        row.get::<Option<String>, _>("task_run_id"),
        Some(task_run_id.clone())
    );

    // Verify denormalized columns on sessions.
    let sess = sqlx::query(
        "SELECT liveness_verdict, liveness_outcome_kind, liveness_outcome_reason
         FROM sessions WHERE id = $1",
    )
    .bind(&session_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        sess.get::<Option<String>, _>("liveness_verdict"),
        Some("dead".to_string())
    );
    assert_eq!(
        sess.get::<Option<String>, _>("liveness_outcome_kind"),
        Some("dead_reclaimed".to_string())
    );

    // Verify denormalized columns on task_runs.
    let run = sqlx::query(
        "SELECT liveness_outcome_kind, liveness_outcome_reason
         FROM task_runs WHERE id = $1",
    )
    .bind(&task_run_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        run.get::<Option<String>, _>("liveness_outcome_kind"),
        Some("dead_reclaimed".to_string())
    );

    // Verify load_current_state returns the persisted data.
    let state = repo.load_current_state(&task_id).await.unwrap();
    assert_eq!(state.task_status.as_deref(), Some("in_progress"));
    assert!(!state.task_is_terminal);
    assert_eq!(state.session_liveness_verdict.as_deref(), Some("dead"));
    assert_eq!(
        state.session_liveness_outcome_kind.as_deref(),
        Some("dead_reclaimed")
    );
    assert_eq!(
        state.task_run_liveness_outcome_kind.as_deref(),
        Some("dead_reclaimed")
    );
    assert_eq!(
        state.active_session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        state.latest_task_run_id.as_deref(),
        Some(task_run_id.as_str())
    );
}

// ─── Test: legacy row readability (no liveness data) ─────────────────────────

#[tokio::test]
async fn load_current_state_legacy_rows_no_liveness_data() {
    let db = Database::open_in_memory().unwrap();
    let (_project_id, task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    // No evidence has been persisted — the session and task_run predate
    // the liveness schema. load_current_state should return None for all
    // liveness-specific fields.
    let state = repo.load_current_state(&task_id).await.unwrap();

    assert_eq!(state.task_status.as_deref(), Some("in_progress"));
    assert!(!state.task_is_terminal);
    assert_eq!(
        state.active_session_id.as_deref(),
        Some(session_id.as_str())
    );
    assert_eq!(
        state.latest_task_run_id.as_deref(),
        Some(task_run_id.as_str())
    );

    // Liveness fields are None (legacy rows).
    assert_eq!(state.session_liveness_verdict, None);
    assert_eq!(state.session_liveness_outcome_kind, None);
    assert_eq!(state.session_liveness_outcome_reason, None);
    assert_eq!(state.session_liveness_evidence, None);
    assert_eq!(state.task_run_liveness_outcome_kind, None);
    assert_eq!(state.task_run_liveness_outcome_reason, None);
    assert_eq!(state.task_run_liveness_evidence, None);
}

// ─── Test: load_current_state with nonexistent task ──────────────────────────

#[tokio::test]
async fn load_current_state_nonexistent_task() {
    let db = Database::open_in_memory().unwrap();
    let repo = LivenessRepository::new(db.clone());

    let state = repo
        .load_current_state("nonexistent-task-id")
        .await
        .unwrap();

    assert_eq!(state.task_status, None);
    assert!(!state.task_is_terminal);
    assert_eq!(state.active_session_id, None);
    assert_eq!(state.latest_task_run_id, None);
    assert_eq!(state.session_liveness_verdict, None);
    assert_eq!(state.task_run_liveness_outcome_kind, None);
}

// ─── Test: load_current_state for terminal task ──────────────────────────────

#[tokio::test]
async fn load_current_state_terminal_task() {
    let db = Database::open_in_memory().unwrap();
    let (_project_id, task_id, _session_id, _task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    // Close the task.
    sqlx::query("UPDATE tasks SET status = 'closed' WHERE id = $1")
        .bind(&task_id)
        .execute(db.pool())
        .await
        .unwrap();

    let state = repo.load_current_state(&task_id).await.unwrap();
    assert_eq!(state.task_status.as_deref(), Some("closed"));
    assert!(state.task_is_terminal);
}

// ─── Test: claim extension recording ────────────────────────────────────────

#[tokio::test]
async fn record_claim_extension_round_trip() {
    let db = Database::open_in_memory().unwrap();
    let (project_id, _task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    // First persist evidence to get a liveness_evidence_id.
    let snapshot = LivenessEvidenceSnapshot {
        session_id: session_id.clone(),
        task_id: None,
        task_run_id: Some(task_run_id.clone()),
        verdict: "slow".to_string(),
        outcome_kind: Some("slow_extended".to_string()),
        outcome_reason: None,
        evidence: json!({"activity": "idle", "pod_phase": "running"}),
    };
    let evidence_id = repo.persist_evidence(&snapshot).await.unwrap();

    // Record a granted claim extension.
    let ext = ClaimExtensionRecord {
        session_id: session_id.clone(),
        task_run_id: Some(task_run_id.clone()),
        project_id: project_id.clone(),
        liveness_evidence_id: Some(evidence_id.clone()),
        granted: true,
        extension_budget_before: 3,
        extension_budget_after: 2,
        metadata: json!({"reason": "slow_below_hard_cap"}),
    };
    let ext_id = repo.record_claim_extension(&ext).await.unwrap();
    assert!(!ext_id.is_empty());

    // Verify the claim_extensions row.
    let row = sqlx::query(
        "SELECT session_id, task_run_id, project_id, liveness_evidence_id,
                granted, extension_budget_before, extension_budget_after, metadata
         FROM claim_extensions WHERE id = $1",
    )
    .bind(&ext_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("session_id"), session_id);
    assert_eq!(
        row.get::<Option<String>, _>("task_run_id"),
        Some(task_run_id.clone())
    );
    assert_eq!(row.get::<String, _>("project_id"), project_id);
    assert_eq!(
        row.get::<Option<String>, _>("liveness_evidence_id"),
        Some(evidence_id)
    );
    assert!(row.get::<bool, _>("granted"));
    assert_eq!(row.get::<i32, _>("extension_budget_before"), 3);
    assert_eq!(row.get::<i32, _>("extension_budget_after"), 2);
}

// ─── Test: denied claim extension ───────────────────────────────────────────

#[tokio::test]
async fn record_claim_extension_denied() {
    let db = Database::open_in_memory().unwrap();
    let (project_id, _task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    let ext = ClaimExtensionRecord {
        session_id: session_id.clone(),
        task_run_id: Some(task_run_id.clone()),
        project_id: project_id.clone(),
        liveness_evidence_id: None,
        granted: false,
        extension_budget_before: 0,
        extension_budget_after: 0,
        metadata: json!({"reason": "budget_exhausted"}),
    };
    let ext_id = repo.record_claim_extension(&ext).await.unwrap();

    let row = sqlx::query("SELECT granted FROM claim_extensions WHERE id = $1")
        .bind(&ext_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!(!row.get::<bool, _>("granted"));
}

// ─── Test: idempotent evidence recording for terminal task race ──────────────

#[tokio::test]
async fn persist_evidence_terminal_task_noop_outcome() {
    let db = Database::open_in_memory().unwrap();
    let (_project_id, task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    // Close the task (terminal state).
    sqlx::query("UPDATE tasks SET status = 'closed' WHERE id = $1")
        .bind(&task_id)
        .execute(db.pool())
        .await
        .unwrap();

    // Persist a noop outcome for the terminal task.
    // Note: "none" is not a valid DB CHECK value for outcome_reason;
    // LivenessReason::None maps to NULL in the database.
    let snapshot = LivenessEvidenceSnapshot {
        session_id: session_id.clone(),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        verdict: "live".to_string(),
        outcome_kind: Some("kill_noop".to_string()),
        outcome_reason: None,
        evidence: json!({"terminal_noop": true}),
    };
    let evidence_id = repo.persist_evidence(&snapshot).await.unwrap();
    assert!(!evidence_id.is_empty());

    // Verify the outcome was recorded.
    let state = repo.load_current_state(&task_id).await.unwrap();
    assert!(state.task_is_terminal);
    assert_eq!(
        state.session_liveness_outcome_kind.as_deref(),
        Some("kill_noop")
    );
    // outcome_reason is NULL for kill_noop (LivenessReason::None).
    assert_eq!(state.session_liveness_outcome_reason, None);

    // Record a second noop — should succeed (idempotent from the
    // repository's perspective; the classifier may decide not to call
    // this again but the repo doesn't enforce that).
    let snapshot2 = LivenessEvidenceSnapshot {
        session_id: session_id.clone(),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        verdict: "live".to_string(),
        outcome_kind: Some("kill_noop".to_string()),
        outcome_reason: None,
        evidence: json!({"terminal_noop": true, "duplicate": true}),
    };
    let evidence_id2 = repo.persist_evidence(&snapshot2).await.unwrap();
    assert_ne!(evidence_id, evidence_id2);

    // Both evidence rows exist.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM liveness_evidence WHERE session_id = $1")
            .bind(&session_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(count, 2);
}

// ─── Test: multiple evidence snapshots update denormalized columns ───────────

#[tokio::test]
async fn multiple_evidence_snapshots_overwrite_denormalized() {
    let db = Database::open_in_memory().unwrap();
    let (_project_id, task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    // First snapshot: Slow verdict.
    let s1 = LivenessEvidenceSnapshot {
        session_id: session_id.clone(),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        verdict: "slow".to_string(),
        outcome_kind: None,
        outcome_reason: None,
        evidence: json!({"activity": "idle"}),
    };
    repo.persist_evidence(&s1).await.unwrap();

    let state = repo.load_current_state(&task_id).await.unwrap();
    assert_eq!(state.session_liveness_verdict.as_deref(), Some("slow"));

    // Second snapshot: Dead verdict (overwrites denormalized columns).
    let s2 = LivenessEvidenceSnapshot {
        session_id: session_id.clone(),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        verdict: "dead".to_string(),
        outcome_kind: Some("dead_reclaimed".to_string()),
        outcome_reason: None,
        evidence: json!({"pod_phase": "absent"}),
    };
    repo.persist_evidence(&s2).await.unwrap();

    let state = repo.load_current_state(&task_id).await.unwrap();
    assert_eq!(state.session_liveness_verdict.as_deref(), Some("dead"));
    assert_eq!(
        state.session_liveness_outcome_kind.as_deref(),
        Some("dead_reclaimed")
    );
    // Both evidence rows exist in the append-only table.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM liveness_evidence WHERE session_id = $1")
            .bind(&session_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(count, 2);
}

// ─── Test: evidence without task_run_id ──────────────────────────────────────

#[tokio::test]
async fn persist_evidence_without_task_run() {
    let db = Database::open_in_memory().unwrap();
    let (_project_id, task_id, session_id, _task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    let snapshot = LivenessEvidenceSnapshot {
        session_id: session_id.clone(),
        task_id: Some(task_id.clone()),
        task_run_id: None,
        verdict: "live".to_string(),
        outcome_kind: None,
        outcome_reason: None,
        evidence: json!({}),
    };
    let _evidence_id = repo.persist_evidence(&snapshot).await.unwrap();

    // Session denormalized columns should be updated.
    let state = repo.load_current_state(&task_id).await.unwrap();
    assert_eq!(state.session_liveness_verdict.as_deref(), Some("live"));

    // task_run columns should NOT be updated (task_run_id was None).
    assert_eq!(state.task_run_liveness_outcome_kind, None);
}

// ─── Test: load_current_state with no running sessions ───────────────────────

#[tokio::test]
async fn load_current_state_no_running_sessions() {
    let db = Database::open_in_memory().unwrap();
    let (_project_id, task_id, session_id, _task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    // Mark the session as completed.
    sqlx::query("UPDATE sessions SET status = 'completed' WHERE id = $1")
        .bind(&session_id)
        .execute(db.pool())
        .await
        .unwrap();

    let state = repo.load_current_state(&task_id).await.unwrap();
    // No running session → active_session_id is None.
    assert_eq!(state.active_session_id, None);
    // Liveness fields from session are all None.
    assert_eq!(state.session_liveness_verdict, None);
    assert_eq!(state.session_liveness_outcome_kind, None);
}
