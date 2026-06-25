// Endpoint contract regressions for `GET /api/admin/usage`.
//
// These tests exercise the HTTP handler layer — admin gating, the frontend
// response contract (kpis / time_series / breakdowns / model_effectiveness /
// project_model_matrix / generated_at), query-parameter validation, and
// rollup behaviour that repository-only tests cannot prove.  They follow the
// same `seed_admin_session` + `test_helpers::create_test_app_with_db` pattern
// used by `tests/agents.rs`.

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

struct SessionSeed<'a> {
    project_id: &'a str,
    model_id: &'a str,
    agent_type: &'a str,
    started_at: &'a str,
    tokens_in: i64,
    tokens_out: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    cost_usd: Option<f64>,
    /// Cost basis: `'actual'`, `'projected'`, or `'unpriced'`.
    cost_basis: &'a str,
    task_id: Option<&'a str>,
}

struct TaskSeed<'a> {
    project_id: &'a str,
    status: &'a str,
    close_reason: Option<&'a str>,
    total_reopen_count: i32,
}

/// Seed a task row directly into the database for integration-level
/// contract tests. Returns the generated task id.
async fn seed_task_row(db: &djinn_db::Database, seed: TaskSeed<'_>) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let short_id = format!("task-{}", &id[..8]);
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, epic_id, title, description, design, \
          issue_type, status, priority, owner, labels, acceptance_criteria, \
          reopen_count, continuation_count, verification_failure_count, \
          total_reopen_count, \
          intervention_count, created_at, updated_at, closed_at, close_reason, \
          merge_commit_sha, memory_refs, merge_conflict_metadata, agent_type, pr_url) \
         VALUES ($1, $2, $3, NULL, 'test title', 'test desc', 'test design', \
                 'task', $4, 0, '', '[]', '[]', \
                 0, 0, 0, \
                 $5, \
                 0, '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z', NULL, $6, \
                 NULL, '[]', NULL, NULL, NULL)",
    )
    .bind(&id)
    .bind(seed.project_id)
    .bind(&short_id)
    .bind(seed.status)
    .bind(seed.total_reopen_count)
    .bind(seed.close_reason)
    .execute(db.pool())
    .await
    .expect("failed to seed task row");
    id
}

/// Seed raw session rows directly into the database for integration-level
/// contract tests that need actual query results.
async fn seed_session_row(db: &djinn_db::Database, seed: SessionSeed<'_>) {
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO sessions \
         (id, project_id, task_id, model_id, agent_type, status, \
          started_at, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens, \
          cost_usd, cost_basis) \
         VALUES ($1, $2, $3, $4, $5, 'completed', $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(&id)
    .bind(seed.project_id)
    .bind(seed.task_id)
    .bind(seed.model_id)
    .bind(seed.agent_type)
    .bind(seed.started_at)
    .bind(seed.tokens_in)
    .bind(seed.tokens_out)
    .bind(seed.cache_read_tokens)
    .bind(seed.cache_write_tokens)
    .bind(seed.cost_usd)
    .bind(seed.cost_basis)
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

/// Find a model_effectiveness row by its (renamed) `model` field.
fn model_effectiveness_row<'a>(rows: &'a [Value], model: &str) -> &'a Value {
    rows.iter()
        .find(|row| row.get("model").and_then(Value::as_str) == Some(model))
        .unwrap_or_else(|| panic!("missing model_effectiveness row for {model}: {rows:?}"))
}

/// Convenience: extract a top-level array field as a slice of Values.
fn array_field<'a>(body: &'a Value, field: &str) -> &'a Vec<Value> {
    body.get(field)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("expected array field '{field}' in {body}"))
}

// ── Admin gating ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonadmin_request_is_rejected_with_403() {
    let db = test_helpers::create_test_db();
    let nonadmin_cookie = seed_nonadmin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(&app, "preset=30d", Some(&nonadmin_cookie)).await;

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

    let (status, body) = get_usage(&app, "preset=30d", None).await;

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
async fn admin_request_returns_frontend_contract_fields() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "start=2025-01-01&end=2025-02-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);

    for field in [
        "kpis",
        "time_series",
        "breakdowns",
        "model_effectiveness",
        "project_model_matrix",
        "generated_at",
    ] {
        assert!(
            body.get(field).is_some(),
            "missing response field '{field}' in response: {body}"
        );
    }

    assert!(body.get("kpis").unwrap().is_array());
    assert!(body.get("time_series").unwrap().is_array());
    assert!(body.get("model_effectiveness").unwrap().is_array());
    assert!(body.get("project_model_matrix").unwrap().is_array());
    assert!(body.get("generated_at").unwrap().is_string());

    let breakdowns = body.get("breakdowns").unwrap();
    for field in ["by_user", "by_project", "by_proposal", "by_task"] {
        assert!(
            breakdowns.get(field).map(Value::is_array).unwrap_or(false),
            "missing breakdowns.{field}: {breakdowns}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kpis_have_label_value_and_delta_fields() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "start=2025-01-01&end=2025-02-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);
    let kpis = array_field(&body, "kpis");
    assert_eq!(kpis.len(), 5, "expected five KPI cards, got {}", kpis.len());

    let labels: Vec<&str> = kpis
        .iter()
        .filter_map(|k| k.get("label").and_then(Value::as_str))
        .collect();
    assert!(
        labels.contains(&"Actual API spend"),
        "expected an Actual API spend KPI: {labels:?}"
    );
    assert!(
        labels.contains(&"Projected subscription cost"),
        "expected a Projected subscription cost KPI: {labels:?}"
    );

    for kpi in kpis {
        assert!(kpi.get("label").unwrap().is_string());
        // value and delta_pct may be null but the keys must exist.
        assert!(kpi.get("value").is_some());
        assert!(kpi.get("delta_pct").is_some());
        assert!(kpi.get("formatted").unwrap().is_string());
    }
}

// ── Granularity and rollups ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn granularity_week_and_month_are_accepted() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    for gran in ["week", "month"] {
        let (status, body) = get_usage(
            &app,
            &format!("start=2025-01-01&end=2025-04-01&granularity={gran}"),
            Some(&admin_cookie),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "granularity={gran} should be accepted"
        );
        assert!(body.get("time_series").unwrap().is_array());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weekly_rollup_buckets_to_week_start_and_counts_sessions() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-rollup", "rollup-project").await;

    // Window 2025-03-10..2025-03-17: 2 sessions; previous week: 1 session.
    for (started, tin, tout, cost) in [
        ("2025-03-11T10:00:00Z", 100, 50, 0.50),
        ("2025-03-12T10:00:00Z", 200, 100, 1.00),
        ("2025-03-05T10:00:00Z", 300, 150, 1.50),
    ] {
        seed_session_row(
            &db,
            SessionSeed {
                project_id: "proj-rollup",
                model_id: "model-a",
                agent_type: "worker",
                started_at: started,
                tokens_in: tin,
                tokens_out: tout,
                cache_read_tokens: 10,
                cache_write_tokens: 5,
                cost_usd: Some(cost),
                cost_basis: "actual",
                task_id: None,
            },
        )
        .await;
    }

    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(
        &app,
        "start=2025-03-10&end=2025-03-17&granularity=week",
        Some(&admin_cookie),
    )
    .await;

    assert_eq!(status, StatusCode::OK);

    // Sessions KPI should reflect the 2 sessions in the main window.
    let kpis = array_field(&body, "kpis");
    let sessions_kpi = kpis
        .iter()
        .find(|k| k.get("label").and_then(Value::as_str) == Some("Sessions"))
        .expect("Sessions KPI");
    assert_eq!(sessions_kpi.get("value").unwrap().as_f64().unwrap(), 2.0);

    // Weekly series buckets to the Monday week-start (2025-03-10).
    let series = array_field(&body, "time_series");
    assert!(!series.is_empty(), "weekly series should have a point");
    for point in series {
        let date = point.get("date").unwrap().as_str().unwrap();
        assert_eq!(date, "2025-03-10", "expected week-start Monday, got {date}");
    }
}

// ── Nullable cost semantics ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn null_cost_yields_null_spend_kpi_and_series_cost() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-nullcost", "nullcost-project").await;
    seed_session_row(
        &db,
        SessionSeed {
            project_id: "proj-nullcost",
            model_id: "unpriced-model",
            agent_type: "worker",
            started_at: "2025-03-11T10:00:00Z",
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            cost_usd: None,
            cost_basis: "unpriced",
            task_id: None,
        },
    )
    .await;

    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "start=2025-03-01&end=2025-04-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);

    let kpis = array_field(&body, "kpis");
    let spend = kpis
        .iter()
        .find(|k| k.get("label").and_then(Value::as_str) == Some("Actual API spend"))
        .expect("Actual API spend KPI");
    assert!(
        spend.get("value").unwrap().is_null(),
        "expected null Actual API spend value for unpriced sessions, got: {}",
        spend.get("value").unwrap()
    );

    let series = array_field(&body, "time_series");
    assert!(!series.is_empty());
    // Both split fields are zero (no actual or projected sessions),
    // but the deprecated cost key must not appear.
    let point = &series[0];
    assert_eq!(
        point.get("actual_spend_usd").unwrap().as_f64().unwrap(),
        0.0
    );
    assert_eq!(point.get("projected_usd").unwrap().as_f64().unwrap(), 0.0);
    assert_eq!(
        point
            .get("unpriced_session_count")
            .unwrap()
            .as_i64()
            .unwrap(),
        1
    );
    assert!(
        point.get("cost").is_none(),
        "deprecated cost key should not appear for unpriced bucket"
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
        "start=2025-01-01&end=2025-02-01&granularity=hourly",
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
async fn invalid_preset_returns_400() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(&app, "preset=90d", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let msg = body.get("_raw").and_then(|v| v.as_str()).unwrap_or("");
    assert!(msg.contains("preset") || msg.contains("90d"), "got: {msg}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reversed_date_range_returns_400() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "start=2025-03-01&end=2025-03-01", Some(&admin_cookie)).await;

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

    let (status, _) = get_usage(&app, "start=not-a-date&end=2025-02-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ── Default parameters ───────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn omitted_query_params_apply_defaults() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;
    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) = get_usage(&app, "", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.get("kpis").unwrap().is_array());
    assert!(body.get("generated_at").unwrap().is_string());
}

// ── Model effectiveness field coverage ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_effectiveness_uses_frontend_field_names() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-eff", "effectiveness-project").await;
    let task_id = seed_task_row(
        &db,
        TaskSeed {
            project_id: "proj-eff",
            status: "closed",
            close_reason: Some("completed"),
            total_reopen_count: 0,
        },
    )
    .await;
    seed_session_row(
        &db,
        SessionSeed {
            project_id: "proj-eff",
            model_id: "test-model",
            agent_type: "worker",
            started_at: "2025-03-11T10:00:00Z",
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            cost_usd: Some(1.00),
            cost_basis: "actual",
            task_id: Some(&task_id),
        },
    )
    .await;

    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "start=2025-03-01&end=2025-04-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);

    let me = array_field(&body, "model_effectiveness");
    assert!(!me.is_empty(), "model_effectiveness should have a row");

    let row = model_effectiveness_row(me, "test-model");
    for field in [
        "model",
        "task_count",
        "success_rate",
        "avg_reopens",
        "cost_per_task",
        "total_cost",
        "total_tokens",
        "session_count",
        "completed_task_count",
        "actual_spend_usd",
        "projected_usd",
        "unpriced_session_count",
    ] {
        assert!(
            row.get(field).is_some(),
            "missing model_effectiveness field '{field}' in {row}"
        );
    }
    assert!(row.get("model").unwrap().is_string());
    assert!(row.get("session_count").unwrap().is_number());
    assert_eq!(row.get("total_tokens").unwrap().as_i64().unwrap(), 150);
    assert!((row.get("actual_spend_usd").unwrap().as_f64().unwrap() - 1.0).abs() < 1e-9);
    assert_eq!(row.get("projected_usd").unwrap().as_f64().unwrap(), 0.0);
    assert_eq!(
        row.get("unpriced_session_count").unwrap().as_i64().unwrap(),
        0
    );
    // Repository-only names must not leak into the API.
    assert!(row.get("model_id").is_none());
    assert!(
        row.get("spend_usd").is_none(),
        "repo-level spend_usd must not leak"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_effectiveness_null_cost_serializes_total_cost_null() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-null-eff", "null-eff-project").await;
    let task_id = seed_task_row(
        &db,
        TaskSeed {
            project_id: "proj-null-eff",
            status: "closed",
            close_reason: Some("completed"),
            total_reopen_count: 0,
        },
    )
    .await;
    seed_session_row(
        &db,
        SessionSeed {
            project_id: "proj-null-eff",
            model_id: "unpriced-model",
            agent_type: "worker",
            started_at: "2025-03-11T10:00:00Z",
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            cost_usd: None,
            cost_basis: "unpriced",
            task_id: Some(&task_id),
        },
    )
    .await;

    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "start=2025-03-01&end=2025-04-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);

    let me = array_field(&body, "model_effectiveness");
    let row = model_effectiveness_row(me, "unpriced-model");
    assert!(
        row.get("total_cost").unwrap().is_null(),
        "expected total_cost null for unpriced model, got: {}",
        row.get("total_cost").unwrap()
    );
    assert!(
        row.get("actual_spend_usd").unwrap().is_null(),
        "expected actual_spend_usd null for unpriced model"
    );
    assert!(
        row.get("projected_usd").unwrap().is_null(),
        "expected projected_usd null for unpriced model"
    );
    assert_eq!(
        row.get("unpriced_session_count").unwrap().as_i64().unwrap(),
        1
    );
}

// ── Entity breakdowns + matrix ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breakdowns_and_matrix_populate_from_seeded_session() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-bd", "breakdown-project").await;
    let task_id = seed_task_row(
        &db,
        TaskSeed {
            project_id: "proj-bd",
            status: "closed",
            close_reason: Some("completed"),
            total_reopen_count: 1,
        },
    )
    .await;
    seed_session_row(
        &db,
        SessionSeed {
            project_id: "proj-bd",
            model_id: "model-bd",
            agent_type: "worker",
            started_at: "2025-03-11T10:00:00Z",
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            cost_usd: Some(4.00),
            cost_basis: "actual",
            task_id: Some(&task_id),
        },
    )
    .await;

    let app = test_helpers::create_test_app_with_db(db);

    let (status, body) =
        get_usage(&app, "start=2025-03-01&end=2025-04-01", Some(&admin_cookie)).await;

    assert_eq!(status, StatusCode::OK);

    // by_project: a row keyed by the project id with the human-readable name.
    let by_project = array_field(body.get("breakdowns").unwrap(), "by_project");
    let proj_row = by_project
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some("proj-bd"))
        .expect("by_project row for proj-bd");
    assert_eq!(
        proj_row.get("name").unwrap().as_str().unwrap(),
        "breakdown-project"
    );
    assert!((proj_row.get("actual_spend_usd").unwrap().as_f64().unwrap() - 4.0).abs() < 1e-9);
    assert_eq!(
        proj_row.get("projected_usd").unwrap().as_f64().unwrap(),
        0.0
    );
    assert_eq!(
        proj_row
            .get("unpriced_session_count")
            .unwrap()
            .as_i64()
            .unwrap(),
        0
    );

    // by_task: a row keyed by the task id, carrying a task_id link field and a
    // derived cost_per_task.
    let by_task = array_field(body.get("breakdowns").unwrap(), "by_task");
    let task_row = by_task
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(task_id.as_str()))
        .expect("by_task row for the seeded task");
    assert_eq!(
        task_row.get("task_id").unwrap().as_str().unwrap(),
        task_id.as_str()
    );
    assert!((task_row.get("cost_per_task").unwrap().as_f64().unwrap() - 4.0).abs() < 1e-9);
    assert!((task_row.get("actual_spend_usd").unwrap().as_f64().unwrap() - 4.0).abs() < 1e-9);

    // project_model_matrix: a cell for (proj-bd, model-bd) with frontend names.
    let matrix = array_field(&body, "project_model_matrix");
    let cell = matrix
        .iter()
        .find(|c| {
            c.get("project_id").and_then(Value::as_str) == Some("proj-bd")
                && c.get("model").and_then(Value::as_str) == Some("model-bd")
        })
        .expect("matrix cell for proj-bd/model-bd");
    assert!((cell.get("actual_spend_usd").unwrap().as_f64().unwrap() - 4.0).abs() < 1e-9);
    assert_eq!(cell.get("projected_usd").unwrap().as_f64().unwrap(), 0.0);
    assert_eq!(
        cell.get("unpriced_session_count")
            .unwrap()
            .as_i64()
            .unwrap(),
        0
    );
    assert_eq!(cell.get("total_tokens").unwrap().as_i64().unwrap(), 150);
}
