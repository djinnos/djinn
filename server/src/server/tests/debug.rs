use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use djinn_agent::actors::coordinator::{
    BreakerDebugEntry, CoordinatorDeps, CoordinatorHandle, DebugDispatchState, DebugSlot,
    DebugTotals, DispatchPauseView,
};
use djinn_agent::actors::slot::{SlotPoolConfig, SlotPoolHandle};
use djinn_db::{CreateUserAuthSession, Database, SessionAuthRepository, UserRepository};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::server::{self, AppState};
use crate::test_helpers;

async fn app_state_with_agents(db: Database) -> AppState {
    let state = AppState::new(db, CancellationToken::new());
    let pool = SlotPoolHandle::spawn(
        state.agent_context(),
        state.cancel().clone(),
        SlotPoolConfig {
            models: Vec::new(),
            role_priorities: std::collections::HashMap::new(),
        },
    );
    let coordinator = CoordinatorHandle::spawn(CoordinatorDeps::new(
        state.events().clone(),
        state.cancel().clone(),
        state.db().clone(),
        pool.clone(),
        state.catalog().clone(),
        state.health_tracker().clone(),
        std::sync::Arc::new(djinn_agent::roles::RoleRegistry::new()),
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        state.lsp().clone(),
    ));
    state.set_agent_handles_for_tests(pool, coordinator).await;
    state
}

async fn app_for(db: Database) -> axum::Router {
    server::router(app_state_with_agents(db).await, false)
}

async fn seed_session(db: &Database, login: &str, is_admin: bool) -> String {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(rand_github_id(), login, None, None)
        .await
        .unwrap();
    UserRepository::new(db.clone())
        .set_admin_status(&user.id, is_admin)
        .await
        .unwrap();
    let token = format!("debug-sess-{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user.id,
            github_login: login,
            github_name: None,
            github_avatar_url: None,
            github_access_token: "gho_debug_test",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
    token
}

fn rand_github_id() -> i64 {
    let bytes = *uuid::Uuid::now_v7().as_bytes();
    i64::from_be_bytes(bytes[8..16].try_into().unwrap()).unsigned_abs() as i64
}

async fn get_debug(app: &axum::Router, cookie: Option<&str>) -> axum::http::Response<Body> {
    let mut builder = Request::builder().uri("/debug/dispatch-state");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", format!("djinn_session={cookie}"));
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn response_json(resp: axum::http::Response<Body>) -> Value {
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).expect("debug response should be JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_debug_dispatch_state_unauthenticated_returns_401() {
    let db = test_helpers::create_test_db();
    let app = app_for(db).await;

    let resp = get_debug(&app, None).await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_debug_dispatch_state_non_admin_returns_403() {
    let db = test_helpers::create_test_db();
    let cookie = seed_session(&db, "debug-non-admin", false).await;
    let app = app_for(db).await;

    let resp = get_debug(&app, Some(&cookie)).await;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_debug_dispatch_state_admin_returns_200() {
    let db = test_helpers::create_test_db();
    let cookie = seed_session(&db, "debug-admin", true).await;
    let app = app_for(db).await;

    let resp = get_debug(&app, Some(&cookie)).await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json; charset=utf-8")
    );
    let json = response_json(resp).await;
    assert!(json.get("snapshot_at").and_then(Value::as_str).is_some());
    assert!(json.get("paused").and_then(Value::as_object).is_some());
    assert!(json.get("totals").and_then(Value::as_object).is_some());
}

#[test]
fn test_debug_dispatch_state_json_deterministic() {
    let state = DebugDispatchState {
        snapshot_at: "2026-06-15T17:30:00.123Z".to_owned(),
        cooldowns: vec![djinn_agent::DebugCooldown {
            task_id: "task-1".to_owned(),
            short_id: "abcd".to_owned(),
            expires_at: "2026-06-15T17:35:00.000Z".to_owned(),
            scope: "task".to_owned(),
        }],
        failure_streaks: vec![djinn_agent::DebugFailureStreak {
            task_id: "task-2".to_owned(),
            short_id: "efgh".to_owned(),
            streak: 3,
        }],
        inflight_ledger: vec![djinn_agent::DebugInflightEntry {
            task_id: "task-3".to_owned(),
            short_id: "ijkl".to_owned(),
            creator: Some("user-1".to_owned()),
            model: "provider/model-a".to_owned(),
            started_at: "2026-06-15T17:29:00.000Z".to_owned(),
        }],
        slot_pool: vec![
            DebugSlot {
                slot_id: 0,
                model: "provider/model-a".to_owned(),
                state: "free".to_owned(),
                task_id: None,
                started_at: None,
            },
            DebugSlot {
                slot_id: 1,
                model: "provider/model-a".to_owned(),
                state: "busy".to_owned(),
                task_id: Some("task-3".to_owned()),
                started_at: Some("1234567890".to_owned()),
            },
        ],
        breaker: vec![BreakerDebugEntry {
            scope: Some("user-1".to_owned()),
            model: "provider/model-a".to_owned(),
            state: "open".to_owned(),
            until: Some("2026-06-15T17:40:00.000Z".to_owned()),
            consecutive_failures: 3,
        }],
        paused: DispatchPauseView {
            global: false,
            projects: vec!["project-a".to_owned()],
            users: vec!["user-1".to_owned()],
        },
        totals: DebugTotals {
            cooldowns_active: 1,
            inflight_ledger_size: 1,
            free_slots: 1,
            busy_slots: 1,
            open_breakers: 1,
        },
    };

    insta::assert_snapshot!(serde_json::to_string_pretty(&state).unwrap(), @r###"
{
  "snapshot_at": "2026-06-15T17:30:00.123Z",
  "cooldowns": [
    {
      "task_id": "task-1",
      "short_id": "abcd",
      "expires_at": "2026-06-15T17:35:00.000Z",
      "scope": "task"
    }
  ],
  "failure_streaks": [
    {
      "task_id": "task-2",
      "short_id": "efgh",
      "streak": 3
    }
  ],
  "inflight_ledger": [
    {
      "task_id": "task-3",
      "short_id": "ijkl",
      "creator": "user-1",
      "model": "provider/model-a",
      "started_at": "2026-06-15T17:29:00.000Z"
    }
  ],
  "slot_pool": [
    {
      "slot_id": 0,
      "model": "provider/model-a",
      "state": "free",
      "task_id": null,
      "started_at": null
    },
    {
      "slot_id": 1,
      "model": "provider/model-a",
      "state": "busy",
      "task_id": "task-3",
      "started_at": "1234567890"
    }
  ],
  "breaker": [
    {
      "scope": "user-1",
      "model": "provider/model-a",
      "state": "open",
      "until": "2026-06-15T17:40:00.000Z",
      "consecutive_failures": 3
    }
  ],
  "paused": {
    "global": false,
    "projects": [
      "project-a"
    ],
    "users": [
      "user-1"
    ]
  },
  "totals": {
    "cooldowns_active": 1,
    "inflight_ledger_size": 1,
    "free_slots": 1,
    "busy_slots": 1,
    "open_breakers": 1
  }
}
"###);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_debug_dispatch_state_does_not_mutate() {
    let db = test_helpers::create_test_db();
    let state = app_state_with_agents(db).await;
    let coordinator = state.coordinator().await.unwrap();

    let first = coordinator.debug_dispatch_state().await.unwrap();
    let second = coordinator.debug_dispatch_state().await.unwrap();

    assert_eq!(first, second);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_debug_dispatch_state_lock_contention_under_5s() {
    let db = test_helpers::create_test_db();
    let state = app_state_with_agents(db).await;
    let coordinator = state.coordinator().await.unwrap();
    let started = tokio::time::Instant::now();

    let mut tasks = Vec::with_capacity(1000);
    for _ in 0..1000 {
        let coordinator = coordinator.clone();
        tasks.push(tokio::spawn(async move {
            coordinator.debug_dispatch_state().await.unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "1000 debug snapshots should complete under 5s"
    );
}
