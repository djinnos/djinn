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
use tower::ServiceExt;

use crate::events::EventBus;
use crate::test_helpers;
use djinn_db::{
    SessionMessageRepository, SessionRepository, repositories::session::CreateSessionParams,
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

async fn patch_json(
    app: axum::Router,
    uri: &str,
    body: Value,
) -> axum::http::StatusCode {
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
    let (status, body) =
        get_json(app, &format!("/api/chat/sessions/{chat_id}/messages")).await;
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

    let updated = session_repo.get_chat_session(&chat_id).await.unwrap().unwrap();
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
    assert!(session_repo.get_chat_session(&chat_id).await.unwrap().is_none());
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
    let after = session_repo.get_chat_session(&chat_id).await.unwrap().unwrap();
    assert_eq!(after.title.as_deref(), Some("DB persistence discussion"));

    // needs_title gate reads `title != DEFAULT_CHAT_TITLE`, so a
    // subsequent request should NOT re-fire the title pass.
    assert_ne!(after.title.as_deref(), Some("New Chat"));
}
