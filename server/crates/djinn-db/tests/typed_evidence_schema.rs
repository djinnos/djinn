//! Migration 176 typed tribunal evidence schema constraints.

use djinn_db::Database;

async fn seed(db: &Database) -> (String, String, String) {
    let project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, 'djinnos', $3)",
    )
    .bind(&project_id)
    .bind(format!("typed-evidence-{project_id}"))
    .bind(format!("typed-evidence-{project_id}"))
    .execute(db.pool())
    .await
    .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs) \
         VALUES ($1, $2, $3, 'typed evidence', '', '', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
    )
    .bind(&task_id)
    .bind(&project_id)
    .bind(task_id.replace('-', ""))
    .execute(db.pool())
    .await
    .unwrap();

    let proposal_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO proposals (id, short_id, title, body, body_format, acceptance_criteria, status, latest_revision_seq) \
         VALUES ($1, $2, 'typed evidence', '', 'markdown', '[]'::jsonb, 'draft', 1)",
    )
    .bind(&proposal_id)
    .bind(proposal_id.replace('-', ""))
    .execute(db.pool())
    .await
    .unwrap();
    (proposal_id, task_id, project_id)
}

async fn insert_finding(
    db: &Database,
    id: &str,
    proposal_id: &str,
    task_id: &str,
    lifecycle: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO typed_evidence_findings \
         (id, proposal_id, demand_hash, lifecycle, claim, demanded_revision_seq, created_by_task_id) \
         VALUES ($1, $2, $3, $4, '{}'::jsonb, 1, $5)",
    )
    .bind(id)
    .bind(proposal_id)
    .bind(format!("hash-{id}"))
    .bind(lifecycle)
    .bind(task_id)
    .execute(db.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn typed_evidence_schema_retains_legacy_columns_and_enforces_identities() {
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let (proposal_id, task_id, project_id) = seed(&db).await;

    let legacy_columns: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns WHERE table_name = 'proposals' \
         AND column_name IN ('linked_spike_task_id', 'needs_evidence_claim')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(legacy_columns, 2, "migration 82 compatibility columns remain");

    let finding_id = uuid::Uuid::now_v7().to_string();
    insert_finding(&db, &finding_id, &proposal_id, &task_id, "demanded")
        .await
        .unwrap();
    assert!(
        insert_finding(
            &db,
            &uuid::Uuid::now_v7().to_string(),
            &proposal_id,
            &task_id,
            "spike_active",
        )
        .await
        .is_err(),
        "a proposal admits only one unresolved finding"
    );

    sqlx::query(
        "INSERT INTO typed_evidence_attempts (id, finding_id, sequence, spike_task_id) \
         VALUES ($1, $2, 1, $3)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(&finding_id)
    .bind(&task_id)
    .execute(db.pool())
    .await
    .unwrap();

    let second_task = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs) \
         VALUES ($1, $2, $3, 'second', '', '', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
    )
    .bind(&second_task)
    .bind(&project_id)
    .bind(second_task.replace('-', ""))
    .execute(db.pool())
    .await
    .unwrap();
    assert!(
        sqlx::query(
            "INSERT INTO typed_evidence_attempts (id, finding_id, sequence, spike_task_id) \
             VALUES ($1, $2, 1, $3)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&finding_id)
        .bind(&second_task)
        .execute(db.pool())
        .await
        .is_err(),
        "attempt sequences are ordered and unique per finding"
    );
    assert!(
        sqlx::query(
            "INSERT INTO typed_evidence_attempts (id, finding_id, sequence, spike_task_id) \
             VALUES ($1, $2, 2, $3)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&finding_id)
        .bind(&task_id)
        .execute(db.pool())
        .await
        .is_err(),
        "a spike task can bind to only one attempt"
    );
}
