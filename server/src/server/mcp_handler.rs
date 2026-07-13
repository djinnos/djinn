use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use djinn_core::auth_context::{
    REVISION_CALLER_CONTEXT, SESSION_USER_ID, SESSION_USER_TOKEN, TrustedRevisionCallerContext,
};
use serde_json::Value;

use super::AppState;
use super::oauth::{self, AuthOutcome};

pub(super) async fn mcp_handler(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let headers = req.headers().clone();
    let worktree_root = req
        .headers()
        .get("x-djinn-worktree-root")
        .and_then(|value| value.to_str().ok())
        .map(std::path::PathBuf::from)
        .filter(|path| path.join(".git").exists());

    // Resolve the request to a user via EITHER the existing `djinn_session`
    // cookie OR an `Authorization: Bearer <token>` MCP access token, threading
    // both the GitHub access token and the stable `users.id` through MCP
    // dispatch via tokio task-locals:
    //   - `SESSION_USER_TOKEN` — read by tools that call user-scoped GitHub
    //     endpoints via `current_user_token()`. For bearer-auth requests this
    //     is a best-effort lookup of the user's freshest live GitHub session
    //     token, and may be `None` (acceptable for v1 — most task tools only
    //     need `user_id` for attribution).
    //   - `SESSION_USER_ID`    — read by repository inserts so new rows on
    //     `tasks`, `epics`, and `sessions` carry `created_by_user_id`.
    //
    // Auth requirement (cut-over): MCP now requires authentication *when the
    // GitHub App is configured* (the multi-user deployment shape). Local
    // single-user dev with no GitHub App configured keeps the historical
    // unauthenticated path so it still works without the OAuth round-trip.
    let auth_required = state.app_config().await.is_some();
    let (user_token, user_id): (Option<String>, Option<String>) =
        match oauth::resolve_request_user(&state, &headers).await {
            AuthOutcome::User(u) => (u.github_access_token, Some(u.user_id)),
            AuthOutcome::Missing | AuthOutcome::InvalidBearer if auth_required => {
                return unauthorized_response();
            }
            // Unauthenticated local-dev path: proceed without attribution.
            AuthOutcome::Missing | AuthOutcome::InvalidBearer => (None, None),
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
            SESSION_USER_ID.scope(
                user_id.clone(),
                REVISION_CALLER_CONTEXT.scope(
                    user_id.and_then(TrustedRevisionCallerContext::authenticated_human),
                    dispatch(state.clone(), worktree_root, payload.clone()),
                ),
            ),
        )
        .await;

    let mut resp = axum::Json(response).into_response();
    if payload.get("method").and_then(Value::as_str) == Some("initialize") {
        resp.headers_mut()
            .insert("mcp-session-id", HeaderValue::from_static("test-session"));
    }
    resp
}

/// 401 with the RFC 9728 `WWW-Authenticate` challenge pointing MCP clients at
/// the protected-resource metadata so they can discover the AS and begin the
/// OAuth connect flow.
fn unauthorized_response() -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    if let Ok(hv) = HeaderValue::from_str(&oauth::www_authenticate_value()) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, hv);
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
            match djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state())
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
            let tools = djinn_control_plane::server::DjinnMcpServer::new(state.mcp_state())
                .all_tool_schemas();
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
