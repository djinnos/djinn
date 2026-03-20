use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde_json::Value;

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;

use crate::sse;

mod chat;
mod state;
pub use state::AppState;

async fn mcp_handler(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let worktree_root = req
        .headers()
        .get("x-djinn-worktree-root")
        .and_then(|value| value.to_str().ok())
        .map(std::path::PathBuf::from)
        .filter(|path| path.join(".git").exists());
    let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("failed to read MCP request body: {err}"),
            )
                .into_response();
        }
    };

    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid MCP JSON payload: {err}"),
            )
                .into_response();
        }
    };

    let response = match payload.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": payload.get("id").cloned().unwrap_or(Value::Null),
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "djinn-server",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            })
        }
        Some("notifications/initialized") => {
            return StatusCode::ACCEPTED.into_response();
        }
        Some("tools/call") => {
            let params = payload.get("params").cloned().unwrap_or(Value::Null);
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            match djinn_mcp::server::DjinnMcpServer::new(state.mcp_state())
                .dispatch_tool_with_worktree(name, args, worktree_root)
                .await
            {
                Ok(result) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": payload.get("id").cloned().unwrap_or(Value::Null),
                    "result": { "structuredContent": result }
                }),
                Err(error) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": payload.get("id").cloned().unwrap_or(Value::Null),
                    "error": { "code": -32000, "message": error }
                }),
            }
        }
        Some("tools/list") => {
            let tools =
                djinn_mcp::server::DjinnMcpServer::new(state.mcp_state()).all_tool_schemas();
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": payload.get("id").cloned().unwrap_or(Value::Null),
                "result": { "tools": tools }
            })
        }
        Some(method) => serde_json::json!({
            "jsonrpc": "2.0",
            "id": payload.get("id").cloned().unwrap_or(Value::Null),
            "error": { "code": -32601, "message": format!("method not found: {method}") }
        }),
        None => serde_json::json!({
            "jsonrpc": "2.0",
            "id": payload.get("id").cloned().unwrap_or(Value::Null),
            "error": { "code": -32600, "message": "missing method" }
        }),
    };

    let mut resp = axum::Json(response).into_response();
    if payload.get("method").and_then(Value::as_str) == Some("initialize") {
        resp.headers_mut()
            .insert("mcp-session-id", HeaderValue::from_static("test-session"));
    }
    resp
}

/// Build the application router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/events", get(sse::events_handler))
        .route("/db-info", get(sse::db_info_handler))
        .route("/api/chat/completions", post(chat::completions_handler))
        .route("/mcp", post(mcp_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> axum::Json<HealthResponse> {
    axum::Json(HealthResponse { status: "ok" })
}

/// Run the server, blocking until shutdown signal.
///
/// After the cancellation token fires, the server waits up to 5 seconds for
/// in-flight connections to finish before returning.  This prevents the
/// process from hanging indefinitely on long-lived connections (SSE, MCP
/// streams) that didn't notice the shutdown signal.
pub async fn run(router: Router, port: u16, cancel: CancellationToken) {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind");

    tracing::info!(port, "listening on 0.0.0.0:{port}");

    // Clone the token so we can also use it for the deadline below.
    let shutdown_cancel = cancel.clone();
    let server = axum::serve(listener, router).with_graceful_shutdown(cancel.cancelled_owned());

    // Spawn the server so we can race it against a hard deadline.
    let handle = tokio::spawn(async move {
        if let Err(e) = server.await {
            tracing::error!(error = %e, "server error");
        }
    });

    // Wait for the shutdown signal, then give in-flight connections 5s.
    shutdown_cancel.cancelled().await;
    match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
        Ok(Ok(())) => tracing::info!("server shut down gracefully"),
        Ok(Err(e)) => tracing::warn!(error = %e, "server task panicked"),
        Err(_) => tracing::warn!("server shutdown timed out after 5s, forcing exit"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::body::Body;
    use axum::http::header::{ACCEPT, CONTENT_TYPE};
    use http_body_util::BodyExt;
    use insta::assert_json_snapshot;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use crate::server::{self, AppState};
    use crate::test_helpers;
    use djinn_core::models::DjinnSettings;
    use djinn_provider::repos::CredentialRepository;
    use tokio_util::sync::CancellationToken;

    const CONTRACT_PROJECT_PATH: &str = "/home/fernando/git/djinnos/djinn";

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

    fn canonicalize_json(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<_> = map.keys().cloned().collect();
                keys.sort();

                let mut out = serde_json::Map::new();
                for key in keys {
                    if let Some(child) = map.get(&key) {
                        out.insert(key, canonicalize_json(child));
                    }
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
            _ => value.clone(),
        }
    }

    /// Integration test: hit /health via tower::ServiceExt::oneshot().
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_returns_ok() {
        let app = test_helpers::create_test_app();

        let req = axum::http::Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), 200);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_initialize_returns_ok() {
        let app = test_helpers::create_test_app();

        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "test-client",
                    "version": "0.0.0"
                }
            }
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chat_completions_rejects_empty_messages() {
        let app = test_helpers::create_test_app();

        let payload = serde_json::json!({
            "model": "openai/gpt-4o-mini",
            "messages": []
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/chat/completions")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
        assert!(text.contains("messages must not be empty"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chat_completions_rejects_unknown_provider() {
        let app = test_helpers::create_test_app();

        let payload = serde_json::json!({
            "model": "doesnotexist/model",
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/chat/completions")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
        assert!(text.contains("unknown provider"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chat_completions_rejects_missing_provider_credential() {
        let app = test_helpers::create_test_app();

        let payload = serde_json::json!({
            "model": "openai/gpt-4o-mini",
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/chat/completions")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 400);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).expect("response body should be utf-8");
        assert!(text.contains("provider credential resolution failed"));
    }

    #[tokio::test]
    async fn all_tool_schemas_includes_cross_domain_tools() {
        let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
        let mcp = djinn_mcp::server::DjinnMcpServer::new(state.mcp_state());
        let tools = mcp.all_tool_schemas();
        assert!(!tools.is_empty(), "all_tool_schemas should not be empty");

        let names = tools
            .iter()
            .filter_map(|v| v.get("name").and_then(serde_json::Value::as_str))
            .collect::<std::collections::HashSet<_>>();

        for required in [
            "task_list",
            "epic_list",
            "memory_search",
            "project_list",
            "provider_catalog",
            "session_list",
            "settings_get",
            "system_ping",
        ] {
            assert!(
                names.contains(required),
                "missing required tool schema: {required}"
            );
        }
    }

    #[tokio::test]
    async fn chat_uses_router_derived_tool_schemas() {
        let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
        let mcp = djinn_mcp::server::DjinnMcpServer::new(state.mcp_state());

        let names = mcp
            .all_tool_schemas()
            .into_iter()
            .filter_map(|v| {
                v.get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect::<std::collections::HashSet<_>>();

        assert!(names.contains("credential_set"));
        assert!(names.contains("task_sync_enable"));
        assert!(names.contains("project_list"));
        assert!(names.contains("execution_start"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_tools_list_schemas_do_not_use_nonstandard_uint_or_nullable_without_type() {
        fn collect_bad_formats(
            tool_name: &str,
            schema_kind: &str,
            path: &str,
            value: &Value,
            bad: &mut Vec<String>,
            bad_nullable: &mut Vec<String>,
        ) {
            match value {
                Value::Object(map) => {
                    if let Some(Value::String(format)) = map.get("format")
                        && (format == "uint" || format.starts_with("uint"))
                    {
                        bad.push(format!(
                            "{tool_name} {schema_kind} {path}/format = {format}"
                        ));
                    }

                    if matches!(map.get("nullable"), Some(Value::Bool(true)))
                        && !matches!(map.get("type"), Some(Value::String(_)))
                    {
                        bad_nullable.push(format!(
                            "{tool_name} {schema_kind} {path} has nullable=true without a type"
                        ));
                    }

                    for (k, v) in map {
                        let next_path = format!("{path}/{k}");
                        collect_bad_formats(
                            tool_name,
                            schema_kind,
                            &next_path,
                            v,
                            bad,
                            bad_nullable,
                        );
                    }
                }
                Value::Array(items) => {
                    for (idx, item) in items.iter().enumerate() {
                        let next_path = format!("{path}[{idx}]");
                        collect_bad_formats(
                            tool_name,
                            schema_kind,
                            &next_path,
                            item,
                            bad,
                            bad_nullable,
                        );
                    }
                }
                _ => {}
            }
        }

        let app = test_helpers::create_test_app();
        let session_id = test_helpers::initialize_mcp_session(&app).await;
        let list_event =
            mcp_jsonrpc(&app, &session_id, 2, "tools/list", serde_json::json!({})).await;
        let result = list_event.get("result").expect("tools/list result missing");

        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools/list result missing tools array");

        let mut bad_formats = Vec::new();
        let mut bad_nullable = Vec::new();
        for tool in tools {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("<unknown>");

            for (schema_kind, key) in &[("input", "inputSchema"), ("output", "outputSchema")] {
                if let Some(schema) = tool.get(*key) {
                    collect_bad_formats(
                        name,
                        schema_kind,
                        "$",
                        schema,
                        &mut bad_formats,
                        &mut bad_nullable,
                    );
                }
            }
        }

        assert!(
            bad_formats.is_empty(),
            "Found nonstandard uint schema formats (prefer i64-compatible fields):\n  {}",
            bad_formats.join("\n  ")
        );

        assert!(
            bad_nullable.is_empty(),
            "Found nullable schema branches without explicit type (breaks strict clients):\n  {}",
            bad_nullable.join("\n  ")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_tools_schema_snapshot() {
        let app = test_helpers::create_test_app();
        let session_id = test_helpers::initialize_mcp_session(&app).await;
        let list_event = mcp_jsonrpc(&app, &session_id, 2, "tools/list", json!({})).await;
        let tools = list_event["result"]["tools"]
            .as_array()
            .expect("tools array");

        let mut signatures: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool["name"],
                    "input_schema": canonicalize_json(tool.get("inputSchema").unwrap_or(&Value::Null)),
                    "output_schema": canonicalize_json(tool.get("outputSchema").unwrap_or(&Value::Null)),
                })
            })
            .collect();
        signatures.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

        assert_json_snapshot!("mcp_tools_schema", signatures);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_contract_desktop_critical_tools_success_shapes() {
        let app = test_helpers::create_test_app_with_github_creds().await;
        let session_id = test_helpers::initialize_mcp_session(&app).await;

        let _ = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            serde_json::json!({
                "name": "contract-shape-project",
                "path": CONTRACT_PROJECT_PATH,
            }),
        )
        .await;

        let provider_catalog = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "provider_catalog",
            serde_json::json!({}),
        )
        .await;
        let providers = provider_catalog
            .get("providers")
            .and_then(Value::as_array)
            .expect("provider_catalog must return providers array");
        assert!(
            !providers.is_empty(),
            "provider_catalog providers should not be empty"
        );
        for provider in providers {
            assert!(provider.get("id").and_then(Value::as_str).is_some());
            assert!(provider.get("name").and_then(Value::as_str).is_some());
            assert!(provider.get("connected").and_then(Value::as_bool).is_some());
        }

        let credential_list = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "credential_list",
            serde_json::json!({}),
        )
        .await;
        assert!(
            credential_list
                .get("credentials")
                .and_then(Value::as_array)
                .is_some(),
            "credential_list must return credentials array"
        );

        let task_list = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_list",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH }),
        )
        .await;
        assert!(task_list.get("tasks").and_then(Value::as_array).is_some());
        assert!(
            task_list
                .get("total_count")
                .and_then(Value::as_i64)
                .is_some()
        );
        assert!(task_list.get("limit").and_then(Value::as_i64).is_some());
        assert!(task_list.get("offset").and_then(Value::as_i64).is_some());
        assert!(task_list.get("has_more").and_then(Value::as_bool).is_some());

        let epic_list = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "epic_list",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH }),
        )
        .await;
        assert!(epic_list.get("epics").and_then(Value::as_array).is_some());
        assert!(
            epic_list
                .get("total_count")
                .and_then(Value::as_i64)
                .is_some()
        );
        assert!(epic_list.get("limit").and_then(Value::as_i64).is_some());
        assert!(epic_list.get("offset").and_then(Value::as_i64).is_some());
        assert!(epic_list.get("has_more").and_then(Value::as_bool).is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_contract_task_and_epic_snapshot_shapes() {
        use insta::assert_json_snapshot;

        let app = test_helpers::create_test_app_with_github_creds().await;
        let session_id = test_helpers::initialize_mcp_session(&app).await;

        let _ = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            serde_json::json!({
                "name": "contract-task-epic-snapshots",
                "path": CONTRACT_PROJECT_PATH,
            }),
        )
        .await;

        let epic = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "epic_create",
            serde_json::json!({
                "project": CONTRACT_PROJECT_PATH,
                "title": "Snapshot Epic",
                "description": "Epic used for MCP snapshot contract testing"
            }),
        )
        .await;

        let task = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_create",
            serde_json::json!({
                "project": CONTRACT_PROJECT_PATH,
                "epic_id": epic["id"],
                "title": "Snapshot Task",
                "description": "Task used for MCP snapshot contract testing"
            }),
        )
        .await;

        let task_show = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_show",
            serde_json::json!({
                "project": CONTRACT_PROJECT_PATH,
                "id": task["id"],
            }),
        )
        .await;
        assert_json_snapshot!("task_show_response", task_show, {
            ".id" => "[UUID]",
            ".epic_id" => "[UUID]",
            ".project_id" => "[UUID]",
            ".short_id" => "[SHORT_ID]",
            ".created_at" => "[TIMESTAMP]",
            ".updated_at" => "[TIMESTAMP]"
        });

        let task_list = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_list",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH, "limit": 10, "offset": 0 }),
        )
        .await;
        assert_json_snapshot!("task_list_response", task_list, {
            ".tasks.**.id" => "[UUID]",
            ".tasks.**.epic_id" => "[UUID]",
            ".tasks.**.project_id" => "[UUID]",
            ".tasks.**.short_id" => "[SHORT_ID]",
            ".tasks.**.created_at" => "[TIMESTAMP]",
            ".tasks.**.updated_at" => "[TIMESTAMP]"
        });

        let task_count_plain = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_count",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH }),
        )
        .await;
        assert_json_snapshot!("task_count_plain_response", task_count_plain);

        let task_count_status = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_count",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH, "group_by": "status" }),
        )
        .await;
        assert_json_snapshot!("task_count_grouped_by_status_response", task_count_status);

        let task_count_priority = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_count",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH, "group_by": "priority" }),
        )
        .await;
        assert_json_snapshot!(
            "task_count_grouped_by_priority_response",
            task_count_priority
        );

        let _comment = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_comment_add",
            serde_json::json!({
                "project": CONTRACT_PROJECT_PATH,
                "id": task["id"],
                "actor_id": "u1",
                "actor_role": "user",
                "body": "snapshot comment"
            }),
        )
        .await;

        let task_activity = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_activity_list",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH, "id": task["id"] }),
        )
        .await;
        assert_json_snapshot!("task_activity_list_response", task_activity, {
            ".entries.**.id" => "[UUID]",
            ".entries.**.task_id" => "[UUID]",
            ".entries.**.actor_id" => "[UUID]",
            ".entries.**.created_at" => "[TIMESTAMP]",
            ".entries.**.timestamp" => "[TIMESTAMP]"
        });

        let epic_show = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "epic_show",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH, "id": epic["id"] }),
        )
        .await;
        assert_json_snapshot!("epic_show_response", epic_show, {
            ".id" => "[UUID]",
            ".project_id" => "[UUID]",
            ".short_id" => "[SHORT_ID]",
            ".created_at" => "[TIMESTAMP]",
            ".updated_at" => "[TIMESTAMP]"
        });

        let epic_list = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "epic_list",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH, "limit": 10, "offset": 0 }),
        )
        .await;
        assert_json_snapshot!("epic_list_response", epic_list, {
            ".epics.**.id" => "[UUID]",
            ".epics.**.project_id" => "[UUID]",
            ".epics.**.short_id" => "[SHORT_ID]",
            ".epics.**.created_at" => "[TIMESTAMP]",
            ".epics.**.updated_at" => "[TIMESTAMP]"
        });

        let blockers = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_blockers_list",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH, "id": task["id"] }),
        )
        .await;
        assert_json_snapshot!("task_blockers_list_response", blockers, {
            ".blockers.**.id" => "[UUID]",
            ".blockers.**.epic_id" => "[UUID]",
            ".blockers.**.project_id" => "[UUID]",
            ".blockers.**.created_at" => "[TIMESTAMP]",
            ".blockers.**.updated_at" => "[TIMESTAMP]"
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_contract_not_found_shapes_include_error_field() {
        let app = test_helpers::create_test_app();
        let session_id = test_helpers::initialize_mcp_session(&app).await;

        let _ = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            serde_json::json!({
                "name": "contract-not-found-project",
                "path": CONTRACT_PROJECT_PATH,
            }),
        )
        .await;

        let task_show = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "task_show",
            serde_json::json!({
                "project": CONTRACT_PROJECT_PATH,
                "id": "task-does-not-exist",
            }),
        )
        .await;
        assert!(
            task_show.get("error").and_then(Value::as_str).is_some(),
            "task_show not-found response must include error"
        );

        let epic_show = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "epic_show",
            serde_json::json!({
                "project": CONTRACT_PROJECT_PATH,
                "id": "epic-does-not-exist",
            }),
        )
        .await;
        assert!(
            epic_show.get("error").and_then(Value::as_str).is_some(),
            "epic_show not-found response must include error"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_contract_board_health_empty_board_returns_zero_counts() {
        let app = test_helpers::create_test_app();
        let session_id = test_helpers::initialize_mcp_session(&app).await;

        let _ = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            serde_json::json!({
                "name": "contract-board-health-empty",
                "path": CONTRACT_PROJECT_PATH,
            }),
        )
        .await;

        let health = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "board_health",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH }),
        )
        .await;

        assert_eq!(
            health["stale_tasks"]
                .as_array()
                .map(|v| v.len())
                .unwrap_or_default(),
            0
        );
        assert_eq!(
            health["epic_stats"]
                .as_array()
                .map(|v| v.len())
                .unwrap_or_default(),
            0
        );
        assert_eq!(
            health["review_queue"]
                .as_array()
                .map(|v| v.len())
                .unwrap_or_default(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_contract_board_health_response_shape_has_required_fields() {
        let app = test_helpers::create_test_app_with_github_creds().await;
        let session_id = test_helpers::initialize_mcp_session(&app).await;

        let _ = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            serde_json::json!({
                "name": "contract-board-health-shape",
                "path": CONTRACT_PROJECT_PATH,
            }),
        )
        .await;

        let health = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "board_health",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH }),
        )
        .await;

        assert!(health.get("stale_tasks").is_some());
        assert!(health.get("epic_stats").is_some());
        assert!(health.get("review_queue").is_some());
        assert!(health.get("stale_threshold_hours").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_contract_board_health_detects_stale_in_progress_task() {
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let state = AppState::new(db.clone(), cancel);
        let app = server::router(state);
        let session_id = test_helpers::initialize_mcp_session(&app).await;

        let _ = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            serde_json::json!({
                "name": "contract-board-health-stale",
                "path": CONTRACT_PROJECT_PATH,
            }),
        )
        .await;

        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let repo = djinn_db::TaskRepository::new(db.clone(), crate::events::EventBus::noop());
        repo.set_status(&task.id, "in_progress").await.unwrap();
        sqlx::query("UPDATE tasks SET updated_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1")
            .bind(&task.id)
            .execute(db.pool())
            .await
            .unwrap();

        let health = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "board_health",
            serde_json::json!({ "project": project.path }),
        )
        .await;

        assert!(
            health["stale_tasks"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0)
                >= 1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_contract_board_reconcile_requires_pool() {
        let app = test_helpers::create_test_app();
        let session_id = test_helpers::initialize_mcp_session(&app).await;

        let _ = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "project_add",
            serde_json::json!({
                "name": "contract-board-reconcile-empty",
                "path": CONTRACT_PROJECT_PATH,
            }),
        )
        .await;

        let result = test_helpers::mcp_call_tool(
            &app,
            &session_id,
            "board_reconcile",
            serde_json::json!({ "project": CONTRACT_PROJECT_PATH }),
        )
        .await;

        // board_reconcile requires the slot pool actor, which is not started in tests
        assert!(result.get("error").and_then(|v| v.as_str()).is_some());
    }

    #[test]
    fn mcp_tools_do_not_use_untyped_json_output() {
        // Bare serde_json::Value generates `true` as its JSON Schema, which
        // strict MCP clients (e.g. Claude Code) reject — breaking the entire
        // tool list.  Use AnyJson or ObjectJson wrappers instead.
        const FORBIDDEN: &[&str] = &[
            "Json<serde_json::Value>",
            "Vec<serde_json::Value>",
            "Option<serde_json::Value>",
            "Option<Vec<serde_json::Value>>",
        ];

        fn visit(dir: &Path, offenders: &mut Vec<String>) {
            let entries = std::fs::read_dir(dir).expect("read tools directory");
            for entry in entries {
                let entry = entry.expect("read entry");
                let path = entry.path();
                if path.is_dir() {
                    visit(&path, offenders);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                // Skip the json_object.rs helper (it wraps Value on purpose).
                if path
                    .file_name()
                    .map(|n| n == "json_object.rs")
                    .unwrap_or(false)
                {
                    continue;
                }
                let content = std::fs::read_to_string(&path).expect("read rust file");
                for pat in FORBIDDEN {
                    if content.contains(pat) {
                        offenders.push(format!("{}  (contains `{}`)", path.display(), pat));
                    }
                }
            }
        }

        let tools_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/djinn-mcp/src/tools");
        let mut offenders = Vec::new();
        visit(&tools_dir, &mut offenders);

        assert!(
            offenders.is_empty(),
            "Found bare serde_json::Value in MCP tool structs (use AnyJson/ObjectJson instead):\n  {}",
            offenders.join("\n  ")
        );
    }

    /// Unit test: verify the in-memory test DB has migrations applied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_db_has_tables() {
        let db = test_helpers::create_test_db();
        db.ensure_initialized().await.unwrap();

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(count, 1, "settings table should exist");
    }

    /// Demonstrates tokio::test(start_paused = true) for time-dependent logic.
    /// With start_paused, tokio::time::sleep completes instantly (time is virtual).
    #[tokio::test(start_paused = true)]
    async fn time_paused_pattern() {
        let before = tokio::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let elapsed = before.elapsed();

        // With start_paused, the 60s sleep advances virtual time instantly.
        assert_eq!(elapsed.as_secs(), 60);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_settings_rejects_disconnected_model_priority_provider() {
        let db = test_helpers::create_test_db();
        let state = AppState::new(db, CancellationToken::new());

        let settings = DjinnSettings {
            model_priority: Some(
                [(
                    "worker".into(),
                    vec!["nvidia/moonshotai/kimi-k2-instruct".into()],
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        };

        let err = state
            .apply_settings(&settings)
            .await
            .expect_err("should reject disconnected provider");

        assert!(err.contains("disconnected providers"));
        assert!(err.contains("nvidia"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_settings_accepts_connected_model_priority_provider() {
        let db = test_helpers::create_test_db();
        let state = AppState::new(db, CancellationToken::new());

        let cred_repo = CredentialRepository::new(state.db().clone(), state.event_bus());
        cred_repo
            .set("synthetic", "SYNTHETIC_API_KEY", "sk-test")
            .await
            .unwrap();

        let settings = DjinnSettings {
            model_priority: Some(
                [(
                    "worker".into(),
                    vec!["synthetic/hf:moonshotai/Kimi-K2.5".into()],
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        };

        state
            .apply_settings(&settings)
            .await
            .expect("connected provider should be accepted");
    }
}
