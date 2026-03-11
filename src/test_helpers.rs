use std::path::Path;

use axum::body::Body;
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::db::connection::Database;
use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::note::NoteRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::events::DjinnEvent;
use crate::models::epic::Epic;
use crate::models::note::Note;
use crate::models::project::Project;
use crate::models::session::SessionRecord;
use crate::models::task::Task;
use crate::server::{self, AppState};

/// Create an in-memory database with all migrations applied.
pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Create an Axum router wired to a fresh in-memory database.
pub fn create_test_app() -> axum::Router {
    let db = create_test_db();
    create_test_app_with_db(db)
}

/// Create an Axum router wired to the given database (for tests that seed data externally).
pub fn create_test_app_with_db(db: Database) -> axum::Router {
    let cancel = CancellationToken::new();
    let state = AppState::new(db, cancel);
    server::router(state)
}

fn test_events() -> broadcast::Sender<DjinnEvent> {
    let (tx, _rx) = broadcast::channel(256);
    tx
}

pub async fn create_test_project(db: &Database) -> Project {
    let repo = ProjectRepository::new(db.clone(), test_events());
    let id = uuid::Uuid::now_v7();
    let path = format!("/tmp/djinn-test-project-{id}");
    let name = format!("test-project-{id}");
    repo.create(&name, &path)
        .await
        .expect("failed to create test project")
}

pub async fn create_test_epic(db: &Database, project_id: &str) -> Epic {
    let repo = EpicRepository::new(db.clone(), test_events());
    repo.create_for_project(
        project_id,
        "test-epic",
        "test epic description",
        "🧪",
        "blue",
        "test-owner",
    )
    .await
    .expect("failed to create test epic")
}

pub async fn create_test_task(db: &Database, project_id: &str, epic_id: &str) -> Task {
    let repo = TaskRepository::new(db.clone(), test_events());
    let task = repo
        .create_in_project(
            project_id,
            Some(epic_id),
            "test-task",
            "test task description",
            "test task design",
            "task",
            2,
            "test-owner",
            None,
        )
        .await
        .expect("failed to create test task");
    assert_eq!(task.status, "backlog");
    task
}

pub async fn create_test_session(db: &Database, project_id: &str, task_id: &str) -> SessionRecord {
    let repo = SessionRepository::new(db.clone(), test_events());
    repo.create(
        project_id,
        Some(task_id),
        "test-model",
        "worker",
        Some("/tmp/djinn-test-worktree"),
        None,
    )
    .await
    .expect("failed to create test session")
}

pub async fn create_test_note(db: &Database, project_id: &str) -> Note {
    let repo = NoteRepository::new(db.clone(), test_events());
    let project_repo = ProjectRepository::new(db.clone(), test_events());
    let project = project_repo
        .get(project_id)
        .await
        .expect("failed to load project for note")
        .expect("project not found for note");

    std::fs::create_dir_all(Path::new(&project.path)).expect("failed to create test project path");

    repo.create(
        project_id,
        Path::new(&project.path),
        "test note",
        "test note body",
        "research",
        "[]",
    )
    .await
    .expect("failed to create test note")
}

fn parse_sse_json_events(body: &str) -> Vec<Value> {
    let mut events = Vec::new();
    let mut data_lines: Vec<String> = Vec::new();

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
            continue;
        }

        if line.is_empty() && !data_lines.is_empty() {
            let payload = data_lines.join("\n").trim().to_string();
            if !payload.is_empty() && let Ok(value) = serde_json::from_str::<Value>(&payload) {
                events.push(value);
            }
            data_lines.clear();
        }
    }

    if !data_lines.is_empty() {
        let payload = data_lines.join("\n").trim().to_string();
        if !payload.is_empty() && let Ok(value) = serde_json::from_str::<Value>(&payload) {
            events.push(value);
        }
    }

    events
}

async fn mcp_jsonrpc(app: &axum::Router, session_id: &str, id: i64, method: &str, params: Value) -> Value {
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .header("mcp-session-id", session_id)
        .body(Body::from(payload.to_string()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8(body.to_vec()).expect("response body should be utf-8");

    if let Ok(single) = serde_json::from_str::<Value>(&raw)
        && single.get("id") == Some(&Value::from(id))
    {
        return single;
    }

    parse_sse_json_events(&raw)
        .into_iter()
        .find(|event| event.get("id") == Some(&Value::from(id)))
        .expect("missing JSON-RPC event with requested id")
}

pub fn extract_tool_result_payload(result: &Value) -> Value {
    if let Some(structured) = result.get("structuredContent") {
        return structured.clone();
    }

    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item.get("text").and_then(Value::as_str)
                && let Ok(parsed) = serde_json::from_str::<Value>(text)
            {
                return parsed;
            }
        }
    }

    result.clone()
}

pub async fn mcp_call_tool(app: &axum::Router, session_id: &str, tool_name: &str, params: Value) -> Value {
    let event = mcp_jsonrpc(
        app,
        session_id,
        999,
        "tools/call",
        serde_json::json!({
            "name": tool_name,
            "arguments": params,
        }),
    )
    .await;

    let result = event
        .get("result")
        .expect("tools/call missing result payload");
    extract_tool_result_payload(result)
}

pub async fn initialize_mcp_session(app: &axum::Router) -> String {
    let initialize_payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {
                "name": "schema-test-client",
                "version": "0.0.0"
            }
        }
    });

    let init_req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .body(Body::from(initialize_payload.to_string()))
        .unwrap();
    let init_resp = app.clone().oneshot(init_req).await.unwrap();
    assert_eq!(init_resp.status(), 200);

    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("missing mcp-session-id header on initialize response")
        .to_string();

    let init_notify_payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });
    let init_notify_req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .header("mcp-session-id", session_id.clone())
        .body(Body::from(init_notify_payload.to_string()))
        .unwrap();
    let init_notify_resp = app.clone().oneshot(init_notify_req).await.unwrap();
    assert_eq!(init_notify_resp.status(), 202);

    session_id
}
