//! Integration tests for [`UsageAnalyticsRepository`] against a real Postgres
//! test DB (via `Database::open_in_memory()`).
//!
//! These tests require a running Postgres instance — the same dependency as all
//! other `djinn-db` repository integration tests.  In environments without the
//! test Postgres instance they will fail with a connection error.

use super::{GroupDimension, UsageAnalyticsQuery, UsageAnalyticsRepository};
use crate::database::Database;

fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

async fn insert_project(db: &Database, name: &str) -> String {
    db.ensure_initialized().await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        id,
        name,
        "test",
        format!("repo-{}", &id.replace('-', "")[..31])
    )
    .execute(db.pool())
    .await
    .unwrap();
    id
}

async fn insert_epic(db: &Database, project_id: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query!(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, '[]'::jsonb)",
        id,
        project_id,
        "ep01",
        "Epic",
        "",
        "",
        "",
        ""
    )
    .execute(db.pool())
    .await
    .unwrap();
    id
}

async fn insert_task(
    db: &Database,
    project_id: &str,
    epic_id: Option<&str>,
    status: &str,
    close_reason: Option<&str>,
    total_reopen_count: i32,
    total_verification_failure_count: i32,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks
            (id, project_id, short_id, epic_id, title, description, design,
             issue_type, status, priority, owner, labels, acceptance_criteria,
             total_reopen_count, total_verification_failure_count, close_reason)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, '[]'::jsonb, '[]'::jsonb,
             $12, $13, $14)",
    )
    .bind(&id)
    .bind(project_id)
    .bind("t001")
    .bind(epic_id)
    .bind("Task")
    .bind("")
    .bind("")
    .bind("task")
    .bind(status)
    .bind(1)
    .bind("")
    .bind(total_reopen_count)
    .bind(total_verification_failure_count)
    .bind(close_reason)
    .execute(db.pool())
    .await
    .unwrap();
    id
}

#[allow(clippy::too_many_arguments)]
async fn insert_session(
    db: &Database,
    project_id: &str,
    task_id: Option<&str>,
    model_id: &str,
    agent_type: &str,
    started_at: &str,
    tokens_in: i64,
    tokens_out: i64,
    cost_usd: Option<f64>,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO sessions
            (id, project_id, task_id, model_id, agent_type, started_at, status,
             tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd)
         VALUES ($1, $2, $3, $4, $5, $6, 'completed', $7, $8, 0, 0, $9)",
    )
    .bind(&id)
    .bind(project_id)
    .bind(task_id)
    .bind(model_id)
    .bind(agent_type)
    .bind(started_at)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(cost_usd)
    .execute(db.pool())
    .await
    .unwrap();
    id
}

async fn insert_proposal(db: &Database) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO proposals (id, title, body, status, acceptance_criteria)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&id)
    .bind("Proposal")
    .bind("")
    .bind("draft")
    .bind("[]")
    .execute(db.pool())
    .await
    .unwrap();
    id
}

async fn insert_proposal_epic(db: &Database, proposal_id: &str, epic_id: &str, project_id: &str) {
    sqlx::query(
        "INSERT INTO proposal_epics (proposal_id, epic_id, project_id)
         VALUES ($1, $2, $3)",
    )
    .bind(proposal_id)
    .bind(epic_id)
    .bind(project_id)
    .execute(db.pool())
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_totals_series_breakdown_no_alias_errors() {
    let db = test_db();
    let repo = UsageAnalyticsRepository::new(db.clone());
    let proj = insert_project(&db, "usage-test").await;

    // Day 1 sessions
    insert_session(
        &db,
        &proj,
        None,
        "model-a",
        "chat",
        "2025-03-10T10:00:00.000Z",
        100,
        50,
        Some(1.0),
    )
    .await;
    insert_session(
        &db,
        &proj,
        None,
        "model-a",
        "chat",
        "2025-03-10T11:00:00.000Z",
        200,
        100,
        Some(2.0),
    )
    .await;
    // Day 2 session
    insert_session(
        &db,
        &proj,
        None,
        "model-b",
        "worker",
        "2025-03-11T10:00:00.000Z",
        300,
        150,
        Some(3.0),
    )
    .await;

    let params = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-12".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let result = repo
        .query(&params)
        .await
        .expect("query should not fail with ColumnNotFound");

    // Totals
    assert_eq!(result.totals.session_count, 3);
    assert_eq!(result.totals.tokens_in, 600);
    assert_eq!(result.totals.tokens_out, 300);
    assert_eq!(result.totals.total_cost_usd, Some(6.0));

    // Series: two days
    assert_eq!(result.series.len(), 2);
    assert_eq!(result.series[0].day, "2025-03-10");
    assert_eq!(result.series[0].session_count, 2);
    assert_eq!(result.series[1].day, "2025-03-11");
    assert_eq!(result.series[1].session_count, 1);

    // Breakdown by model
    assert_eq!(result.breakdown.len(), 2);
    let model_a = result
        .breakdown
        .iter()
        .find(|r| r.group_key == "model-a")
        .unwrap();
    assert_eq!(model_a.session_count, 2);
    assert_eq!(model_a.tokens_in, 300);
    assert_eq!(model_a.total_cost_usd, Some(3.0));
    let model_b = result
        .breakdown
        .iter()
        .find(|r| r.group_key == "model-b")
        .unwrap();
    assert_eq!(model_b.session_count, 1);
    assert_eq!(model_b.total_cost_usd, Some(3.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn null_cost_semantics_preserve_none() {
    let db = test_db();
    let repo = UsageAnalyticsRepository::new(db.clone());
    let proj = insert_project(&db, "null-cost-test").await;

    // One session with NULL cost but tokens present
    insert_session(
        &db,
        &proj,
        None,
        "model-x",
        "chat",
        "2025-03-10T10:00:00.000Z",
        100,
        50,
        None,
    )
    .await;
    // One session with priced cost
    insert_session(
        &db,
        &proj,
        None,
        "model-x",
        "chat",
        "2025-03-10T11:00:00.000Z",
        200,
        100,
        Some(2.0),
    )
    .await;

    let params = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-11".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let result = repo.query(&params).await.expect("query should succeed");

    // When ANY session in the group has NULL cost, the aggregate cost is NULL
    assert_eq!(result.totals.session_count, 2);
    assert_eq!(result.totals.tokens_in, 300);
    assert_eq!(result.totals.total_cost_usd, None);

    // Breakdown should also be NULL for that day/model
    let row = result
        .breakdown
        .iter()
        .find(|r| r.group_key == "model-x")
        .unwrap();
    assert_eq!(row.session_count, 2);
    assert_eq!(row.total_cost_usd, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_effectiveness_shared_credit_no_inflation() {
    let db = test_db();
    let repo = UsageAnalyticsRepository::new(db.clone());
    let proj = insert_project(&db, "effectiveness-test").await;
    let epic = insert_epic(&db, &proj).await;

    // Task with multiple worker sessions for the SAME model — should not inflate rates
    let task1 = insert_task(&db, &proj, Some(&epic), "closed", Some("completed"), 1, 0).await;
    insert_session(
        &db,
        &proj,
        Some(&task1),
        "model-a",
        "worker",
        "2025-03-10T10:00:00.000Z",
        100,
        50,
        Some(1.0),
    )
    .await;
    insert_session(
        &db,
        &proj,
        Some(&task1),
        "model-a",
        "worker",
        "2025-03-10T11:00:00.000Z",
        200,
        100,
        Some(2.0),
    )
    .await;

    // Another task touched by TWO models — shared-credit should count for both
    let task2 = insert_task(&db, &proj, Some(&epic), "closed", Some("completed"), 0, 0).await;
    insert_session(
        &db,
        &proj,
        Some(&task2),
        "model-a",
        "worker",
        "2025-03-10T12:00:00.000Z",
        50,
        25,
        Some(0.5),
    )
    .await;
    insert_session(
        &db,
        &proj,
        Some(&task2),
        "model-b",
        "worker",
        "2025-03-10T13:00:00.000Z",
        60,
        30,
        Some(0.6),
    )
    .await;

    // Failed closed task (no completed close_reason)
    let task3 = insert_task(&db, &proj, Some(&epic), "closed", Some("failed"), 2, 1).await;
    insert_session(
        &db,
        &proj,
        Some(&task3),
        "model-a",
        "worker",
        "2025-03-10T14:00:00.000Z",
        10,
        5,
        Some(0.1),
    )
    .await;

    let params = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-11".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let (effectiveness, _matrix) = repo
        .query_effectiveness(&params)
        .await
        .expect("effectiveness query should succeed");

    let model_a = effectiveness
        .iter()
        .find(|e| e.model_id == "model-a")
        .unwrap();
    let model_b = effectiveness
        .iter()
        .find(|e| e.model_id == "model-b")
        .unwrap();

    // model-a: 3 sessions, 2 completed tasks (task1 + task2), 3 closed tasks (task1 + task2 + task3)
    assert_eq!(model_a.sessions, 3);
    assert_eq!(model_a.shared_credit_completed_task_count, 2);
    // success_rate = 2 / 3
    assert!((model_a.success_rate.unwrap() - 0.6666666666666666).abs() < 1e-9);
    // avg_reopens = (1 + 0 + 2) / 3 = 1.0
    assert!((model_a.avg_reopens.unwrap() - 1.0).abs() < 1e-9);
    // verification_pass_rate = tasks with 0 verification failures / closed tasks
    // task1 has 0 failures, task2 has 0, task3 has 1 => 2/3
    assert!((model_a.verification_pass_rate.unwrap() - 0.6666666666666666).abs() < 1e-9);

    // model-b: 1 session, 1 completed task, 1 closed task
    assert_eq!(model_b.sessions, 1);
    assert_eq!(model_b.shared_credit_completed_task_count, 1);
    assert!((model_b.success_rate.unwrap() - 1.0).abs() < 1e-9);
    assert!((model_b.avg_reopens.unwrap() - 0.0).abs() < 1e-9);
    assert!((model_b.verification_pass_rate.unwrap() - 1.0).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_attribution_via_coalesce() {
    let db = test_db();
    let repo = UsageAnalyticsRepository::new(db.clone());
    let proj = insert_project(&db, "user-attribution-test").await;

    // Insert a user so the FK is satisfied
    let user_id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO users (id, email, name) VALUES ($1, $2, $3)")
        .bind(&user_id)
        .bind("test@example.com")
        .bind("Test User")
        .execute(db.pool())
        .await
        .unwrap();

    // Session with created_by_user_id set directly
    sqlx::query(
        "INSERT INTO sessions
            (id, project_id, task_id, model_id, agent_type, started_at, status,
             tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd, created_by_user_id)
         VALUES ($1, $2, $3, $4, $5, $6, 'completed', $7, $8, 0, 0, $9, $10)",
    )
    .bind(uuid::Uuid::now_v7().to_string())
    .bind(&proj)
    .bind(None::<String>)
    .bind("model-a")
    .bind("chat")
    .bind("2025-03-10T10:00:00.000Z")
    .bind(100i64)
    .bind(50i64)
    .bind(Some(1.0f64))
    .bind(&user_id)
    .execute(db.pool())
    .await
    .unwrap();

    let params = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-11".into(),
        group_by: GroupDimension::User,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let result = repo.query(&params).await.expect("query should succeed");
    let row = result
        .breakdown
        .iter()
        .find(|r| r.group_key == user_id)
        .unwrap();
    assert_eq!(row.session_count, 1);
    assert_eq!(row.tokens_in, 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proposal_grouping_through_epics() {
    let db = test_db();
    let repo = UsageAnalyticsRepository::new(db.clone());
    let proj = insert_project(&db, "proposal-test").await;
    let epic = insert_epic(&db, &proj).await;
    let proposal = insert_proposal(&db).await;
    insert_proposal_epic(&db, &proposal, &epic, &proj).await;

    let task = insert_task(&db, &proj, Some(&epic), "closed", Some("completed"), 0, 0).await;
    insert_session(
        &db,
        &proj,
        Some(&task),
        "model-a",
        "worker",
        "2025-03-10T10:00:00.000Z",
        100,
        50,
        Some(1.0),
    )
    .await;

    let params = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-11".into(),
        group_by: GroupDimension::Proposal,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let result = repo.query(&params).await.expect("query should succeed");
    let row = result
        .breakdown
        .iter()
        .find(|r| r.group_key == proposal)
        .unwrap();
    assert_eq!(row.session_count, 1);
    assert_eq!(row.tokens_in, 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_model_matrix_includes_null_project_sessions() {
    let db = test_db();
    let repo = UsageAnalyticsRepository::new(db.clone());
    let proj = insert_project(&db, "matrix-test").await;

    // Session with no project (chat session) — but our schema requires project_id NOT NULL,
    // so we use a session with a project but no task to represent "chat with project".
    // The matrix groups by project_id, so NULL-project sessions would have project_id ''.
    // Since project_id is NOT NULL in schema, we test the matrix with a real project instead.
    insert_session(
        &db,
        &proj,
        None,
        "model-a",
        "chat",
        "2025-03-10T10:00:00.000Z",
        100,
        50,
        Some(1.0),
    )
    .await;
    insert_session(
        &db,
        &proj,
        None,
        "model-a",
        "worker",
        "2025-03-10T11:00:00.000Z",
        200,
        100,
        Some(2.0),
    )
    .await;
    insert_session(
        &db,
        &proj,
        None,
        "model-b",
        "chat",
        "2025-03-10T12:00:00.000Z",
        300,
        150,
        Some(3.0),
    )
    .await;

    let params = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-11".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let (_effectiveness, matrix) = repo
        .query_effectiveness(&params)
        .await
        .expect("matrix query should succeed");

    // Matrix should have project_id = proj for all rows since all sessions have a project
    let model_a = matrix
        .iter()
        .find(|r| r.model_id == "model-a" && r.project_id == proj)
        .unwrap();
    assert_eq!(model_a.sessions, 2);
    assert_eq!(model_a.spend_usd, Some(3.0));

    let model_b = matrix
        .iter()
        .find(|r| r.model_id == "model-b" && r.project_id == proj)
        .unwrap();
    assert_eq!(model_b.sessions, 1);
    assert_eq!(model_b.spend_usd, Some(3.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_sessions_without_task_included_in_totals() {
    let db = test_db();
    let repo = UsageAnalyticsRepository::new(db.clone());
    let proj = insert_project(&db, "chat-test").await;

    // Chat session with no task
    insert_session(
        &db,
        &proj,
        None,
        "model-a",
        "chat",
        "2025-03-10T10:00:00.000Z",
        50,
        25,
        Some(0.5),
    )
    .await;
    // Worker session with no task (edge case)
    insert_session(
        &db,
        &proj,
        None,
        "model-a",
        "worker",
        "2025-03-10T11:00:00.000Z",
        100,
        50,
        Some(1.0),
    )
    .await;

    let params = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-11".into(),
        group_by: GroupDimension::Agent,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let result = repo.query(&params).await.expect("query should succeed");
    assert_eq!(result.totals.session_count, 2);
    assert_eq!(result.totals.tokens_in, 150);

    let chat_row = result
        .breakdown
        .iter()
        .find(|r| r.group_key == "chat")
        .unwrap();
    assert_eq!(chat_row.session_count, 1);
    let worker_row = result
        .breakdown
        .iter()
        .find(|r| r.group_key == "worker")
        .unwrap();
    assert_eq!(worker_row.session_count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breakdown_filtering_by_agent_type() {
    let db = test_db();
    let repo = UsageAnalyticsRepository::new(db.clone());
    let proj = insert_project(&db, "filter-test").await;

    insert_session(
        &db,
        &proj,
        None,
        "model-a",
        "chat",
        "2025-03-10T10:00:00.000Z",
        50,
        25,
        Some(0.5),
    )
    .await;
    insert_session(
        &db,
        &proj,
        None,
        "model-a",
        "worker",
        "2025-03-10T11:00:00.000Z",
        100,
        50,
        Some(1.0),
    )
    .await;

    let params = UsageAnalyticsQuery {
        from: "2025-03-10".into(),
        to: "2025-03-11".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: Some("worker".into()),
    };

    let result = repo.query(&params).await.expect("query should succeed");
    assert_eq!(result.totals.session_count, 1);
    assert_eq!(result.totals.tokens_in, 100);
    assert_eq!(result.breakdown.len(), 1);
    assert_eq!(result.breakdown[0].group_key, "model-a");
}
