use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::IntoResponse;
use djinn_core::auth_context::{SESSION_USER_ID, SESSION_USER_TOKEN};
use serde_json::Value;

use super::AppState;
use super::auth::authenticate;

pub(super) async fn mcp_handler(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let headers = req.headers().clone();
    let worktree_root = req
        .headers()
        .get("x-djinn-worktree-root")
        .and_then(|value| value.to_str().ok())
        .map(std::path::PathBuf::from)
        .filter(|path| path.join(".git").exists());

    // Resolve the authenticated user's GitHub access token *and* stable
    // `users.id` (if any), and thread both through MCP dispatch via tokio
    // task-locals:
    //   - `SESSION_USER_TOKEN` — read by tools that call user-scoped
    //     GitHub endpoints via `current_user_token()`.
    //   - `SESSION_USER_ID`    — read by repository inserts so new rows
    //     on `tasks`, `epics`, and `sessions` carry `created_by_user_id`
    //     attribution via `current_user_id()`.
    let (user_token, user_id): (Option<String>, Option<String>) =
        match authenticate(&state, &headers).await {
            Ok(Some(user)) => (Some(user.github_access_token), Some(user.id)),
            Ok(None) => (None, None),
            Err(err) => {
                tracing::warn!(error = %err, "mcp_handler: authenticate failed; proceeding unauth");
                (None, None)
            }
        };

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

    // `notifications/initialized` has no response body — handle it directly.
    if payload.get("method").and_then(Value::as_str) == Some("notifications/initialized") {
        return StatusCode::ACCEPTED.into_response();
    }

    let response = SESSION_USER_TOKEN
        .scope(
            user_token,
            SESSION_USER_ID.scope(user_id, dispatch(state.clone(), worktree_root, payload.clone())),
        )
        .await;

    let mut resp = axum::Json(response).into_response();
    if payload.get("method").and_then(Value::as_str) == Some("initialize") {
        resp.headers_mut()
            .insert("mcp-session-id", HeaderValue::from_static("test-session"));
    }
    resp
}

async fn dispatch(
    state: AppState,
    worktree_root: Option<std::path::PathBuf>,
    payload: Value,
) -> Value {
    match payload.get("method").and_then(Value::as_str) {
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
    }
}
