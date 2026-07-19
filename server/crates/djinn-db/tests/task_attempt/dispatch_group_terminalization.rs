use crate::*;

async fn insert_attempt(
    db: &Database,
    id: &str,
    task_id: &str,
    role: &str,
    dispatch_key: &str,
    sequence: i32,
    group_id: Option<&str>,
    outcome: &str,
) {
    sqlx::query("INSERT INTO task_attempts (id, task_id, role, attempt_seq, dispatch_key, outcome, dispatch_group_id) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(id).bind(task_id).bind(role).bind(sequence).bind(dispatch_key).bind(outcome).bind(group_id)
        .execute(db.pool()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminalize_dispatch_group_is_exact_forward_only_and_idempotent() {
    let db = test_db();
    let (_project_id, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());
    let group = uuid::Uuid::now_v7().to_string();
    let other_group = uuid::Uuid::now_v7().to_string();
    let ids: Vec<String> = (0..6).map(|_| new_attempt_id()).collect();
    insert_attempt(
        &db,
        &ids[0],
        &task_id,
        "coordinator",
        "same-key-a",
        1,
        Some(&group),
        "pending",
    )
    .await;
    insert_attempt(
        &db,
        &ids[1],
        &task_id,
        "supervisor",
        "same-key-b",
        2,
        Some(&group),
        "pending",
    )
    .await;
    insert_attempt(
        &db,
        &ids[2],
        &task_id,
        "worker",
        "same-key-a-other-group",
        3,
        Some(&other_group),
        "pending",
    )
    .await;
    insert_attempt(
        &db,
        &ids[3],
        &task_id,
        "supervisor",
        "same-key-b-legacy",
        4,
        None,
        "pending",
    )
    .await;
    insert_attempt(
        &db,
        &ids[4],
        &task_id,
        "worker",
        "submitted-in-group",
        5,
        Some(&group),
        "submitted",
    )
    .await;
    insert_attempt(
        &db,
        &ids[5],
        &task_id,
        "worker",
        "terminal-in-group",
        6,
        Some(&group),
        "completed",
    )
    .await;
    let result = repo
        .terminalize_dispatch_group(
            &group,
            TaskAttemptOutcome::SpawnFailed,
            DispatchGroupTerminalEvidence {
                summary: Some("dispatch setup failed"),
                summary_json: Some(r#"{"failure_class":"dispatch_failure_orphan"}"#),
            },
        )
        .await
        .unwrap();
    let mut expected = vec![ids[0].clone(), ids[1].clone()];
    expected.sort();
    assert_eq!(result.updated_attempt_ids, expected);
    for id in &ids[..2] {
        let row = repo.get(id).await.unwrap().unwrap();
        assert_eq!(row.outcome, "spawn_failed");
        assert_eq!(row.summary.as_deref(), Some("dispatch setup failed"));
        assert_eq!(
            row.summary_json.as_deref(),
            Some(r#"{"failure_class": "dispatch_failure_orphan"}"#)
        );
        assert!(row.terminal_at.is_some());
    }
    assert_eq!(repo.get(&ids[2]).await.unwrap().unwrap().outcome, "pending");
    assert_eq!(repo.get(&ids[3]).await.unwrap().unwrap().outcome, "pending");
    assert_eq!(
        repo.get(&ids[4]).await.unwrap().unwrap().outcome,
        "submitted"
    );
    assert_eq!(
        repo.get(&ids[5]).await.unwrap().unwrap().outcome,
        "completed"
    );
    let repeated = repo
        .terminalize_dispatch_group(
            &group,
            TaskAttemptOutcome::Cancelled,
            DispatchGroupTerminalEvidence {
                summary: Some("must not overwrite"),
                summary_json: Some(r#"{"must_not":"overwrite"}"#),
            },
        )
        .await
        .unwrap();
    assert!(repeated.updated_attempt_ids.is_empty());
    assert_eq!(
        repo.get(&ids[0]).await.unwrap().unwrap().outcome,
        "spawn_failed"
    );
}

/// AC4: A deterministic partial-failure regression proving the explicit
/// transaction rolls back ALL member updates when an in-transaction constraint
/// violation occurs. Uses a temporary CHECK constraint (a supported database
/// constraint mechanism, not external infrastructure) to force the UPDATE to
/// fail mid-transaction, then asserts every member retains its pre-call
/// outcome and evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminalize_dispatch_group_rolls_back_all_members_on_constraint_failure() {
    let db = test_db();
    let (_project_id, task_id) = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());
    let group = uuid::Uuid::now_v7().to_string();
    let ids: Vec<String> = (0..3).map(|_| new_attempt_id()).collect();

    // Three pending members in the exact group with differing roles and
    // dispatch keys, so no task/role/dispatch-key heuristic could correlate
    // them — only the exact group UUID does.
    insert_attempt(
        &db,
        &ids[0],
        &task_id,
        "coordinator",
        "rb-key-a",
        1,
        Some(&group),
        "pending",
    )
    .await;
    insert_attempt(
        &db,
        &ids[1],
        &task_id,
        "supervisor",
        "rb-key-b",
        2,
        Some(&group),
        "pending",
    )
    .await;
    insert_attempt(
        &db,
        &ids[2],
        &task_id,
        "worker",
        "rb-key-c",
        3,
        Some(&group),
        "pending",
    )
    .await;

    // Install a temporary CHECK constraint that the terminal outcome would
    // violate, deterministically forcing the in-transaction UPDATE to fail.
    sqlx::query(
        "ALTER TABLE task_attempts ADD CONSTRAINT test_rb_guard CHECK (outcome != 'spawn_failed')",
    )
    .execute(db.pool())
    .await
    .unwrap();

    // The terminalization must fail because the CHECK constraint rejects the
    // outcome the UPDATE attempts to write on every matched row.
    let result = repo
        .terminalize_dispatch_group(
            &group,
            TaskAttemptOutcome::SpawnFailed,
            DispatchGroupTerminalEvidence {
                summary: Some("dispatch setup failed"),
                summary_json: Some(r#"{"failure_class":"dispatch_failure_orphan"}"#),
            },
        )
        .await;
    assert!(
        result.is_err(),
        "terminalize_dispatch_group must return an error when a constraint is violated"
    );

    // Drop the guard constraint so subsequent assertions operate on the real
    // schema (each test_db() is fresh, but be explicit).
    sqlx::query("ALTER TABLE task_attempts DROP CONSTRAINT IF EXISTS test_rb_guard")
        .execute(db.pool())
        .await
        .unwrap();

    // Every member must retain its pre-call outcome and evidence — the
    // transaction rolled back all member updates atomically. No row was
    // advanced, no evidence was persisted, no terminal_at was stamped.
    for id in &ids {
        let row = repo.get(id).await.unwrap().unwrap();
        assert_eq!(
            row.outcome, "pending",
            "member {} must remain pending after rollback",
            id
        );
        assert!(
            row.summary.is_none(),
            "member {} summary must be absent after rollback",
            id
        );
        assert!(
            row.summary_json.is_none(),
            "member {} summary_json must be absent after rollback",
            id
        );
        assert!(
            row.terminal_at.is_none(),
            "member {} terminal_at must be unset after rollback",
            id
        );
    }
}
