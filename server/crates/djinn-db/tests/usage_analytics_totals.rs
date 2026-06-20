//! Integration test: verify `UsageAnalyticsRepository::query()` returns correct
//! `UsageTotals` against a live Postgres database.
//!
//! Guards against the NUMERIC→i64 decode panic that occurred because
//! `SUM(bigint)` returns `NUMERIC` in Postgres.  The fix casts each integer
//! aggregate to `::bigint` so `row.get::<i64>(…)` succeeds.
//!
//! Also includes model-effectiveness regression tests exercising shared-credit
//! attribution, NULL-cost semantics, no-inflation of task counts, and
//! verification_pass_rate correctness.

use djinn_db::repositories::usage_analytics::GroupDimension;
use djinn_db::{Database, ModelEffectivenessRow, UsageAnalyticsQuery, UsageAnalyticsRepository};

/// Helper: create an in-memory test database (cloned from djinn_test_template).
fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Helper: build a `UsageAnalyticsQuery` for effectiveness tests.
///
/// Date range covers all seeded data (2025-01-01 .. 2025-12-31).
fn effectiveness_params() -> UsageAnalyticsQuery {
    UsageAnalyticsQuery {
        from: "2025-01-01".into(),
        to: "2025-12-31".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
    }
}

/// Seed a project row, returning its id.
async fn seed_project(db: &Database, name: &str) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&id)
    .bind(name)
    .bind(format!("owner-{id}"))
    .bind(format!("repo-{id}"))
    .execute(db.pool())
    .await
    .expect("insert project");
    id
}

/// Seed a task row with the given status/close_reason/verification fields.
async fn seed_task(
    db: &Database,
    task_id: &str,
    project_id: &str,
    status: &str,
    close_reason: Option<&str>,
    total_reopen_count: i32,
    total_verification_failure_count: i32,
) {
    let short_id = format!("t{}", &task_id[..10.min(task_id.len())]);
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, \
          status, close_reason, \
          total_reopen_count, total_verification_failure_count, \
          labels, acceptance_criteria, memory_refs) \
         VALUES ($1, $2, $3, $4, '', '', \
                 $5, $6, $7, $8, \
                 '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(&short_id)
    .bind(format!("task-{task_id}"))
    .bind(status)
    .bind(close_reason)
    .bind(total_reopen_count)
    .bind(total_verification_failure_count)
    .execute(db.pool())
    .await
    .expect("insert task");
}

/// Seed a worker session linked to a task with a given cost (or NULL).
#[allow(clippy::too_many_arguments)]
async fn seed_worker_session(
    db: &Database,
    session_id: &str,
    project_id: &str,
    task_id: &str,
    model_id: &str,
    cost_usd: Option<f64>,
    tokens_in: i64,
    tokens_out: i64,
    started_at: &str,
) {
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, task_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, \
          cache_read_tokens, cache_write_tokens, cost_usd) \
         VALUES ($1, $2, $3, $4, 'worker', 'completed', \
                 $5, $6, $7, 0, 0, $8)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(task_id)
    .bind(model_id)
    .bind(started_at)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(cost_usd)
    .execute(db.pool())
    .await
    .expect("insert worker session");
}

/// Find a model effectiveness row by model_id, panicking if not found.
fn find_model(rows: &[ModelEffectivenessRow], model_id: &str) -> ModelEffectivenessRow {
    rows.iter()
        .find(|r| r.model_id == model_id)
        .cloned()
        .unwrap_or_else(|| panic!("model {model_id} not found in effectiveness rows"))
}

/// Seed a project and one session row with known token counts and a non-NULL
/// cost_usd so the totals query has something to aggregate.
async fn seed_session(db: &Database) {
    db.ensure_initialized().await.expect("ensure_initialized");
    let project_id = uuid::Uuid::now_v7().to_string();
    let session_id = uuid::Uuid::now_v7().to_string();

    // Insert a project (FK target).
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&project_id)
    .bind("test-proj")
    .bind("test-owner")
    .bind("test-repo")
    .execute(db.pool())
    .await
    .expect("insert project");

    // Insert a session with non-trivial token counts and a known cost.
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, \
          cache_read_tokens, cache_write_tokens, cost_usd) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(&session_id)
    .bind(&project_id)
    .bind("gpt-4o")
    .bind("worker")
    .bind("completed")
    .bind("2025-06-15T10:00:00.000Z")
    .bind(1234_i64) // tokens_in
    .bind(5678_i64) // tokens_out
    .bind(100_i64) // cache_read_tokens
    .bind(200_i64) // cache_write_tokens
    .bind(0.042_f64) // cost_usd
    .execute(db.pool())
    .await
    .expect("insert session");
}

/// Seed a session with NULL cost_usd to test the NULL-cost semantic.
async fn seed_unpriced_session(db: &Database) {
    db.ensure_initialized().await.expect("ensure_initialized");
    let project_id = uuid::Uuid::now_v7().to_string();
    let session_id = uuid::Uuid::now_v7().to_string();

    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&project_id)
    .bind("test-proj-unpriced")
    .bind(format!("owner-{session_id}"))
    .bind(format!("repo-{session_id}"))
    .execute(db.pool())
    .await
    .expect("insert project");

    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, \
          cache_read_tokens, cache_write_tokens, cost_usd) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(&session_id)
    .bind(&project_id)
    .bind("claude-3-opus")
    .bind("worker")
    .bind("completed")
    .bind("2025-06-15T11:00:00.000Z")
    .bind(999_i64)
    .bind(888_i64)
    .bind(50_i64)
    .bind(75_i64)
    .bind(None::<f64>) // NULL cost_usd (unpriced)
    .execute(db.pool())
    .await
    .expect("insert unpriced session");
}

/// Verify the totals query decodes i64 columns without panicking and returns
/// the correct aggregated values for a single session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_totals_decodes_i64_from_sum() {
    let db = create_test_db();
    seed_session(&db).await;

    let repo = UsageAnalyticsRepository::new(db);
    let params = UsageAnalyticsQuery {
        from: "2025-01-01".into(),
        to: "2025-12-31".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let result = repo.query(&params).await.expect("query should succeed");

    assert_eq!(result.totals.session_count, 1);
    assert_eq!(result.totals.tokens_in, 1234);
    assert_eq!(result.totals.tokens_out, 5678);
    assert_eq!(result.totals.cache_read_tokens, 100);
    assert_eq!(result.totals.cache_write_tokens, 200);
    // total_cost_usd should be Some(0.042) — no NULL sessions, so no NULL
    // cost semantic applies.
    let cost = result.totals.total_cost_usd.expect("cost should be Some");
    assert!((cost - 0.042_f64).abs() < 1e-9, "cost mismatch: {cost}");

    // Series should have exactly one day bucket.
    assert_eq!(result.series.len(), 1);
    assert_eq!(result.series[0].day, "2025-06-15");
    assert_eq!(result.series[0].tokens_in, 1234);
    assert_eq!(result.series[0].tokens_out, 5678);
}

/// Verify NULL-cost semantics: when ANY matching session has NULL cost_usd,
/// the aggregate total_cost_usd must be NULL (not $0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_totals_null_cost_semantics_preserved() {
    let db = create_test_db();
    seed_session(&db).await;
    seed_unpriced_session(&db).await;

    let repo = UsageAnalyticsRepository::new(db);
    let params = UsageAnalyticsQuery {
        from: "2025-01-01".into(),
        to: "2025-12-31".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let result = repo.query(&params).await.expect("query should succeed");

    assert_eq!(result.totals.session_count, 2);
    // Token counts should be summed correctly across both sessions.
    assert_eq!(result.totals.tokens_in, 1234 + 999);
    assert_eq!(result.totals.tokens_out, 5678 + 888);
    assert_eq!(result.totals.cache_read_tokens, 100 + 50);
    assert_eq!(result.totals.cache_write_tokens, 200 + 75);
    // Because one session has NULL cost_usd, total_cost_usd must be NULL.
    assert!(
        result.totals.total_cost_usd.is_none(),
        "expected NULL total_cost_usd when any session is unpriced, got {:?}",
        result.totals.total_cost_usd,
    );
}

/// Verify the breakdown query also decodes correctly (same SUM→NUMERIC hazard).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breakdown_decodes_i64_from_sum() {
    let db = create_test_db();
    seed_session(&db).await;

    let repo = UsageAnalyticsRepository::new(db);
    let params = UsageAnalyticsQuery {
        from: "2025-01-01".into(),
        to: "2025-12-31".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
    };

    let result = repo.query(&params).await.expect("query should succeed");

    assert_eq!(result.breakdown.len(), 1);
    assert_eq!(result.breakdown[0].tokens_in, 1234);
    assert_eq!(result.breakdown[0].tokens_out, 5678);
    assert_eq!(result.breakdown[0].cache_read_tokens, 100);
    assert_eq!(result.breakdown[0].cache_write_tokens, 200);
}

// ── Model effectiveness regression tests ───────────────────────────────────

/// Shared-credit attribution: a task touched by two models is counted once for
/// each model's `shared_credit_completed_task_count`.
///
/// Seed data:
///   - task-1 (closed/completed): sessions from model-a and model-b
///   - task-2 (closed/completed): session from model-a only
///   - task-3 (closed/failed):    session from model-b only
///
/// Expected:
///   model-a: completed_count=2 (task-1, task-2), closed_count=2, success_rate=1.0
///   model-b: completed_count=1 (task-1), closed_count=2, success_rate=0.5
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effectiveness_shared_credit_attribution() {
    let db = create_test_db();
    db.ensure_initialized().await.expect("ensure_initialized");

    let proj = seed_project(&db, "shared-credit-proj").await;

    // Task 1: closed/completed, no reopens, no verification failures.
    seed_task(&db, "task-1-aaa", &proj, "closed", Some("completed"), 0, 0).await;
    // Task 2: closed/completed.
    seed_task(&db, "task-2-bbb", &proj, "closed", Some("completed"), 0, 0).await;
    // Task 3: closed/failed.
    seed_task(&db, "task-3-ccc", &proj, "closed", Some("failed"), 0, 1).await;

    // model-a: sessions on task-1 and task-2
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-1-aaa",
        "model-a",
        Some(0.05),
        100,
        200,
        "2025-06-10T10:00:00.000Z",
    )
    .await;
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-2-bbb",
        "model-a",
        Some(0.03),
        150,
        250,
        "2025-06-11T10:00:00.000Z",
    )
    .await;

    // model-b: sessions on task-1 and task-3
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-1-aaa",
        "model-b",
        Some(0.08),
        200,
        300,
        "2025-06-10T11:00:00.000Z",
    )
    .await;
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-3-ccc",
        "model-b",
        Some(0.01),
        80,
        120,
        "2025-06-12T10:00:00.000Z",
    )
    .await;

    let repo = UsageAnalyticsRepository::new(db);
    let (effectiveness, _matrix) = repo
        .query_effectiveness(&effectiveness_params())
        .await
        .expect("query_effectiveness should succeed");

    // ── model-a ──
    let a = find_model(&effectiveness, "model-a");
    assert_eq!(a.sessions, 2, "model-a should have 2 sessions");
    // Both task-1 and task-2 are completed → shared_credit_completed_task_count = 2
    assert_eq!(
        a.shared_credit_completed_task_count, 2,
        "model-a should get credit for task-1 and task-2"
    );
    // All closed tasks are completed → success_rate = 1.0
    let sr_a = a.success_rate.expect("success_rate should be Some");
    assert!(
        (sr_a - 1.0_f64).abs() < 1e-9,
        "model-a success_rate should be 1.0, got {sr_a}"
    );
    // Spend = 0.05 + 0.03
    let spend_a = a.spend_usd.expect("model-a spend should be Some");
    assert!(
        (spend_a - 0.08_f64).abs() < 1e-9,
        "model-a spend mismatch: {spend_a}"
    );

    // ── model-b ──
    let b = find_model(&effectiveness, "model-b");
    assert_eq!(b.sessions, 2, "model-b should have 2 sessions");
    // Only task-1 is completed among model-b's tasks; task-3 is failed.
    assert_eq!(
        b.shared_credit_completed_task_count, 1,
        "model-b should get credit for task-1 only"
    );
    // closed_count=2 (task-1 completed, task-3 failed), completed=1 → 0.5
    let sr_b = b.success_rate.expect("success_rate should be Some");
    assert!(
        (sr_b - 0.5_f64).abs() < 1e-9,
        "model-b success_rate should be 0.5, got {sr_b}"
    );
    // Spend = 0.08 + 0.01
    let spend_b = b.spend_usd.expect("model-b spend should be Some");
    assert!(
        (spend_b - 0.09_f64).abs() < 1e-9,
        "model-b spend mismatch: {spend_b}"
    );
}

/// Multiple worker sessions from the same model on the same completed task
/// must count as exactly 1 completed task (no inflation).
///
/// Also asserts success_rate and verification_pass_rate never exceed 1.0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effectiveness_no_inflation_from_duplicate_sessions() {
    let db = create_test_db();
    db.ensure_initialized().await.expect("ensure_initialized");

    let proj = seed_project(&db, "no-inflation-proj").await;

    // One completed task with reopens=2 and zero verification failures.
    seed_task(&db, "task-dup-1", &proj, "closed", Some("completed"), 2, 0).await;

    // Three worker sessions from the same model on the same task.
    for i in 0..3 {
        seed_worker_session(
            &db,
            &uuid::Uuid::now_v7().to_string(),
            &proj,
            "task-dup-1",
            "model-x",
            Some(0.01 * (i as f64 + 1.0)),
            100 * (i as i64 + 1),
            200 * (i as i64 + 1),
            &format!("2025-06-1{:?}T10:00:00.000Z", i),
        )
        .await;
    }

    let repo = UsageAnalyticsRepository::new(db);
    let (effectiveness, _matrix) = repo
        .query_effectiveness(&effectiveness_params())
        .await
        .expect("query_effectiveness should succeed");

    let x = find_model(&effectiveness, "model-x");

    // 3 sessions recorded at the session level.
    assert_eq!(x.sessions, 3, "model-x should have 3 sessions");

    // But only 1 distinct task → shared_credit_completed_task_count = 1.
    assert_eq!(
        x.shared_credit_completed_task_count, 1,
        "3 sessions on 1 task should count as 1 completed task"
    );

    // success_rate must not exceed 1.0.
    let sr = x.success_rate.expect("success_rate should be Some");
    assert!(
        sr <= 1.0_f64 + 1e-9,
        "success_rate must not exceed 1.0, got {sr}"
    );
    assert!(
        (sr - 1.0_f64).abs() < 1e-9,
        "success_rate should be exactly 1.0 (1/1 closed tasks completed), got {sr}"
    );

    // verification_pass_rate must not exceed 1.0.
    let vpr = x
        .verification_pass_rate
        .expect("verification_pass_rate should be Some");
    assert!(
        vpr <= 1.0_f64 + 1e-9,
        "verification_pass_rate must not exceed 1.0, got {vpr}"
    );
    assert!(
        (vpr - 1.0_f64).abs() < 1e-9,
        "verification_pass_rate should be 1.0 (0 failures), got {vpr}"
    );

    // avg_reopens should reflect the task's total_reopen_count = 2.0.
    let avg = x.avg_reopens.expect("avg_reopens should be Some");
    assert!(
        (avg - 2.0_f64).abs() < 1e-9,
        "avg_reopens should be 2.0, got {avg}"
    );

    // cost_per_completed_task = total_spend / 1 task.
    let total_spend = 0.01 + 0.02 + 0.03; // 0.06
    let cpt = x
        .cost_per_completed_task
        .expect("cost_per_completed_task should be Some");
    assert!(
        (cpt - total_spend).abs() < 1e-9,
        "cost_per_completed_task should be {total_spend}, got {cpt}"
    );
}

/// NULL-cost semantics in effectiveness: a model whose sessions all have
/// `cost_usd = NULL` must have `spend_usd = None` (not 0.0), while tokens
/// are still counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effectiveness_null_cost_spend_remains_none() {
    let db = create_test_db();
    db.ensure_initialized().await.expect("ensure_initialized");

    let proj = seed_project(&db, "null-cost-eff-proj").await;

    // One completed task.
    seed_task(&db, "task-null-1", &proj, "closed", Some("completed"), 0, 0).await;

    // Priced session from model-priced.
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-null-1",
        "model-priced",
        Some(0.05),
        100,
        200,
        "2025-06-15T10:00:00.000Z",
    )
    .await;

    // Unpriced session from model-unpriced (NULL cost, non-zero tokens).
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-null-1",
        "model-unpriced",
        None,
        500,
        600,
        "2025-06-15T11:00:00.000Z",
    )
    .await;

    let repo = UsageAnalyticsRepository::new(db);
    let (effectiveness, _matrix) = repo
        .query_effectiveness(&effectiveness_params())
        .await
        .expect("query_effectiveness should succeed");

    // ── model-priced: spend_usd = Some(0.05) ──
    let priced = find_model(&effectiveness, "model-priced");
    let spend_priced = priced.spend_usd.expect("priced model spend should be Some");
    assert!(
        (spend_priced - 0.05_f64).abs() < 1e-9,
        "priced spend mismatch: {spend_priced}"
    );
    assert_eq!(priced.tokens_in, 100);
    assert_eq!(priced.tokens_out, 200);

    // ── model-unpriced: spend_usd must be None (not 0.0) ──
    let unpriced = find_model(&effectiveness, "model-unpriced");
    assert!(
        unpriced.spend_usd.is_none(),
        "model-unpriced spend_usd must be None (NULL cost), got {:?}",
        unpriced.spend_usd,
    );
    // Tokens are still counted despite NULL cost.
    assert_eq!(unpriced.tokens_in, 500, "tokens_in should be counted");
    assert_eq!(unpriced.tokens_out, 600, "tokens_out should be counted");
    assert_eq!(unpriced.sessions, 1, "should have 1 session");
}

/// Verify `verification_pass_rate` is present and correct: it should equal
/// the fraction of closed tasks with zero verification failures per model.
///
/// Seed data:
///   - task-v1 (closed/completed, verification_failures=0) → pass
///   - task-v2 (closed/completed, verification_failures=1) → fail
///   - task-v3 (closed/failed,    verification_failures=0) → pass (0 failures)
///
/// Expected for model-v:
///   closed_count = 3, verified_count = 2 → verification_pass_rate = 2/3
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effectiveness_verification_pass_rate_correctness() {
    let db = create_test_db();
    db.ensure_initialized().await.expect("ensure_initialized");

    let proj = seed_project(&db, "vpr-proj").await;

    // Task v1: closed/completed, 0 verification failures → pass.
    seed_task(&db, "task-v1-ddd", &proj, "closed", Some("completed"), 0, 0).await;
    // Task v2: closed/completed, 1 verification failure → fail.
    seed_task(&db, "task-v2-eee", &proj, "closed", Some("completed"), 0, 1).await;
    // Task v3: closed/failed, 0 verification failures → pass.
    seed_task(&db, "task-v3-fff", &proj, "closed", Some("failed"), 0, 0).await;

    // model-v: sessions on all three tasks.
    for (task_id, started) in [
        ("task-v1-ddd", "2025-06-10T10:00:00.000Z"),
        ("task-v2-eee", "2025-06-11T10:00:00.000Z"),
        ("task-v3-fff", "2025-06-12T10:00:00.000Z"),
    ] {
        seed_worker_session(
            &db,
            &uuid::Uuid::now_v7().to_string(),
            &proj,
            task_id,
            "model-v",
            Some(0.10),
            100,
            200,
            started,
        )
        .await;
    }

    let repo = UsageAnalyticsRepository::new(db);
    let (effectiveness, _matrix) = repo
        .query_effectiveness(&effectiveness_params())
        .await
        .expect("query_effectiveness should succeed");

    let v = find_model(&effectiveness, "model-v");

    assert_eq!(v.sessions, 3, "model-v should have 3 sessions");

    // All 3 tasks are closed; 2 are completed.
    assert_eq!(
        v.shared_credit_completed_task_count, 2,
        "model-v completed tasks should be 2 (task-v1, task-v2)"
    );

    let sr = v.success_rate.expect("success_rate should be Some");
    // completed_count=2, closed_count=3 → 2/3
    assert!(
        (sr - (2.0 / 3.0)).abs() < 1e-9,
        "success_rate should be ~0.6667, got {sr}"
    );

    // Verification pass rate: tasks with 0 failures among closed = task-v1, task-v3.
    // verified_count=2, closed_count=3 → 2/3.
    let vpr = v
        .verification_pass_rate
        .expect("verification_pass_rate should be Some");
    assert!(
        (vpr - (2.0 / 3.0)).abs() < 1e-9,
        "verification_pass_rate should be ~0.6667 (2/3), got {vpr}"
    );
}
