// Endpoint contract regressions for `GET /api/admin/usage`.
//
// These tests exercise the HTTP handler layer — admin gating, response shape,
// query-parameter validation, and rollup/pagination behaviour that repository-
// only tests cannot prove.  They follow the same `seed_admin_session` +
// `test_helpers::create_test_app_with_db` pattern used by `tests/agents.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::test_helpers;
use djinn_db::{CreateUserAuthSession, SessionAuthRepository, UserRepository};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn rand_github_id() -> i64 {
    let bytes = *uuid::Uuid::now_v7().as_bytes();
    i64::from_be_bytes(bytes[8..16].try_into().unwrap()).unsigned_abs() as i64
}

/// Create an admin user + session in the database and return the session token
/// suitable for a `djinn_session` cookie.
async fn seed_admin_session(db: &djinn_db::Database) -> String {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(rand_github_id(), "usage-admin", None, None)
        .await
        .unwrap();
    UserRepository::new(db.clone())
        .set_admin_status(&user.id, true)
        .await
        .unwrap();

    let token = format!("usage-sess-{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user.id,
            github_login: "usage-admin",
            github_name: None,
            github_avatar_url: None,
            github_access_token: "gho_usage_test",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
    token
}

/// Create a non-admin user + session and return the session token.
async fn seed_nonadmin_session(db: &djinn_db::Database) -> String {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(rand_github_id(), "usage-regular", None, None)
        .await
        .unwrap();

    let token = format!("usage-nongmin-{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user.id,
            github_login: "usage-regular",
            github_name: None,
            github_avatar_url: None,
            github_access_token: "gho_usage_nonadmin",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
    token
}

/// Issue a GET `/api/admin/usage` request with the given query string and
/// optional `djinn_session` cookie.
async fn get_usage(app: &axum::Router, query: &str, cookie: Option<&str>) -> (StatusCode, Value) {
    let uri = if query.is_empty() {
        "/api/admin/usage".to_string()
    } else {
        format!("/api/admin/usage?{query}")
    };

    let mut builder = Request::builder().method("GET").uri(&uri);
    if let Some(token) = cookie {
        builder = builder.header("cookie", format!("djinn_session={token}"));
    }

    let request = builder.body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| serde_json::json!({ "_raw": String::from_utf8_lossy(&body) }));

    (status, json)
}

/// Seed raw session rows directly into the database for integration-level
/// contract tests that need actual query results.
///
/// Inserts sessions with the given `started_at` prefix (first 10 chars must be
/// a valid ISO date), `model_id`, `agent_type`, `project_id`, and optional
/// `cost_usd`.
async fn seed_session_row(
    db: &djinn_db::Database,
    project_id: &str,
    model_id: &str,
    agent_type: &str,
    started_at: &str,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: Option<f64>,
) {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, task_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, cost_usd) \
         VALUES ($1, $2, NULL, $3, $4, 'completed', $5, $6, $7, $8, $9, $10)",
    )
    .bind(&id)
    .bind(project_id)
    .bind(model_id)
    .bind(agent_type)
    .bind(started_at)
    .bind(tokens_in)
    .bind(tokens_out)
    .bind(cache_read_tokens)
    .bind(cache_write_tokens)
    .bind(cost_usd)
    .execute(db.pool())
    .await
    .expect("failed to seed session row");
}

/// Seed a project so that `project_id` FK constraints pass.
async fn seed_project(db: &djinn_db::Database, project_id: &str, name: &str) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, $2, 'test', $2) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(project_id)
    .bind(name)
    .execute(db.pool())
    .await
    .expect("failed to seed project");
}

// ── Admin gating ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonadmin_request_is_rejected_with_403() {
    let db = test_helpers::create_test_db();
    let nonadmin_cookie = seed_nonadmin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(
        &app,
        "from=2025-01-01&to=2025-02-01",
        Some(&nonadmin_cookie),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        body.get("_raw")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("admin"))
            .unwrap_or(false),
        "expected admin-related error message, got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_request_is_rejected_with_401() {
    let db = test_helpers::create_test_db();
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(&app, "from=2025-01-01&to=2025-02-01", None).await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        body.get("_raw")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("authentication"))
            .unwrap_or(false),
        "expected authentication-related error message, got: {body}"
    );
}

// ── Response shape ───────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_request_returns_all_six_top_level_fields() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "from=2025-01-01&to=2025-02-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);

    for field in [
        "totals",
        "previous_totals",
        "series",
        "breakdown",
        "model_effectiveness",
        "project_model_matrix",
        "granularity",
    ] {
        assert!(
            body.get(field).is_some(),
            "missing response field '{field}' in response: {body}"
        );
    }

    // Verify types
    assert!(body.get("totals").unwrap().is_object());
    assert!(body.get("previous_totals").unwrap().is_object());
    assert!(body.get("series").unwrap().is_array());
    assert!(body.get("breakdown").unwrap().is_array());
    assert!(body.get("model_effectiveness").unwrap().is_array());
    assert!(body.get("project_model_matrix").unwrap().is_array());
    assert_eq!(body.get("granularity").unwrap().as_str().unwrap(), "day");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn totals_and_previous_totals_contain_expected_scalar_fields() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "from=2025-01-01&to=2025-02-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);

    for key in ["totals", "previous_totals"] {
        let obj = body.get(key).unwrap();
        for field in [
            "session_count",
            "tokens_in",
            "tokens_out",
            "cache_read_tokens",
            "cache_write_tokens",
            "total_cost_usd",
        ] {
            assert!(
                obj.get(field).is_some(),
                "missing '{field}' in {key}: {obj}"
            );
        }
    }
}

// ── Granularity and rollups ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn granularity_week_is_accepted_and_reflected_in_response() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(
        &app,
        "from=2025-01-01&to=2025-02-01&granularity=week",
        Some(&admin_cookie),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("granularity").unwrap().as_str().unwrap(), "week");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn granularity_month_is_accepted_and_reflected_in_response() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(
        &app,
        "from=2025-01-01&to=2025-04-01&granularity=month",
        Some(&admin_cookie),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("granularity").unwrap().as_str().unwrap(), "month");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weekly_rollup_and_previous_window_with_seeded_data() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    // Seed a project so FK constraints pass.
    seed_project(&db, "proj-rollup", "rollup-project").await;

    // Seed sessions across two weeks:
    //   Window (2025-03-10..2025-03-17): 2 sessions
    //   Previous window (2025-03-03..2025-03-10): 1 session
    seed_session_row(
        &db,
        "proj-rollup",
        "model-a",
        "worker",
        "2025-03-11T10:00:00Z",
        100,
        50,
        10,
        5,
        Some(0.50),
    )
    .await;
    seed_session_row(
        &db,
        "proj-rollup",
        "model-a",
        "worker",
        "2025-03-12T10:00:00Z",
        200,
        100,
        20,
        10,
        Some(1.00),
    )
    .await;
    seed_session_row(
        &db,
        "proj-rollup",
        "model-a",
        "worker",
        "2025-03-05T10:00:00Z",
        300,
        150,
        30,
        15,
        Some(1.50),
    )
    .await;

    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(
        &app,
        "from=2025-03-10&to=2025-03-17&granularity=week",
        Some(&admin_cookie),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("granularity").unwrap().as_str().unwrap(), "week");

    // The main window totals should reflect the 2 sessions.
    let totals = body.get("totals").unwrap();
    assert_eq!(totals.get("session_count").unwrap().as_i64().unwrap(), 2);
    assert_eq!(totals.get("tokens_in").unwrap().as_i64().unwrap(), 300);
    assert_eq!(totals.get("tokens_out").unwrap().as_i64().unwrap(), 150);

    // Previous totals should reflect the 1 session in the prior week.
    let prev = body.get("previous_totals").unwrap();
    assert_eq!(prev.get("session_count").unwrap().as_i64().unwrap(), 1);
    assert_eq!(prev.get("tokens_in").unwrap().as_i64().unwrap(), 300);

    // Weekly series should have at most one rolled-up bucket (Mon 2025-03-10).
    let series = body.get("series").unwrap().as_array().unwrap();
    assert!(
        !series.is_empty(),
        "weekly series should have at least one point"
    );
    // All points should be week-start dates (Monday).
    for point in series {
        let day = point.get("day").unwrap().as_str().unwrap();
        assert!(
            day.starts_with("2025-03-"),
            "expected March 2025 week-start, got {day}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monthly_rollup_with_seeded_data() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-monthly", "monthly-project").await;

    // Seed sessions across two months.
    seed_session_row(
        &db,
        "proj-monthly",
        "model-a",
        "worker",
        "2025-01-15T10:00:00Z",
        100,
        50,
        10,
        5,
        Some(0.50),
    )
    .await;
    seed_session_row(
        &db,
        "proj-monthly",
        "model-a",
        "worker",
        "2025-01-20T10:00:00Z",
        200,
        100,
        20,
        10,
        Some(1.00),
    )
    .await;
    seed_session_row(
        &db,
        "proj-monthly",
        "model-b",
        "worker",
        "2025-02-10T10:00:00Z",
        300,
        150,
        30,
        15,
        Some(1.50),
    )
    .await;

    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(
        &app,
        "from=2025-01-01&to=2025-03-01&granularity=month",
        Some(&admin_cookie),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("granularity").unwrap().as_str().unwrap(), "month");

    // Totals should cover all 3 sessions.
    let totals = body.get("totals").unwrap();
    assert_eq!(totals.get("session_count").unwrap().as_i64().unwrap(), 3);

    // Monthly series should have at most 2 buckets (Jan and Feb).
    let series = body.get("series").unwrap().as_array().unwrap();
    assert!(
        series.len() <= 2,
        "expected at most 2 monthly buckets, got {}",
        series.len()
    );
}

// ── Nullable cost semantics ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn null_cost_fields_serialize_as_json_null() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-nullcost", "nullcost-project").await;

    // Seed a session with NULL cost_usd.
    seed_session_row(
        &db,
        "proj-nullcost",
        "unpriced-model",
        "worker",
        "2025-03-11T10:00:00Z",
        100,
        50,
        10,
        5,
        None, // unpriced
    )
    .await;

    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "from=2025-03-01&to=2025-04-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);

    // total_cost_usd must be JSON null (not 0.0) when sessions are unpriced.
    let totals = body.get("totals").unwrap();
    assert!(
        totals.get("total_cost_usd").unwrap().is_null(),
        "expected null total_cost_usd for unpriced sessions, got: {}",
        totals.get("total_cost_usd").unwrap()
    );

    // previous_totals should also have null cost (no sessions in previous window).
    let prev = body.get("previous_totals").unwrap();
    assert!(
        prev.get("total_cost_usd").unwrap().is_null(),
        "expected null total_cost_usd in previous_totals, got: {}",
        prev.get("total_cost_usd").unwrap()
    );
}

// ── Bad query handling ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_granularity_returns_400() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(
        &app,
        "from=2025-01-01&to=2025-02-01&granularity=hourly",
        Some(&admin_cookie),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.get("_raw").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("granularity") || msg.contains("hourly"),
        "expected granularity error, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_group_by_returns_400() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(
        &app,
        "from=2025-01-01&to=2025-02-01&group_by=invalid_dimension",
        Some(&admin_cookie),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.get("_raw").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("group_by") || msg.contains("invalid_dimension"),
        "expected group_by error, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reversed_date_range_returns_400() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    // from >= to
    let (status, body) =
        get_usage(&app, "from=2025-03-01&to=2025-03-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.get("_raw").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("from must be before to"),
        "expected date range error, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_date_format_returns_400() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "from=not-a-date&to=2025-02-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.get("_raw").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        msg.contains("from") || msg.contains("YYYY-MM-DD"),
        "expected date format error, got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_month_day_returns_400() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, _) = get_usage(&app, "from=2025-02-30&to=2025-03-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ── Default parameters ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn omitted_query_params_apply_defaults() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    // No query params at all — should default to day granularity and succeed.
    let (status, body) = get_usage(&app, "", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("granularity").unwrap().as_str().unwrap(), "day");
    assert!(body.get("totals").unwrap().is_object());
}

// ── Breakdown group_by dimension ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_group_by_dimensions_are_accepted() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    for dim in ["model", "project", "user", "proposal", "task", "agent"] {
        let (status, _) = get_usage(
            &app,
            &format!("from=2025-01-01&to=2025-02-01&group_by={dim}"),
            Some(&admin_cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "group_by={dim} should be accepted");
    }
}
