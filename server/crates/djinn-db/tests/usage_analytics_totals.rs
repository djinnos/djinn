//! Integration test: verify `UsageAnalyticsRepository::query()` returns correct
//! `UsageTotals` against a live Postgres database.
//!
//! Guards against the NUMERIC→i64 decode panic that occurred because
//! `SUM(bigint)` returns `NUMERIC` in Postgres.  The fix casts each integer
//! aggregate to `::bigint` so `row.get::<i64>(…)` succeeds.

use djinn_db::repositories::usage_analytics::GroupDimension;
use djinn_db::{Database, UsageAnalyticsQuery, UsageAnalyticsRepository};

/// Helper: create an in-memory test database (cloned from djinn_test_template).
fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
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
