//! Integration tests for the DB-backed chat session endpoints.
//!
//! These exercise the sibling CRUD endpoints to `/api/chat/completions`:
//! list (agent_type=chat filter), message round-trip (incl. tool_calls
//! with full `input` payloads), PATCH rename, and DELETE cascade.  The
//! handler path (streaming + auto-title) is exercised by a direct
//! repository-level test because our unit harness has no stand-in
//! `LlmProvider`; that keeps the coverage honest without faking
//! network I/O.

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::events::EventBus;
use crate::server::{self, AppState};
use crate::test_helpers;
use djinn_core::auth_context::SESSION_USER_ID;
use djinn_db::{
    CreateUserAuthSession, Database, SessionAuthRepository, SessionMessageRepository,
    SessionRepository, UserRepository, repositories::session::CreateSessionParams,
};

async fn get_json(app: axum::Router, uri: &str) -> (axum::http::StatusCode, Value) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    (status, value)
}

async fn patch_json(app: axum::Router, uri: &str, body: Value) -> axum::http::StatusCode {
    let req = axum::http::Request::builder()
        .method("PATCH")
        .uri(uri)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

async fn delete(app: axum::Router, uri: &str) -> axum::http::StatusCode {
    let req = axum::http::Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap().status()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_sessions_filters_to_chat_agent_type() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());

    // Seed one chat and one non-chat session via whichever repo method
    // matches the agent_type.
    let chat_id = uuid::Uuid::now_v7().to_string();
    session_repo
        .upsert_chat_session(&chat_id, "openai/gpt-4o-mini")
        .await
        .unwrap();

    // Non-chat session needs a project + task.
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "openai/gpt-4o-mini",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: "unpriced",
        })
        .await
        .unwrap();

    let app = test_helpers::create_test_app_with_db(db);
    let (status, body) = get_json(app, "/api/chat/sessions").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "only chat session should be returned");
    assert_eq!(sessions[0]["id"].as_str().unwrap(), chat_id);
    assert_eq!(sessions[0]["title"].as_str().unwrap(), "New Chat");
    assert!(sessions[0]["project_slug"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_messages_round_trips_tool_calls_with_input() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());
    let message_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());

    let chat_id = uuid::Uuid::now_v7().to_string();
    session_repo
        .upsert_chat_session(&chat_id, "openai/gpt-4o-mini")
        .await
        .unwrap();

    // User turn (single text block).
    let user_content = json!([{"type": "text", "text": "search the memory"}]);
    message_repo
        .insert_message(&chat_id, "", "user", &user_content.to_string(), None)
        .await
        .unwrap();

    // Assistant turn with text + tool_use block carrying full input.
    let assistant_content = json!([
        {"type": "text", "text": "I'll search."},
        {
            "type": "tool_use",
            "id": "call-123",
            "name": "memory_search",
            "input": {"query": "chat persistence", "limit": 5}
        }
    ]);
    message_repo
        .insert_message(
            &chat_id,
            "",
            "assistant",
            &assistant_content.to_string(),
            None,
        )
        .await
        .unwrap();

    let app = test_helpers::create_test_app_with_db(db);
    let (status, body) = get_json(app, &format!("/api/chat/sessions/{chat_id}/messages")).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);

    // User message content surfaces as a plain string (single text block
    // simplification in the response DTO).
    assert_eq!(messages[0]["role"].as_str().unwrap(), "user");
    assert_eq!(
        messages[0]["content"].as_str().unwrap(),
        "search the memory"
    );

    // Assistant message preserves tool_use `input` verbatim.
    assert_eq!(messages[1]["role"].as_str().unwrap(), "assistant");
    let tool_calls = messages[1]["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["name"].as_str().unwrap(), "memory_search");
    assert_eq!(
        tool_calls[0]["input"],
        json!({"query": "chat persistence", "limit": 5})
    );
}

/// The tool-result `user` row that incremental persistence writes after an
/// assistant tool-call turn must NOT surface as its own (contentless)
/// bubble — it is folded into the preceding assistant message, stamping
/// `success` per call so the UI status dot is right on reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_messages_folds_tool_results_into_preceding_assistant() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());
    let message_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());

    let chat_id = uuid::Uuid::now_v7().to_string();
    session_repo
        .upsert_chat_session(&chat_id, "openai/gpt-4o-mini")
        .await
        .unwrap();

    // user → assistant(text + two tool_use) → user(two tool_result), exactly
    // as `run_chat_loop` now persists a tool turn.
    let user_content = json!([{"type": "text", "text": "look it up"}]);
    message_repo
        .insert_message(&chat_id, "", "user", &user_content.to_string(), None)
        .await
        .unwrap();

    let assistant_content = json!([
        {"type": "text", "text": "On it."},
        {"type": "tool_use", "id": "call-ok", "name": "memory_search", "input": {"q": "a"}},
        {"type": "tool_use", "id": "call-bad", "name": "code_graph", "input": {"q": "b"}}
    ]);
    message_repo
        .insert_message(
            &chat_id,
            "",
            "assistant",
            &assistant_content.to_string(),
            None,
        )
        .await
        .unwrap();

    let tool_results = json!([
        {"type": "tool_result", "tool_use_id": "call-ok", "content": [{"type": "text", "text": "hit"}], "is_error": false},
        {"type": "tool_result", "tool_use_id": "call-bad", "content": [{"type": "text", "text": "boom"}], "is_error": true}
    ]);
    message_repo
        .insert_message(&chat_id, "", "user", &tool_results.to_string(), None)
        .await
        .unwrap();

    let app = test_helpers::create_test_app_with_db(db);
    let (status, body) = get_json(app, &format!("/api/chat/sessions/{chat_id}/messages")).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let messages = body["messages"].as_array().unwrap();

    // The tool-result row is folded away — two messages, not three.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"].as_str().unwrap(), "user");
    assert_eq!(messages[1]["role"].as_str().unwrap(), "assistant");

    // Both tool calls are present, with success matched by call_id.
    let tool_calls = messages[1]["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0]["name"].as_str().unwrap(), "memory_search");
    assert!(tool_calls[0]["success"].as_bool().unwrap());
    assert_eq!(tool_calls[1]["name"].as_str().unwrap(), "code_graph");
    assert!(!tool_calls[1]["success"].as_bool().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_updates_chat_title() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());

    let chat_id = uuid::Uuid::now_v7().to_string();
    session_repo
        .upsert_chat_session(&chat_id, "openai/gpt-4o-mini")
        .await
        .unwrap();

    let app = test_helpers::create_test_app_with_db(db.clone());
    let status = patch_json(
        app,
        &format!("/api/chat/sessions/{chat_id}"),
        json!({"title": "Renamed Chat"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    let updated = session_repo
        .get_chat_session(&chat_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.title.as_deref(), Some("Renamed Chat"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patch_rejects_empty_title() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());

    let chat_id = uuid::Uuid::now_v7().to_string();
    session_repo
        .upsert_chat_session(&chat_id, "openai/gpt-4o-mini")
        .await
        .unwrap();

    let app = test_helpers::create_test_app_with_db(db);
    let status = patch_json(
        app,
        &format!("/api/chat/sessions/{chat_id}"),
        json!({"title": "   "}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_cascades_messages() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());
    let message_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());

    let chat_id = uuid::Uuid::now_v7().to_string();
    session_repo
        .upsert_chat_session(&chat_id, "openai/gpt-4o-mini")
        .await
        .unwrap();

    message_repo
        .insert_message(
            &chat_id,
            "",
            "user",
            r#"[{"type":"text","text":"hi"}]"#,
            None,
        )
        .await
        .unwrap();
    message_repo
        .insert_message(
            &chat_id,
            "",
            "assistant",
            r#"[{"type":"text","text":"hello"}]"#,
            None,
        )
        .await
        .unwrap();

    // Sanity: messages exist before deletion.
    let conv_before = message_repo.load_conversation(&chat_id).await.unwrap();
    assert_eq!(conv_before.messages.len(), 2);

    let app = test_helpers::create_test_app_with_db(db.clone());
    let status = delete(app, &format!("/api/chat/sessions/{chat_id}")).await;
    assert_eq!(status, axum::http::StatusCode::NO_CONTENT);

    // Session row is gone.
    assert!(
        session_repo
            .get_chat_session(&chat_id)
            .await
            .unwrap()
            .is_none()
    );
    // And the FK cascade dropped the message rows along with it.
    let conv_after = message_repo.load_conversation(&chat_id).await.unwrap();
    assert!(
        conv_after.messages.is_empty(),
        "FK cascade should remove session messages on session delete"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_nonexistent_session_returns_404() {
    let db = test_helpers::create_test_db();
    let app = test_helpers::create_test_app_with_db(db);
    let fake = uuid::Uuid::now_v7().to_string();
    let status = delete(app, &format!("/api/chat/sessions/{fake}")).await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
}

// ── Private chat sessions: authenticated cross-user isolation (Part 2) ──────
//
// These run with a GitHub App configured so the gating mirrors `/mcp`: auth is
// required, and a user must own a chat session to list/open/rename/delete it.

/// Build a router whose state has a GitHub App configured (so chat-session auth
/// gating requires authentication), returning both router and state.
async fn auth_required_app(db: Database) -> (axum::Router, AppState) {
    let cancel = CancellationToken::new();
    let state = AppState::new(db, cancel);
    let cfg = djinn_provider::github_app::AppConfig {
        app_id: 1,
        slug: "djinn".into(),
        client_id: "Iv1.x".into(),
        client_secret: "y".into(),
        pem: "PEM".into(),
        webhook_secret: "w".into(),
        public_url: "http://127.0.0.1:8372".into(),
    };
    state.set_app_config(Some(Arc::new(cfg))).await;
    let app = server::router(state.clone(), false);
    (app, state)
}

/// Seed a user + a live `djinn_session` cookie; returns (user_id, cookie_token).
async fn seed_session(db: &Database, github_id: i64, login: &str) -> (String, String) {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(github_id, login, None, None)
        .await
        .unwrap();
    let token = format!("sess-{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user.id,
            github_login: login,
            github_name: None,
            github_avatar_url: None,
            github_access_token: "gho_test",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
    (user.id, token)
}

async fn req_status(
    app: &axum::Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> axum::http::StatusCode {
    let mut builder = axum::http::Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", format!("djinn_session={c}"));
    }
    let body = match body {
        Some(v) => {
            builder = builder.header(CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    app.clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthenticated_request_is_401_when_app_configured() {
    let db = test_helpers::create_test_db();
    let (app, _state) = auth_required_app(db).await;
    let status = req_status(&app, "GET", "/api/chat/sessions", None, None).await;
    assert_eq!(status, axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_only_returns_own_sessions() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());

    let (alice_id, alice_cookie) = seed_session(&db, 70001, "alice-http").await;
    let (_bob_id, _bob_cookie) = seed_session(&db, 70002, "bob-http").await;

    let alice_sid = uuid::Uuid::now_v7().to_string();
    SESSION_USER_ID
        .scope(Some(alice_id.clone()), async {
            session_repo
                .upsert_chat_session(&alice_sid, "openai/gpt-5")
                .await
                .unwrap();
        })
        .await;
    let bob_sid = uuid::Uuid::now_v7().to_string();
    // Bob's session via a direct stamped upsert.
    SESSION_USER_ID
        .scope(Some(_bob_id.clone()), async {
            session_repo
                .upsert_chat_session(&bob_sid, "openai/gpt-5")
                .await
                .unwrap();
        })
        .await;

    let (app, _state) = auth_required_app(db).await;
    let req = axum::http::Request::builder()
        .method("GET")
        .uri("/api/chat/sessions")
        .header("cookie", format!("djinn_session={alice_cookie}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let sessions = value["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "Alice sees only her session");
    assert_eq!(sessions[0]["id"].as_str().unwrap(), alice_sid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cannot_open_rename_or_delete_another_users_session() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());

    let (_alice_id, alice_cookie) = seed_session(&db, 80001, "alice-x").await;
    let (bob_id, _bob_cookie) = seed_session(&db, 80002, "bob-x").await;

    // Bob owns a chat session.
    let bob_sid = uuid::Uuid::now_v7().to_string();
    SESSION_USER_ID
        .scope(Some(bob_id.clone()), async {
            session_repo
                .upsert_chat_session(&bob_sid, "openai/gpt-5")
                .await
                .unwrap();
        })
        .await;

    let (app, _state) = auth_required_app(db).await;

    // Alice opening Bob's session messages → 404 (no existence leak).
    let open = req_status(
        &app,
        "GET",
        &format!("/api/chat/sessions/{bob_sid}/messages"),
        Some(&alice_cookie),
        None,
    )
    .await;
    assert_eq!(open, axum::http::StatusCode::NOT_FOUND);

    // Alice renaming Bob's session → 404.
    let rename = req_status(
        &app,
        "PATCH",
        &format!("/api/chat/sessions/{bob_sid}"),
        Some(&alice_cookie),
        Some(json!({"title": "hijacked"})),
    )
    .await;
    assert_eq!(rename, axum::http::StatusCode::NOT_FOUND);

    // Alice deleting Bob's session → 404.
    let del = req_status(
        &app,
        "DELETE",
        &format!("/api/chat/sessions/{bob_sid}"),
        Some(&alice_cookie),
        None,
    )
    .await;
    assert_eq!(del, axum::http::StatusCode::NOT_FOUND);

    // And Bob's session still exists + untouched.
    let still = session_repo
        .get_chat_session(&bob_sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.title.as_deref(), Some("New Chat"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_can_rename_and_delete_own_session() {
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());

    let (alice_id, alice_cookie) = seed_session(&db, 90001, "alice-own").await;
    let sid = uuid::Uuid::now_v7().to_string();
    SESSION_USER_ID
        .scope(Some(alice_id.clone()), async {
            session_repo
                .upsert_chat_session(&sid, "openai/gpt-5")
                .await
                .unwrap();
        })
        .await;

    let (app, _state) = auth_required_app(db.clone()).await;

    let rename = req_status(
        &app,
        "PATCH",
        &format!("/api/chat/sessions/{sid}"),
        Some(&alice_cookie),
        Some(json!({"title": "My Chat"})),
    )
    .await;
    assert_eq!(rename, axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        session_repo
            .get_chat_session(&sid)
            .await
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("My Chat")
    );

    let del = req_status(
        &app,
        "DELETE",
        &format!("/api/chat/sessions/{sid}"),
        Some(&alice_cookie),
        None,
    )
    .await;
    assert_eq!(del, axum::http::StatusCode::NO_CONTENT);
    assert!(session_repo.get_chat_session(&sid).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_title_path_overwrites_title_via_repo() {
    // The handler's auto-title pass requires a real provider to call
    // out to.  The repository-level UPDATE it performs is the
    // load-bearing side-effect, so we verify that path directly:
    // `update_chat_title` fires exactly once and then the session row
    // no longer carries the default "New Chat" sentinel.  This
    // mirrors the post-condition the SSE `session_title` event signals.
    let db = test_helpers::create_test_db();
    let session_repo = SessionRepository::new(db.clone(), EventBus::noop());

    let chat_id = uuid::Uuid::now_v7().to_string();
    let first = session_repo
        .upsert_chat_session(&chat_id, "openai/gpt-4o-mini")
        .await
        .unwrap();
    assert_eq!(first.title.as_deref(), Some("New Chat"));

    // Second upsert is idempotent and keeps the existing title.
    let second = session_repo
        .upsert_chat_session(&chat_id, "openai/gpt-4o-mini")
        .await
        .unwrap();
    assert_eq!(second.title.as_deref(), Some("New Chat"));
    assert_eq!(second.id, first.id);

    // Apply the title once — simulating the handler's post-first-reply
    // auto-title write.
    session_repo
        .update_chat_title(&chat_id, "DB persistence discussion")
        .await
        .unwrap();
    let after = session_repo
        .get_chat_session(&chat_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.title.as_deref(), Some("DB persistence discussion"));

    // needs_title gate reads `title != DEFAULT_CHAT_TITLE`, so a
    // subsequent request should NOT re-fire the title pass.
    assert_ne!(after.title.as_deref(), Some("New Chat"));
}
