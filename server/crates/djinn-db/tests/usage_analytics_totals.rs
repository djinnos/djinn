//! Integration test: verify `UsageAnalyticsRepository::totals()` /
//! `series_detailed()` / `entity_breakdown()` return correct aggregates
//! against a live Postgres database.
//!
//! Guards against the NUMERIC→i64 decode panic that occurred because
//! `SUM(bigint)` returns `NUMERIC` in Postgres.  The fix casts each integer
//! aggregate to `::bigint` so `row.get::<i64>(…)` succeeds.
//!
//! Also includes model-effectiveness regression tests exercising shared-credit
//! attribution, NULL-cost semantics, and no-inflation of task counts.

use djinn_db::repositories::test_support::seed_test_user;
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
        user_id: None,
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

/// Seed a task row with the given status/close_reason fields.
async fn seed_task(
    db: &Database,
    task_id: &str,
    project_id: &str,
    status: &str,
    close_reason: Option<&str>,
    total_reopen_count: i32,
) {
    let creator = seed_test_user(db).await;
    let short_id = format!("t{}", &task_id[..10.min(task_id.len())]);
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, \
          status, close_reason, \
          total_reopen_count, \
          labels, acceptance_criteria, memory_refs, created_by_user_id) \
         VALUES ($1, $2, $3, $4, '', '', \
                 $5, $6, $7, \
                 '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, $8)",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(&short_id)
    .bind(format!("task-{task_id}"))
    .bind(status)
    .bind(close_reason)
    .bind(total_reopen_count)
    .bind(&creator)
    .execute(db.pool())
    .await
    .expect("insert task");
}

/// Seed a worker session linked to a task with a given cost (or NULL) and cost basis.
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
    cost_basis: &str,
) {
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, task_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, \
          cache_read_tokens, cache_write_tokens, cost_usd, cost_basis) \
         VALUES ($1, $2, $3, $4, 'worker', 'completed', \
                 $5, $6, $7, 0, 0, $8, $9)",
    )
    .bind(session_id)
    .bind(project_id)
    .bind(task_id)
    .bind(model_id)
    .bind(started_at)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(cost_usd)
    .bind(cost_basis)
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
          cache_read_tokens, cache_write_tokens, cost_usd, cost_basis) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'actual')",
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
          cache_read_tokens, cache_write_tokens, cost_usd, cost_basis) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'unpriced')",
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
        user_id: None,
    };

    let totals = repo.totals(&params).await.expect("totals should succeed");

    assert_eq!(totals.session_count, 1);
    assert_eq!(totals.tokens_in, 1234);
    assert_eq!(totals.tokens_out, 5678);
    assert_eq!(totals.cache_read_tokens, 100);
    assert_eq!(totals.cache_write_tokens, 200);
    // actual_spend_usd should be Some(0.042) — the only session is priced.
    let cost = totals.actual_spend_usd.expect("cost should be Some");
    assert!((cost - 0.042_f64).abs() < 1e-9, "cost mismatch: {cost}");
    assert_eq!(totals.unpriced_session_count, 0);

    // Detailed series should have exactly one (day, model, project, agent) row.
    let series = repo
        .series_detailed(&params)
        .await
        .expect("series should succeed");
    assert_eq!(series.len(), 1);
    assert_eq!(series[0].day, "2025-06-15");
    assert_eq!(series[0].tokens_in, 1234);
    assert_eq!(series[0].tokens_out, 5678);
}

/// Verify priced-subtotal semantics: a mix of priced and unpriced sessions
/// yields the sum over the *priced* sessions only (not NULL, not $0), and the
/// unpriced sessions are counted separately so the UI can qualify the estimate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_totals_reports_priced_subtotal_and_unpriced_count() {
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
        user_id: None,
    };

    let totals = repo.totals(&params).await.expect("totals should succeed");

    assert_eq!(totals.session_count, 2);
    // Token counts should be summed correctly across both sessions.
    assert_eq!(totals.tokens_in, 1234 + 999);
    assert_eq!(totals.tokens_out, 5678 + 888);
    assert_eq!(totals.cache_read_tokens, 100 + 50);
    assert_eq!(totals.cache_write_tokens, 200 + 75);
    // Spend is the priced subtotal (only the 0.042 session is priced); the
    // unpriced session is excluded from the sum but counted.
    let cost = totals
        .actual_spend_usd
        .expect("priced subtotal should be Some when at least one session is priced");
    assert!((cost - 0.042_f64).abs() < 1e-9, "cost mismatch: {cost}");
    assert_eq!(
        totals.unpriced_session_count, 1,
        "exactly one unpriced session should be counted"
    );
}

/// When *every* matching session is unpriced, the priced subtotal is NULL
/// (nothing to sum) and all sessions are reported as unpriced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_totals_all_unpriced_yields_none_subtotal() {
    let db = create_test_db();
    seed_unpriced_session(&db).await;

    let repo = UsageAnalyticsRepository::new(db);
    let params = UsageAnalyticsQuery {
        from: "2025-01-01".into(),
        to: "2025-12-31".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
        user_id: None,
    };

    let totals = repo.totals(&params).await.expect("totals should succeed");

    assert_eq!(totals.session_count, 1);
    assert!(
        totals.actual_spend_usd.is_none(),
        "expected NULL subtotal when no session is priced, got {:?}",
        totals.actual_spend_usd,
    );
    assert_eq!(totals.unpriced_session_count, 1);
}

/// Verify the entity breakdown query also decodes correctly (same SUM→NUMERIC
/// hazard) and aggregates tokens per entity.
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
        user_id: None,
    };

    let rows = repo
        .entity_breakdown(&params, GroupDimension::Project)
        .await
        .expect("entity_breakdown should succeed");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].tokens_in, 1234);
    assert_eq!(rows[0].tokens_out, 5678);
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

    // Task 1: closed/completed, no reopens.
    seed_task(&db, "task-1-aaa", &proj, "closed", Some("completed"), 0).await;
    // Task 2: closed/completed.
    seed_task(&db, "task-2-bbb", &proj, "closed", Some("completed"), 0).await;
    // Task 3: closed/failed.
    seed_task(&db, "task-3-ccc", &proj, "closed", Some("failed"), 0).await;

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
        "actual",
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
        "actual",
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
        "actual",
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
        "actual",
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
    let spend_a = a.actual_spend_usd.expect("model-a spend should be Some");
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
    let spend_b = b.actual_spend_usd.expect("model-b spend should be Some");
    assert!(
        (spend_b - 0.09_f64).abs() < 1e-9,
        "model-b spend mismatch: {spend_b}"
    );
}

/// Multiple worker sessions from the same model on the same completed task
/// must count as exactly 1 completed task (no inflation).
///
/// Also asserts success_rate never exceeds 1.0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effectiveness_no_inflation_from_duplicate_sessions() {
    let db = create_test_db();
    db.ensure_initialized().await.expect("ensure_initialized");

    let proj = seed_project(&db, "no-inflation-proj").await;

    // One completed task with reopens=2.
    seed_task(&db, "task-dup-1", &proj, "closed", Some("completed"), 2).await;

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
            "actual",
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

    // avg_reopens should reflect the task's total_reopen_count = 2.0.
    let avg = x.avg_reopens.expect("avg_reopens should be Some");
    assert!(
        (avg - 2.0_f64).abs() < 1e-9,
        "avg_reopens should be 2.0, got {avg}"
    );

    // actual_cost_per_completed_task = total_spend / 1 task.
    let total_spend = 0.01 + 0.02 + 0.03; // 0.06
    let cpt = x
        .actual_cost_per_completed_task
        .expect("actual_cost_per_completed_task should be Some");
    assert!(
        (cpt - total_spend).abs() < 1e-9,
        "actual_cost_per_completed_task should be {total_spend}, got {cpt}"
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
    seed_task(&db, "task-null-1", &proj, "closed", Some("completed"), 0).await;

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
        "actual",
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
        "unpriced",
    )
    .await;

    let repo = UsageAnalyticsRepository::new(db);
    let (effectiveness, _matrix) = repo
        .query_effectiveness(&effectiveness_params())
        .await
        .expect("query_effectiveness should succeed");

    // ── model-priced: spend_usd = Some(0.05) ──
    let priced = find_model(&effectiveness, "model-priced");
    let spend_priced = priced
        .actual_spend_usd
        .expect("priced model spend should be Some");
    assert!(
        (spend_priced - 0.05_f64).abs() < 1e-9,
        "priced spend mismatch: {spend_priced}"
    );
    assert_eq!(priced.tokens_in, 100);
    assert_eq!(priced.tokens_out, 200);

    // ── model-unpriced: spend_usd must be None (not 0.0) ──
    let unpriced = find_model(&effectiveness, "model-unpriced");
    assert!(
        unpriced.actual_spend_usd.is_none(),
        "model-unpriced spend_usd must be None (NULL cost), got {:?}",
        unpriced.actual_spend_usd,
    );
    // Tokens are still counted despite NULL cost.
    assert_eq!(unpriced.tokens_in, 500, "tokens_in should be counted");
    assert_eq!(unpriced.tokens_out, 600, "tokens_out should be counted");
    assert_eq!(unpriced.sessions, 1, "should have 1 session");
}

/// Mixed cost-basis split at the totals, row, and matrix grains:
/// seed one `actual`, one `projected`, and one `unpriced` session in the same
/// project/model scope, then verify:
///   - `actual_spend_usd` sums only actual-basis sessions.
///   - `projected_usd` sums only projected-basis sessions.
///   - `unpriced_session_count` counts the unpriced session.
///   - The unpriced session's cost is excluded from BOTH dollar figures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn totals_split_actual_projected_and_unpriced() {
    let db = create_test_db();
    db.ensure_initialized().await.expect("ensure_initialized");

    let proj = seed_project(&db, "split-basis-proj").await;

    // Task for effectiveness attribution.
    seed_task(&db, "task-split-1", &proj, "closed", Some("completed"), 0).await;

    // actual-basis session: $0.10
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-split-1",
        "model-split",
        Some(0.10),
        100,
        200,
        "2025-06-10T10:00:00.000Z",
        "actual",
    )
    .await;

    // projected-basis session: $0.25
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-split-1",
        "model-split",
        Some(0.25),
        150,
        250,
        "2025-06-11T10:00:00.000Z",
        "projected",
    )
    .await;

    // unpriced-basis session: NULL cost — excluded from both sums.
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-split-1",
        "model-split",
        None,
        50,
        75,
        "2025-06-12T10:00:00.000Z",
        "unpriced",
    )
    .await;

    let repo = UsageAnalyticsRepository::new(db);
    let params = UsageAnalyticsQuery {
        from: "2025-01-01".into(),
        to: "2025-12-31".into(),
        group_by: GroupDimension::Model,
        project_id: None,
        model_id: None,
        agent_type: None,
        user_id: None,
    };

    // ── Totals grain ──
    let totals = repo.totals(&params).await.expect("totals should succeed");
    assert_eq!(totals.session_count, 3, "three sessions total");
    assert_eq!(totals.tokens_in, 100 + 150 + 50);
    assert_eq!(totals.tokens_out, 200 + 250 + 75);
    // actual_spend_usd = 0.10 (only actual-basis).
    let actual = totals
        .actual_spend_usd
        .expect("actual_spend_usd should be Some");
    assert!(
        (actual - 0.10_f64).abs() < 1e-9,
        "actual spend should be 0.10, got {actual}"
    );
    // projected_usd = 0.25 (only projected-basis).
    let projected = totals.projected_usd.expect("projected_usd should be Some");
    assert!(
        (projected - 0.25_f64).abs() < 1e-9,
        "projected should be 0.25, got {projected}"
    );
    assert_eq!(
        totals.unpriced_session_count, 1,
        "exactly one unpriced session"
    );

    // ── Effectiveness / model-effectiveness grain ──
    let (effectiveness, matrix) = repo
        .query_effectiveness(&params)
        .await
        .expect("query_effectiveness should succeed");

    let model_row = find_model(&effectiveness, "model-split");
    assert_eq!(model_row.sessions, 3, "model-split has 3 sessions");
    let me_actual = model_row
        .actual_spend_usd
        .expect("model actual_spend_usd should be Some");
    assert!(
        (me_actual - 0.10_f64).abs() < 1e-9,
        "model actual should be 0.10, got {me_actual}"
    );
    let me_proj = model_row
        .projected_usd
        .expect("model projected_usd should be Some");
    assert!(
        (me_proj - 0.25_f64).abs() < 1e-9,
        "model projected should be 0.25, got {me_proj}"
    );
    assert_eq!(model_row.unpriced_session_count, 1, "model unpriced count");

    // ── Project × model matrix grain ──
    let cell = matrix
        .iter()
        .find(|c| c.project_id == proj && c.model_id == "model-split")
        .expect("matrix cell for split-basis-proj / model-split");
    let cell_actual = cell
        .actual_spend_usd
        .expect("cell actual_spend_usd should be Some");
    assert!(
        (cell_actual - 0.10_f64).abs() < 1e-9,
        "cell actual should be 0.10, got {cell_actual}"
    );
    let cell_proj = cell
        .projected_usd
        .expect("cell projected_usd should be Some");
    assert!(
        (cell_proj - 0.25_f64).abs() < 1e-9,
        "cell projected should be 0.25, got {cell_proj}"
    );
    assert_eq!(cell.unpriced_session_count, 1, "cell unpriced count");

    // ── Entity breakdown grain (by_project) ──
    let breakdown = repo
        .entity_breakdown(&params, GroupDimension::Project)
        .await
        .expect("entity_breakdown should succeed");
    let proj_row = breakdown
        .iter()
        .find(|r| r.id == proj)
        .expect("breakdown row for split-basis-proj");
    let bd_actual = proj_row
        .actual_spend_usd
        .expect("breakdown actual_spend_usd should be Some");
    assert!(
        (bd_actual - 0.10_f64).abs() < 1e-9,
        "breakdown actual should be 0.10, got {bd_actual}"
    );
    let bd_proj = proj_row
        .projected_usd
        .expect("breakdown projected_usd should be Some");
    assert!(
        (bd_proj - 0.25_f64).abs() < 1e-9,
        "breakdown projected should be 0.25, got {bd_proj}"
    );
    assert_eq!(proj_row.unpriced_session_count, 1, "breakdown unpriced");
}

/// Regression for shared-credit masking the worst first-pass model.
///
/// Scenario mirrors the production finding: a "first-pass" model runs early
/// worker sessions whose passes are reopened/reworked, while a second model
/// runs the later session that actually lands the merge. Shared-credit
/// `success_rate` flatters the first-pass model; the new
/// `first_pass_rejection_rate` and `final_pass_share` columns must expose it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effectiveness_first_pass_rejection_and_final_pass_share() {
    let db = create_test_db();
    db.ensure_initialized().await.expect("ensure_initialized");

    let proj = seed_project(&db, "first-pass-proj").await;

    // Task 1: closed/completed, reopened once (first pass was rejected).
    seed_task(&db, "task-1-aaa", &proj, "closed", Some("completed"), 1).await;
    // Task 2: closed/completed, single clean pass, no reopens.
    seed_task(&db, "task-2-bbb", &proj, "closed", Some("completed"), 0).await;

    // Task 1: mimo runs first (rejected), gpt runs the landing session later.
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-1-aaa",
        "mimo",
        Some(0.05),
        100,
        200,
        "2025-06-01T10:00:00.000Z",
        "actual",
    )
    .await;
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-1-aaa",
        "gpt",
        Some(0.03),
        150,
        250,
        "2025-06-01T12:00:00.000Z",
        "actual",
    )
    .await;
    // Task 2: gpt lands it in a single pass.
    seed_worker_session(
        &db,
        &uuid::Uuid::now_v7().to_string(),
        &proj,
        "task-2-bbb",
        "gpt",
        Some(0.02),
        120,
        180,
        "2025-06-02T10:00:00.000Z",
        "actual",
    )
    .await;

    let repo = UsageAnalyticsRepository::new(db);
    let (effectiveness, _matrix) = repo
        .query_effectiveness(&effectiveness_params())
        .await
        .expect("query_effectiveness should succeed");

    // ── mimo: best shared-credit success, worst first-pass reality ──
    let mimo = find_model(&effectiveness, "mimo");
    assert_eq!(mimo.sessions, 1);
    // Shared credit: mimo touched completed task-1 → success looks perfect.
    assert!(
        (mimo.success_rate.unwrap() - 1.0).abs() < 1e-9,
        "mimo shared-credit success_rate is a flattering 1.0"
    );
    // Its one session was superseded on a reopened task → 100% rejection.
    assert_eq!(mimo.first_pass_rejected_session_count, 1);
    assert!(
        (mimo.first_pass_rejection_rate.unwrap() - 1.0).abs() < 1e-9,
        "mimo first_pass_rejection_rate should be 1.0"
    );
    // It never ran the last worker session on a completed task.
    assert_eq!(mimo.final_pass_completed_task_count, 0);
    assert!(
        (mimo.final_pass_share.unwrap() - 0.0).abs() < 1e-9,
        "mimo final_pass_share should be 0.0"
    );

    // ── gpt: actually landed both tasks ──
    let gpt = find_model(&effectiveness, "gpt");
    assert_eq!(gpt.sessions, 2);
    // Neither of gpt's sessions was superseded → no first-pass rejections.
    assert_eq!(gpt.first_pass_rejected_session_count, 0);
    assert!(
        (gpt.first_pass_rejection_rate.unwrap() - 0.0).abs() < 1e-9,
        "gpt first_pass_rejection_rate should be 0.0"
    );
    // gpt ran the last worker session on both completed tasks it touched.
    assert_eq!(gpt.shared_credit_completed_task_count, 2);
    assert_eq!(gpt.final_pass_completed_task_count, 2);
    assert!(
        (gpt.final_pass_share.unwrap() - 1.0).abs() < 1e-9,
        "gpt final_pass_share should be 1.0"
    );
}
