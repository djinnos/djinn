//! Migration 163 execution-generation and task-scoped liveness evidence contract.

use djinn_db::Database;

async fn seed_task(db: &Database) -> String {
    let pool = db.pool();
    sqlx::query(
        "INSERT INTO users (id, github_id, github_login) \
         VALUES ('generation-user', 9000000163, 'generation-migration-user')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('generation-project', 'generation-project', 'djinnos', 'generation-contract')",
    )
    .execute(pool)
    .await
    .unwrap();
    let task_id = "generation-task".to_owned();
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs, created_by_user_id) \
         VALUES ($1, 'generation-project', 'gen163', 'title', 'description', 'design', \
                 '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, 'generation-user')",
    )
    .bind(&task_id)
    .execute(pool)
    .await
    .unwrap();
    task_id
}

#[tokio::test]
async fn migration_163_adds_generation_and_task_owned_reconciliation_evidence() {
    let db = Database::open_in_memory().expect("open migrated database");
    let pool = db.pool();
    let task_id = seed_task(&db).await;

    let generation: i64 =
        sqlx::query_scalar("SELECT execution_generation FROM tasks WHERE id = $1")
            .bind(&task_id)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(generation, 0);

    for (index, outcome) in [
        "success",
        "crash",
        "timeout",
        "dead_reclaimed",
        "protocol_violation",
        "kill_noop",
        "slow_extended",
        "terminated",
        "desync_reconciled",
        "genuinely_absent",
        "task_not_found",
        "teardown_failed",
        "settlement_failed",
        "reconciliation_incomplete",
        "audit_failed",
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query(
            "INSERT INTO liveness_evidence (id, session_id, task_id, verdict, outcome_kind) \
             VALUES ($1, NULL, $2, 'dead', $3)",
        )
        .bind(format!("generation-evidence-{index}"))
        .bind(&task_id)
        .bind(outcome)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("outcome {outcome} should be accepted: {error}"));
    }

    let no_owner = sqlx::query(
        "INSERT INTO liveness_evidence (id, session_id, task_id, verdict) \
         VALUES ('generation-no-owner', NULL, NULL, 'dead')",
    )
    .execute(pool)
    .await;
    assert!(
        no_owner.is_err(),
        "evidence must have a session or task owner"
    );

    let invalid_session = sqlx::query(
        "INSERT INTO liveness_evidence (id, session_id, task_id, verdict) \
         VALUES ('generation-invalid-session', 'missing-session', $1, 'dead')",
    )
    .bind(&task_id)
    .execute(pool)
    .await;
    assert!(
        invalid_session.is_err(),
        "non-null session IDs retain their FK"
    );
}
