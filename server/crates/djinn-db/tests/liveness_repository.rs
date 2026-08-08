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
        session_id: Some(session_id.clone()),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        trigger_identity: None,
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
        session_id: Some(session_id.clone()),
        task_id: None,
        task_run_id: Some(task_run_id.clone()),
        trigger_identity: None,
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
        session_id: Some(session_id.clone()),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        trigger_identity: None,
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
        session_id: Some(session_id.clone()),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        trigger_identity: None,
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
        session_id: Some(session_id.clone()),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        trigger_identity: None,
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
        session_id: Some(session_id.clone()),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        trigger_identity: None,
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
        session_id: Some(session_id.clone()),
        task_id: Some(task_id.clone()),
        task_run_id: None,
        trigger_identity: None,
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

#[tokio::test]
async fn persist_task_scoped_evidence_skips_ambiguous_denormalization() {
    let db = Database::open_in_memory().unwrap();
    let (_, task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());
    for (outcome_kind, executions) in [
        ("terminated", json!([])),
        (
            "desync_reconciled",
            json!([{"session_id":"first"}, {"session_id":"second"}]),
        ),
        ("genuinely_absent", json!([])),
        ("task_not_found", json!([])),
        ("teardown_failed", json!([])),
        ("settlement_failed", json!([])),
        ("reconciliation_incomplete", json!([])),
        ("audit_failed", json!([])),
    ] {
        repo.persist_evidence(&LivenessEvidenceSnapshot {
            session_id: None,
            task_id: Some(task_id.clone()),
            task_run_id: None,
            trigger_identity: None,
            verdict: "dead".to_string(),
            outcome_kind: Some(outcome_kind.to_string()),
            outcome_reason: None,
            evidence: json!({"executions": executions}),
        })
        .await
        .unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM liveness_evidence WHERE task_id = $1 AND session_id IS NULL AND task_run_id IS NULL")
        .bind(&task_id).fetch_one(db.pool()).await.unwrap();
    assert_eq!(count, 8);
    let persisted_outcomes: Vec<String> = sqlx::query_scalar(
        "SELECT outcome_kind FROM liveness_evidence
         WHERE task_id = $1 AND session_id IS NULL AND task_run_id IS NULL
         ORDER BY outcome_kind",
    )
    .bind(&task_id)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        persisted_outcomes,
        vec![
            "audit_failed",
            "desync_reconciled",
            "genuinely_absent",
            "reconciliation_incomplete",
            "settlement_failed",
            "task_not_found",
            "teardown_failed",
            "terminated",
        ]
    );
    assert_eq!(
        repo.get_session_liveness_fields(&session_id).await.unwrap(),
        (None, None)
    );
    let run_outcome: Option<String> =
        sqlx::query_scalar("SELECT liveness_outcome_kind FROM task_runs WHERE id = $1")
            .bind(&task_run_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(run_outcome, None);
}

#[tokio::test]
async fn persist_evidence_rejects_missing_owner_without_audit_row() {
    let db = Database::open_in_memory().unwrap();
    let repo = LivenessRepository::new(db.clone());
    assert!(
        repo.persist_evidence(&LivenessEvidenceSnapshot {
            session_id: None,
            task_id: None,
            task_run_id: None,
            trigger_identity: None,
            verdict: "live".to_string(),
            outcome_kind: Some("task_not_found".to_string()),
            outcome_reason: None,
            evidence: json!({}),
        })
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM liveness_evidence")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn persist_evidence_rejects_missing_session_foreign_key_without_audit_row() {
    let db = Database::open_in_memory().unwrap();
    let (_, task_id, _session_id, _task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    assert!(
        repo.persist_evidence(&LivenessEvidenceSnapshot {
            session_id: Some("missing-session".to_string()),
            task_id: Some(task_id.clone()),
            task_run_id: None,
            trigger_identity: None,
            verdict: "dead".to_string(),
            outcome_kind: Some("terminated".to_string()),
            outcome_reason: None,
            evidence: json!({}),
        })
        .await
        .is_err()
    );

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM liveness_evidence WHERE task_id = $1")
            .bind(&task_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn persist_evidence_rolls_back_insert_and_session_update_when_task_run_update_fails() {
    let db = Database::open_in_memory().unwrap();
    let (_, task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());

    sqlx::query(
        "CREATE FUNCTION liveness_task_run_update_failure_for_test() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'injected liveness task run update failure'; END;
         $$ LANGUAGE plpgsql",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER liveness_task_run_update_failure_for_test
         BEFORE UPDATE ON task_runs FOR EACH ROW
         EXECUTE FUNCTION liveness_task_run_update_failure_for_test()",
    )
    .execute(db.pool())
    .await
    .unwrap();

    assert!(
        repo.persist_evidence(&LivenessEvidenceSnapshot {
            session_id: Some(session_id.clone()),
            task_id: Some(task_id.clone()),
            task_run_id: Some(task_run_id.clone()),
            trigger_identity: None,
            verdict: "dead".to_string(),
            outcome_kind: Some("terminated".to_string()),
            outcome_reason: None,
            evidence: json!({"failure": "task_run_update"}),
        })
        .await
        .is_err()
    );

    let evidence_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM liveness_evidence WHERE task_id = $1")
            .bind(&task_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(evidence_count, 0);
    assert_eq!(
        repo.get_session_liveness_fields(&session_id).await.unwrap(),
        (None, None)
    );
    let task_run_outcome: Option<String> =
        sqlx::query_scalar("SELECT liveness_outcome_kind FROM task_runs WHERE id = $1")
            .bind(&task_run_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(task_run_outcome, None);
}

#[tokio::test]
async fn liveness_exit_projection_contract() {
    let db = Database::open_in_memory().unwrap();
    let (_, task_id, session_id, task_run_id) = seed_full_fixture(&db).await;
    let repo = LivenessRepository::new(db.clone());
    let trigger_identity = format!("session_exit:{session_id}");

    let first = LivenessEvidenceSnapshot {
        session_id: Some(session_id.clone()),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        trigger_identity: Some(trigger_identity.clone()),
        verdict: "protocol_violation".to_owned(),
        outcome_kind: Some("protocol_violation".to_owned()),
        outcome_reason: Some("clean_exit_nonterminal".to_owned()),
        evidence: json!({"sample": "first", "exit_code": 0}),
    };
    let replay = LivenessEvidenceSnapshot {
        session_id: Some(session_id.clone()),
        task_id: Some(task_id.clone()),
        task_run_id: Some(task_run_id.clone()),
        trigger_identity: Some(trigger_identity.clone()),
        verdict: "dead".to_owned(),
        outcome_kind: Some("dead_reclaimed".to_owned()),
        outcome_reason: None,
        evidence: json!({"sample": "losing replay", "exit_code": 1}),
    };

    // Establish the canonical exit observation, then race two independently
    // delivered replays against it. This keeps the expected winner taxonomy
    // deterministic while exercising the same conflict path used by
    // concurrent exit delivery.
    let first_id = repo.persist_evidence(&first).await.unwrap();
    let (replay_result, concurrent_replay_result) = tokio::join!(
        repo.persist_evidence(&replay),
        repo.persist_evidence(&replay),
    );
    assert_eq!(replay_result.unwrap(), first_id);
    assert_eq!(concurrent_replay_result.unwrap(), first_id);

    let evidence_row = sqlx::query(
        "SELECT session_id, task_id, task_run_id, verdict, outcome_kind, outcome_reason, evidence, created_at
         FROM liveness_evidence WHERE id = $1",
    )
    .bind(&first_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let original_session_id: String = evidence_row.get("session_id");
    let original_task_id: Option<String> = evidence_row.get("task_id");
    let original_task_run_id: Option<String> = evidence_row.get("task_run_id");
    let original_verdict: String = evidence_row.get("verdict");
    let original_kind: Option<String> = evidence_row.get("outcome_kind");
    let original_reason: Option<String> = evidence_row.get("outcome_reason");
    let original_evidence: serde_json::Value = evidence_row.get("evidence");
    let original_created_at: String = evidence_row.get("created_at");
    assert_eq!(original_verdict, "protocol_violation");
    assert_eq!(original_kind.as_deref(), Some("protocol_violation"));
    assert_eq!(original_reason.as_deref(), Some("clean_exit_nonterminal"));
    assert_eq!(original_evidence, first.evidence);

    sqlx::query("UPDATE tasks SET status = 'closed' WHERE id = $1")
        .bind(&task_id)
        .execute(db.pool())
        .await
        .unwrap();
    let replay_after_state_change = repo.persist_evidence(&replay).await.unwrap();
    assert_eq!(replay_after_state_change, first_id);

    let evidence_row = sqlx::query(
        "SELECT session_id, task_id, task_run_id, verdict, outcome_kind, outcome_reason, evidence, created_at
         FROM liveness_evidence WHERE id = $1",
    )
    .bind(&first_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        evidence_row.get::<String, _>("session_id"),
        original_session_id
    );
    assert_eq!(
        evidence_row.get::<Option<String>, _>("task_id"),
        original_task_id
    );
    assert_eq!(
        evidence_row.get::<Option<String>, _>("task_run_id"),
        original_task_run_id
    );
    assert_eq!(evidence_row.get::<String, _>("verdict"), original_verdict);
    assert_eq!(
        evidence_row.get::<Option<String>, _>("outcome_kind"),
        original_kind
    );
    assert_eq!(
        evidence_row.get::<Option<String>, _>("outcome_reason"),
        original_reason
    );
    assert_eq!(
        evidence_row.get::<serde_json::Value, _>("evidence"),
        original_evidence
    );
    assert_eq!(
        evidence_row.get::<String, _>("created_at"),
        original_created_at
    );
    let row_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM liveness_evidence WHERE trigger_identity = $1")
            .bind(&trigger_identity)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(row_count, 1);

    let session_projection: (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<serde_json::Value>,
    ) = sqlx::query_as(
        "SELECT liveness_verdict, liveness_outcome_kind, liveness_outcome_reason,
                liveness_evidence
         FROM sessions WHERE id = $1",
    )
    .bind(&session_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(
        session_projection.0.as_deref(),
        Some(original_verdict.as_str())
    );
    assert_eq!(session_projection.1, original_kind);
    assert_eq!(session_projection.2, original_reason);
    assert_eq!(session_projection.3, Some(original_evidence.clone()));
    let run_projection: (Option<String>, Option<String>, Option<serde_json::Value>) =
        sqlx::query_as(
            "SELECT liveness_outcome_kind, liveness_outcome_reason, liveness_evidence
             FROM task_runs WHERE id = $1",
        )
        .bind(&task_run_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(run_projection.0, original_kind);
    assert_eq!(run_projection.1, original_reason);
    assert_eq!(run_projection.2, Some(original_evidence));

    // No trigger identity retains the legacy append-only contract. Keep this
    // separate owner in this selector so exit-trigger idempotency does not
    // accidentally make older classification evidence unreadable.
    let (_, legacy_task_id, legacy_session_id, legacy_task_run_id) = seed_full_fixture(&db).await;
    let legacy_first = LivenessEvidenceSnapshot {
        session_id: Some(legacy_session_id.clone()),
        task_id: Some(legacy_task_id.clone()),
        task_run_id: Some(legacy_task_run_id.clone()),
        trigger_identity: None,
        verdict: "live".to_owned(),
        outcome_kind: Some("success".to_owned()),
        outcome_reason: None,
        evidence: json!({"sample": "legacy first"}),
    };
    let legacy_second = LivenessEvidenceSnapshot {
        session_id: Some(legacy_session_id.clone()),
        task_id: Some(legacy_task_id.clone()),
        task_run_id: Some(legacy_task_run_id.clone()),
        trigger_identity: None,
        verdict: "dead".to_owned(),
        outcome_kind: Some("dead_reclaimed".to_owned()),
        outcome_reason: Some("hard_runtime_exceeded".to_owned()),
        evidence: json!({"sample": "legacy second"}),
    };
    let legacy_first_id = repo.persist_evidence(&legacy_first).await.unwrap();
    let legacy_second_id = repo.persist_evidence(&legacy_second).await.unwrap();
    assert_ne!(legacy_first_id, legacy_second_id);

    let legacy_rows: Vec<(Option<String>, String, Option<String>, Option<String>)> =
        sqlx::query_as(
            "SELECT trigger_identity, verdict, outcome_kind, outcome_reason
             FROM liveness_evidence WHERE session_id = $1 ORDER BY id",
        )
        .bind(&legacy_session_id)
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(legacy_rows.len(), 2, "legacy evidence remains append-only");
    assert!(legacy_rows.iter().all(|row| row.0.is_none()));
    assert!(
        legacy_rows.iter().any(|row| {
            row.1 == "live" && row.2.as_deref() == Some("success") && row.3.is_none()
        })
    );
    assert!(legacy_rows.iter().any(|row| {
        row.1 == "dead"
            && row.2.as_deref() == Some("dead_reclaimed")
            && row.3.as_deref() == Some("hard_runtime_exceeded")
    }));

    let legacy_state = repo.load_current_state(&legacy_task_id).await.unwrap();
    assert_eq!(
        legacy_state.session_liveness_verdict.as_deref(),
        Some("dead")
    );
    assert_eq!(
        legacy_state.session_liveness_outcome_kind.as_deref(),
        Some("dead_reclaimed")
    );
    assert_eq!(
        legacy_state.session_liveness_outcome_reason.as_deref(),
        Some("hard_runtime_exceeded")
    );
    assert_eq!(
        legacy_state.task_run_liveness_outcome_kind.as_deref(),
        Some("dead_reclaimed")
    );
    assert_eq!(
        legacy_state.task_run_liveness_outcome_reason.as_deref(),
        Some("hard_runtime_exceeded")
    );
}
