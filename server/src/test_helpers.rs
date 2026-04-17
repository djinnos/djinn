use std::path::Path;

use axum::body::Body;
use axum::http::header::{ACCEPT, CONTENT_TYPE};
use http_body_util::BodyExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::events::EventBus;
use crate::server::{self, AppState};
use djinn_core::models::{Epic, Note, Project, SessionRecord, Task};
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::{
    Database, EpicCreateInput, EpicRepository, NoteRepository, ProjectRepository,
    SessionRepository, TaskRepository,
};

pub(crate) fn workspace_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).expect("create server test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create server test tempdir")
}

/// Create an in-memory database with all migrations applied.
pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Create an Axum router wired to a fresh in-memory database.
pub fn create_test_app() -> axum::Router {
    let db = create_test_db();
    create_test_app_with_db(db)
}

/// Create an Axum router with a test project pre-registered in the DB (bypasses
/// `project_add` path and GitHub validation for CI compatibility).
///
/// Returns `(Router, project_path, TempDir)`. The `TempDir` guard must be kept
/// alive for the duration of the test or the temp directory will be deleted.
pub async fn create_test_app_with_project() -> (axum::Router, String, tempfile::TempDir) {
    let db = create_test_db();
    let dir = workspace_tempdir("server-test-project-");
    let path = dir.path().to_string_lossy().to_string();
    let repo = ProjectRepository::new(db.clone(), test_events());
    repo.create("test-project", &path)
        .await
        .expect("failed to register test project");
    let app = create_test_app_with_db(db);
    (app, path, dir)
}

/// Create an Axum router wired to the given database (for tests that seed data externally).
pub fn create_test_app_with_db(db: Database) -> axum::Router {
    let cancel = CancellationToken::new();
    let state = AppState::new(db, cancel);
    server::router(state)
}

/// Create an `AppState` backed by an in-memory database (for unit tests that
/// need state but not a full Axum router).
pub async fn test_app_state_in_memory() -> AppState {
    let db = create_test_db();
    let cancel = CancellationToken::new();
    AppState::new(db, cancel)
}

/// Create an `AgentContext` from a database and cancellation token.
///
/// Use this in actor/agent tests instead of constructing `AppState` directly.
pub fn agent_context_from_db(
    db: Database,
    cancel: CancellationToken,
) -> djinn_agent::context::AgentContext {
    AppState::new(db, cancel).agent_context()
}

pub fn test_events() -> EventBus {
    EventBus::noop()
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

/// Create a project backed by a real temporary directory on disk.
/// Returns the project and a `TempDir` guard — the directory is cleaned up when the guard drops.
pub async fn create_test_project_with_dir(db: &Database) -> (Project, tempfile::TempDir) {
    let dir = workspace_tempdir("server-test-project-");
    let repo = ProjectRepository::new(db.clone(), test_events());
    let path = dir.path().to_string_lossy().to_string();
    let name = dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let project = repo
        .create(&name, &path)
        .await
        .expect("failed to create test project");
    (project, dir)
}

pub async fn create_test_epic(db: &Database, project_id: &str) -> Epic {
    let repo = EpicRepository::new(db.clone(), test_events());
    repo.create_for_project(
        project_id,
        EpicCreateInput {
            title: "test-epic",
            description: "test epic description",
            emoji: "🧪",
            color: "blue",
            owner: "test-owner",
            memory_refs: None,
            status: None,
            auto_breakdown: None,
            originating_adr_id: None,
        },
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
            None,
        )
        .await
        .expect("failed to create test task");
    assert_eq!(task.status, "open");
    // Ensure tasks have AC so Start transitions succeed in tests.
    repo.update(
        &task.id,
        &task.title,
        &task.description,
        &task.design,
        task.priority,
        &task.owner,
        &task.labels,
        r#"[{"description":"default test criterion","met":false}]"#,
    )
    .await
    .expect("failed to set test task acceptance criteria")
}

pub async fn create_test_session(db: &Database, project_id: &str, task_id: &str) -> SessionRecord {
    let repo = SessionRepository::new(db.clone(), test_events());
    repo.create(CreateSessionParams {
        project_id,
        task_id: Some(task_id),
        model: "test-model",
        agent_type: "worker",
        worktree_path: Some("/tmp/djinn-test-worktree"),
        metadata_json: None,
    task_run_id: None,
    })
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
            if !payload.is_empty()
                && let Ok(value) = serde_json::from_str::<Value>(&payload)
            {
                events.push(value);
            }
            data_lines.clear();
        }
    }

    if !data_lines.is_empty() {
        let payload = data_lines.join("\n").trim().to_string();
        if !payload.is_empty()
            && let Ok(value) = serde_json::from_str::<Value>(&payload)
        {
            events.push(value);
        }
    }

    events
}

async fn mcp_jsonrpc(
    app: &axum::Router,
    session_id: &str,
    id: i64,
    method: &str,
    params: Value,
) -> Value {
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

pub async fn mcp_call_tool_with_headers(
    app: &axum::Router,
    session_id: &str,
    tool_name: &str,
    params: Value,
    extra_headers: &[(&str, &str)],
) -> Value {
    let payload = serde_json::json!({
        "name": tool_name,
        "arguments": params,
    });

    let req = extra_headers.iter().fold(
        axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-session-id", session_id),
        |builder, (name, value)| builder.header(*name, *value),
    );

    let req = req
        .body(Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 999,
                "method": "tools/call",
                "params": payload,
            })
            .to_string(),
        ))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let raw = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
    let event = if let Ok(single) = serde_json::from_str::<Value>(&raw)
        && single.get("id") == Some(&Value::from(999))
    {
        single
    } else {
        parse_sse_json_events(&raw)
            .into_iter()
            .find(|event| event.get("id") == Some(&Value::from(999)))
            .expect("missing JSON-RPC event with requested id")
    };

    if let Some(error) = event.get("error") {
        return serde_json::json!({
            "ok": false,
            "error": error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown MCP error"),
        });
    }

    let result = event
        .get("result")
        .unwrap_or_else(|| panic!("tools/call missing result payload: {event}"));
    extract_tool_result_payload(result)
}

pub async fn mcp_call_tool(
    app: &axum::Router,
    session_id: &str,
    tool_name: &str,
    params: Value,
) -> Value {
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

    if let Some(error) = event.get("error") {
        return serde_json::json!({
            "ok": false,
            "error": error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown MCP error"),
        });
    }

    let result = event
        .get("result")
        .unwrap_or_else(|| panic!("tools/call missing result payload: {event}"));
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

pub async fn initialize_mcp_session_with_headers(
    app: &axum::Router,
    extra_headers: &[(&str, &str)],
) -> String {
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

    let mut init_req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream");
    for (name, value) in extra_headers {
        init_req = init_req.header(*name, *value);
    }
    let init_req = init_req
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
    let mut init_notify_req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .header("mcp-session-id", session_id.clone());
    for (name, value) in extra_headers {
        init_notify_req = init_notify_req.header(*name, *value);
    }
    let init_notify_req = init_notify_req
        .body(Body::from(init_notify_payload.to_string()))
        .unwrap();
    let init_notify_resp = app.clone().oneshot(init_notify_req).await.unwrap();
    assert_eq!(init_notify_resp.status(), 202);

    session_id
}
