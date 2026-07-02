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
use djinn_db::test_support::{
    UsageTestSessionSeed, UsageTestTaskSeed, seed_project, seed_session_row, seed_task_row,
};
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
async fn split_kpi_reducer_invariant_and_seeded_totals() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-kpi", "kpi-project").await;
    let task_id = seed_task_row(
        &db,
        UsageTestTaskSeed {
            project_id: "proj-kpi",
            status: "closed",
            close_reason: Some("completed"),
            total_reopen_count: 0,
        },
    )
    .await;

    // actual-basis session: $1.25
    seed_session_row(
        &db,
        UsageTestSessionSeed {
            project_id: "proj-kpi",
            model_id: "model-kpi",
            agent_type: "worker",
            started_at: "2025-03-11T10:00:00Z",
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            cost_usd: Some(1.25),
            cost_basis: "actual",
            task_id: Some(&task_id),
        },
    )
    .await;

    // projected-basis session: $2.50
    seed_session_row(
        &db,
        UsageTestSessionSeed {
            project_id: "proj-kpi",
            model_id: "model-kpi",
            agent_type: "worker",
            started_at: "2025-03-12T10:00:00Z",
            tokens_in: 200,
            tokens_out: 100,
            cache_read_tokens: 20,
            cache_write_tokens: 10,
            cost_usd: Some(2.50),
            cost_basis: "projected",
            task_id: Some(&task_id),
        },
    )
    .await;

    // unpriced-basis session: NULL cost
    seed_session_row(
        &db,
        UsageTestSessionSeed {
            project_id: "proj-kpi",
            model_id: "model-kpi",
            agent_type: "worker",
            started_at: "2025-03-13T10:00:00Z",
            tokens_in: 50,
            tokens_out: 25,
            cache_read_tokens: 5,
            cache_write_tokens: 2,
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

    let kpis = array_field(&body, "kpis");
    assert_eq!(kpis.len(), 6, "expected six KPI cards, got {}", kpis.len());

    // ── Generic shape coverage (label, value, delta_pct, formatted) ──
    let labels: Vec<&str> = kpis
        .iter()
        .filter_map(|k| k.get("label").and_then(Value::as_str))
        .collect();
    assert!(
        labels.contains(&"Total cost (list-price)"),
        "expected a Total cost (list-price) KPI: {labels:?}"
    );
    assert!(
        labels.contains(&"Actual API Spend"),
        "expected an Actual API Spend KPI: {labels:?}"
    );
    assert!(
        labels.contains(&"Projected Cost"),
        "expected a Projected Cost KPI: {labels:?}"
    );

    for kpi in kpis {
        assert!(kpi.get("label").unwrap().is_string());
        assert!(kpi.get("value").is_some());
        assert!(kpi.get("delta_pct").is_some());
        assert!(kpi.get("formatted").unwrap().is_string());
    }

    // ── Tokens breakdown must remain intact ──
    let tokens_kpi = kpis
        .iter()
        .find(|k| k.get("label").and_then(Value::as_str) == Some("Tokens"))
        .expect("Tokens KPI");
    let breakdown = tokens_kpi
        .get("breakdown")
        .and_then(Value::as_array)
        .expect("Tokens KPI must have a breakdown array");
    assert!(
        breakdown
            .iter()
            .any(|b| b.get("label").and_then(Value::as_str) == Some("Input")),
        "Tokens breakdown missing Input"
    );
    assert!(
        breakdown
            .iter()
            .any(|b| b.get("label").and_then(Value::as_str) == Some("Cached")),
        "Tokens breakdown missing Cached"
    );
    assert!(
        breakdown
            .iter()
            .any(|b| b.get("label").and_then(Value::as_str) == Some("Output")),
        "Tokens breakdown missing Output"
    );

    // ── Reducer: sum split fields across KPIs the same way the frontend does ──
    let sum_actual: f64 = kpis
        .iter()
        .filter_map(|k| k.get("actual_spend_usd").and_then(Value::as_f64))
        .sum();
    let sum_projected: f64 = kpis
        .iter()
        .filter_map(|k| k.get("projected_usd").and_then(Value::as_f64))
        .sum();
    let sum_unpriced: i64 = kpis
        .iter()
        .filter_map(|k| k.get("unpriced_count").and_then(Value::as_i64))
        .sum();

    assert_eq!(
        sum_actual, 1.25,
        "reduced actual_spend_usd should equal seeded total"
    );
    assert_eq!(
        sum_projected, 2.50,
        "reduced projected_usd should equal seeded total"
    );
    assert_eq!(
        sum_unpriced, 1,
        "reduced unpriced_count should equal seeded total"
    );

    // Top-level unpriced_session_count must repeat the unpriced count.
    assert_eq!(
        body.get("unpriced_session_count")
            .and_then(Value::as_i64)
            .unwrap_or(-1),
        1,
        "top-level unpriced_session_count should repeat unpriced count"
    );

    // ── Single-contributor invariant ──
    let actual_contributors: Vec<&Value> = kpis
        .iter()
        .filter(|k| {
            k.get("actual_spend_usd")
                .map(|v| !v.is_null())
                .unwrap_or(false)
        })
        .collect();
    let projected_contributors: Vec<&Value> = kpis
        .iter()
        .filter(|k| {
            k.get("projected_usd")
                .map(|v| !v.is_null())
                .unwrap_or(false)
        })
        .collect();
    let unpriced_contributors: Vec<&Value> = kpis
        .iter()
        .filter(|k| {
            k.get("unpriced_count")
                .map(|v| !v.is_null())
                .unwrap_or(false)
        })
        .collect();
    let list_price_contributors: Vec<&Value> = kpis
        .iter()
        .filter(|k| {
            k.get("list_price_usd")
                .map(|v| !v.is_null())
                .unwrap_or(false)
        })
        .collect();

    assert_eq!(
        actual_contributors.len(),
        1,
        "exactly one KPI must carry actual_spend_usd"
    );
    assert_eq!(
        projected_contributors.len(),
        1,
        "exactly one KPI must carry projected_usd"
    );
    assert_eq!(
        unpriced_contributors.len(),
        1,
        "exactly one KPI must carry unpriced_count"
    );
    assert_eq!(
        list_price_contributors.len(),
        1,
        "exactly one KPI must carry list_price_usd"
    );

    // ── Non-contributor cards must not duplicate aggregate fields ──
    for kpi in kpis {
        let label = kpi.get("label").and_then(Value::as_str).unwrap_or("");
        match label {
            "Total cost (list-price)" => {
                assert!(
                    kpi.get("list_price_usd").is_some(),
                    "Total cost (list-price) must carry list_price_usd"
                );
                assert!(
                    kpi.get("actual_spend_usd").is_none(),
                    "Total cost (list-price) must not carry actual_spend_usd"
                );
                assert!(
                    kpi.get("projected_usd").is_none(),
                    "Total cost (list-price) must not carry projected_usd"
                );
                assert!(
                    kpi.get("unpriced_count").is_none(),
                    "Total cost (list-price) must not carry unpriced_count"
                );
            }
            "Actual API Spend" => {
                assert!(
                    kpi.get("actual_spend_usd").is_some(),
                    "Actual API Spend must carry actual_spend_usd"
                );
                assert!(
                    kpi.get("projected_usd").is_none(),
                    "Actual API Spend must not carry projected_usd"
                );
                assert!(
                    kpi.get("unpriced_count").is_none(),
                    "Actual API Spend must not carry unpriced_count"
                );
            }
            "Projected Cost" => {
                assert!(
                    kpi.get("projected_usd").is_some(),
                    "Projected Cost must carry projected_usd"
                );
                assert!(
                    kpi.get("actual_spend_usd").is_none(),
                    "Projected Cost must not carry actual_spend_usd"
                );
                assert!(
                    kpi.get("unpriced_count").is_none(),
                    "Projected Cost must not carry unpriced_count"
                );
            }
            "Sessions" => {
                assert!(
                    kpi.get("unpriced_count").is_some(),
                    "Sessions must carry unpriced_count"
                );
                assert!(
                    kpi.get("actual_spend_usd").is_none(),
                    "Sessions must not carry actual_spend_usd"
                );
                assert!(
                    kpi.get("projected_usd").is_none(),
                    "Sessions must not carry projected_usd"
                );
            }
            "Tokens" | "Cache reads" => {
                assert!(
                    kpi.get("actual_spend_usd").is_none(),
                    "{label} must not carry actual_spend_usd"
                );
                assert!(
                    kpi.get("projected_usd").is_none(),
                    "{label} must not carry projected_usd"
                );
                assert!(
                    kpi.get("unpriced_count").is_none(),
                    "{label} must not carry unpriced_count"
                );
            }
            _ => panic!("unexpected KPI label: {label}"),
        }
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
            UsageTestSessionSeed {
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
        UsageTestSessionSeed {
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
    let actual_kpi = kpis
        .iter()
        .find(|k| k.get("label").and_then(Value::as_str) == Some("Actual API Spend"))
        .expect("Actual API Spend KPI");
    assert!(
        actual_kpi.get("value").unwrap().is_null(),
        "expected null Actual API Spend value for unpriced sessions, got: {}",
        actual_kpi.get("value").unwrap()
    );

    let series = array_field(&body, "time_series");
    assert!(!series.is_empty());
    assert!(
        series[0].get("actual_spend_usd").unwrap().is_null(),
        "expected null series actual_spend_usd for unpriced bucket, got: {}",
        series[0].get("actual_spend_usd").unwrap()
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
        UsageTestTaskSeed {
            project_id: "proj-eff",
            status: "closed",
            close_reason: Some("completed"),
            total_reopen_count: 0,
        },
    )
    .await;
    seed_session_row(
        &db,
        UsageTestSessionSeed {
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
        "actual_cost_per_task",
        "list_price_cost_per_task",
        "actual_spend_usd",
        "projected_usd",
        "list_price_usd",
        "unpriced_session_count",
        "total_tokens",
        "session_count",
        "completed_task_count",
    ] {
        assert!(
            row.get(field).is_some(),
            "missing model_effectiveness field '{field}' in {row}"
        );
    }
    assert!(row.get("model").unwrap().is_string());
    assert!(row.get("session_count").unwrap().is_number());
    assert_eq!(row.get("total_tokens").unwrap().as_i64().unwrap(), 150);
    // Repository-only names must not leak into the API.
    assert!(row.get("model_id").is_none());
    assert!(row.get("spend_usd").is_none());
    assert!(row.get("total_cost").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_effectiveness_null_cost_serializes_total_cost_null() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-null-eff", "null-eff-project").await;
    let task_id = seed_task_row(
        &db,
        UsageTestTaskSeed {
            project_id: "proj-null-eff",
            status: "closed",
            close_reason: Some("completed"),
            total_reopen_count: 0,
        },
    )
    .await;
    seed_session_row(
        &db,
        UsageTestSessionSeed {
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
        row.get("actual_spend_usd").unwrap().is_null(),
        "expected actual_spend_usd null for unpriced model, got: {}",
        row.get("actual_spend_usd").unwrap()
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
        UsageTestTaskSeed {
            project_id: "proj-bd",
            status: "closed",
            close_reason: Some("completed"),
            total_reopen_count: 1,
        },
    )
    .await;
    seed_session_row(
        &db,
        UsageTestSessionSeed {
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

    // by_task: a row keyed by the task id, carrying a task_id link field and a
    // derived actual_cost_per_task.
    let by_task = array_field(body.get("breakdowns").unwrap(), "by_task");
    let task_row = by_task
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some(task_id.as_str()))
        .expect("by_task row for the seeded task");
    assert_eq!(
        task_row.get("task_id").unwrap().as_str().unwrap(),
        task_id.as_str()
    );
    assert!(
        (task_row
            .get("actual_cost_per_task")
            .unwrap()
            .as_f64()
            .unwrap()
            - 4.0)
            .abs()
            < 1e-9
    );

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
    assert_eq!(cell.get("total_tokens").unwrap().as_i64().unwrap(), 150);
}

// ── Split cost-basis contract: actual + projected + unpriced ────────────────

/// Verify that `/api/admin/usage` returns separate `actual_spend_usd` and
/// `projected_usd` KPIs, and that unpriced sessions are excluded from both
/// dollar figures but counted visibly in series, breakdowns, and matrix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_cost_basis_contract_at_api_level() {
    let db = test_helpers::create_test_db();
    let admin_cookie = seed_admin_session(&db).await;

    seed_project(&db, "proj-split", "split-project").await;
    let task_id = seed_task_row(
        &db,
        UsageTestTaskSeed {
            project_id: "proj-split",
            status: "closed",
            close_reason: Some("completed"),
            total_reopen_count: 0,
        },
    )
    .await;

    // actual-basis session: $2.00
    seed_session_row(
        &db,
        UsageTestSessionSeed {
            project_id: "proj-split",
            model_id: "model-split",
            agent_type: "worker",
            started_at: "2025-03-11T10:00:00Z",
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            cost_usd: Some(2.00),
            cost_basis: "actual",
            task_id: Some(&task_id),
        },
    )
    .await;

    // projected-basis session: $3.50
    seed_session_row(
        &db,
        UsageTestSessionSeed {
            project_id: "proj-split",
            model_id: "model-split",
            agent_type: "worker",
            started_at: "2025-03-12T10:00:00Z",
            tokens_in: 200,
            tokens_out: 100,
            cache_read_tokens: 20,
            cache_write_tokens: 10,
            cost_usd: Some(3.50),
            cost_basis: "projected",
            task_id: Some(&task_id),
        },
    )
    .await;

    // unpriced-basis session: NULL cost
    seed_session_row(
        &db,
        UsageTestSessionSeed {
            project_id: "proj-split",
            model_id: "model-split",
            agent_type: "worker",
            started_at: "2025-03-13T10:00:00Z",
            tokens_in: 50,
            tokens_out: 25,
            cache_read_tokens: 5,
            cache_write_tokens: 2,
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

    // ── KPIs: separate Actual API Spend and Projected Cost cards ──
    let kpis = array_field(&body, "kpis");
    let actual_kpi = kpis
        .iter()
        .find(|k| k.get("label").and_then(Value::as_str) == Some("Actual API Spend"))
        .expect("Actual API Spend KPI");
    assert!(
        (actual_kpi.get("value").unwrap().as_f64().unwrap() - 2.0).abs() < 1e-9,
        "actual KPI should be $2.00, got: {}",
        actual_kpi.get("value").unwrap()
    );

    let projected_kpi = kpis
        .iter()
        .find(|k| k.get("label").and_then(Value::as_str) == Some("Projected Cost"))
        .expect("Projected Cost KPI");
    assert!(
        (projected_kpi.get("value").unwrap().as_f64().unwrap() - 3.5).abs() < 1e-9,
        "projected KPI should be $3.50, got: {}",
        projected_kpi.get("value").unwrap()
    );

    // ── Sessions KPI: all 3 sessions counted ──
    let sessions_kpi = kpis
        .iter()
        .find(|k| k.get("label").and_then(Value::as_str) == Some("Sessions"))
        .expect("Sessions KPI");
    assert_eq!(sessions_kpi.get("value").unwrap().as_f64().unwrap(), 3.0);

    // ── Model effectiveness: split costs for model-split ──
    let me = array_field(&body, "model_effectiveness");
    let row = model_effectiveness_row(me, "model-split");
    assert!(
        (row.get("actual_spend_usd").unwrap().as_f64().unwrap() - 2.0).abs() < 1e-9,
        "model actual_spend_usd should be $2.00"
    );
    assert!(
        (row.get("projected_usd").unwrap().as_f64().unwrap() - 3.5).abs() < 1e-9,
        "model projected_usd should be $3.50"
    );
    assert_eq!(
        row.get("unpriced_session_count").unwrap().as_i64().unwrap(),
        1,
        "one unpriced session"
    );

    // ── Project model matrix: split costs per cell ──
    let matrix = array_field(&body, "project_model_matrix");
    let cell = matrix
        .iter()
        .find(|c| {
            c.get("project_id").and_then(Value::as_str) == Some("proj-split")
                && c.get("model").and_then(Value::as_str) == Some("model-split")
        })
        .expect("matrix cell for proj-split/model-split");
    assert!(
        (cell.get("actual_spend_usd").unwrap().as_f64().unwrap() - 2.0).abs() < 1e-9,
        "matrix cell actual_spend_usd should be $2.00"
    );
    assert!(
        (cell.get("projected_usd").unwrap().as_f64().unwrap() - 3.5).abs() < 1e-9,
        "matrix cell projected_usd should be $3.50"
    );
    assert_eq!(
        cell.get("unpriced_session_count")
            .unwrap()
            .as_i64()
            .unwrap(),
        1,
        "matrix cell unpriced"
    );

    // ── Breakdown (by_project): split costs per entity ──
    let by_project = array_field(body.get("breakdowns").unwrap(), "by_project");
    let proj_row = by_project
        .iter()
        .find(|r| r.get("id").and_then(Value::as_str) == Some("proj-split"))
        .expect("by_project row for proj-split");
    assert!(
        (proj_row.get("actual_spend_usd").unwrap().as_f64().unwrap() - 2.0).abs() < 1e-9,
        "breakdown actual should be $2.00"
    );
    assert!(
        (proj_row.get("projected_usd").unwrap().as_f64().unwrap() - 3.5).abs() < 1e-9,
        "breakdown projected should be $3.50"
    );
    assert_eq!(
        proj_row
            .get("unpriced_session_count")
            .unwrap()
            .as_i64()
            .unwrap(),
        1,
        "breakdown unpriced"
    );
}
