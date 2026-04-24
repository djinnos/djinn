//! HTTP endpoints for chat-session CRUD.
//!
//! These are the sibling endpoints to `POST /api/chat/completions`:
//!   * `GET    /api/chat/sessions`             — list
//!   * `GET    /api/chat/sessions/:id/messages` — message history
//!   * `PATCH  /api/chat/sessions/:id`         — rename
//!   * `DELETE /api/chat/sessions/:id`         — delete (cascades via FK)
//!
//! They are all scoped to `agent_type = 'chat'`; non-chat (worker,
//! planner, etc.) sessions remain invisible here.  The handler path
//! (`handler.rs`) is responsible for upserting the session row and
//! persisting messages; this module is read-mostly.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, patch},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::server::AppState;
use djinn_db::{ProjectRepository, SessionMessageRepository, SessionRepository};

pub(in crate::server) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/chat/sessions", get(list_chat_sessions))
        .route("/api/chat/sessions/{id}", patch(rename_chat_session))
        .route("/api/chat/sessions/{id}", delete(delete_chat_session))
        .route(
            "/api/chat/sessions/{id}/messages",
            get(list_chat_session_messages),
        )
}

// ─── Response DTOs ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatSessionDTO {
    id: String,
    title: String,
    project_slug: Option<String>,
    model: Option<String>,
    /// Unix milliseconds.  `created_at` mirrors `sessions.started_at`.
    created_at: i64,
    updated_at: i64,
}

#[derive(Serialize)]
struct ListChatSessionsResponse {
    sessions: Vec<ChatSessionDTO>,
}

#[derive(Serialize)]
struct ChatToolCallDTO {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
}

#[derive(Serialize)]
struct ChatMessageDTO {
    id: String,
    role: String,
    /// Content is passed through as the stored JSON array of content
    /// blocks when the client-side schema is structured, or as a plain
    /// string when the stored payload happens to be a single text
    /// block.  The UI handles both shapes.
    content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachments: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ChatToolCallDTO>,
    created_at: i64,
}

#[derive(Serialize)]
struct ListChatMessagesResponse {
    messages: Vec<ChatMessageDTO>,
}

// ─── Helpers ───────────────────────────────────────────────────────────

/// Best-effort ISO-8601-or-MySQL-datetime → unix milliseconds.
/// Chat row timestamps are always the ISO form stamped by the migration
/// 1 default (`DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')`), but
/// `session_messages.created_at` has historically been a MySQL
/// `DATETIME`-format string too; accept both.
fn parse_to_millis(raw: &str) -> i64 {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    if let Ok(dt) = OffsetDateTime::parse(raw, &Rfc3339) {
        return (dt.unix_timestamp_nanos() / 1_000_000) as i64;
    }
    // MySQL default `YYYY-MM-DD HH:MM:SS[.fff]`.
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second][optional [.[subsecond]]]"
    );
    if let Ok(dt) = time::PrimitiveDateTime::parse(raw, fmt) {
        return (dt.assume_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    }
    0
}

// ─── GET /api/chat/sessions ────────────────────────────────────────────

async fn list_chat_sessions(
    State(state): State<AppState>,
) -> Result<Json<ListChatSessionsResponse>, (StatusCode, String)> {
    let repo = SessionRepository::new(state.db().clone(), state.event_bus());
    let sessions = repo
        .list_chat_sessions()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Chat sessions are project-less by invariant, so `project_slug` is
    // always None — but we resolve a slug for any non-null project_id
    // defensively (in case future non-chat rows leak into the filter).
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());

    // Fetch the latest message timestamp per session for `updated_at`.
    let ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
    let msg_repo = SessionMessageRepository::new(state.db().clone(), state.event_bus());
    let latest_per_session = {
        let rows = msg_repo
            .load_for_sessions(&ids)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (sid, _role, _content, created_at) in rows {
            // load_for_sessions is ASC-ordered so the final write wins.
            map.insert(sid, created_at);
        }
        map
    };

    let mut out = Vec::with_capacity(sessions.len());
    for s in sessions {
        let project_slug = match &s.project_id {
            Some(pid) => project_repo
                .get(pid)
                .await
                .ok()
                .flatten()
                .map(|p| format!("{}/{}", p.github_owner, p.github_repo)),
            None => None,
        };
        let created_at = parse_to_millis(&s.started_at);
        let updated_at = latest_per_session
            .get(&s.id)
            .map(|t| parse_to_millis(t))
            .unwrap_or(created_at);
        out.push(ChatSessionDTO {
            id: s.id,
            title: s.title.unwrap_or_else(|| "New Chat".to_string()),
            project_slug,
            model: Some(s.model_id),
            created_at,
            updated_at,
        });
    }
    // Explicit updated_at DESC sort overrides the repository's COALESCE-
    // based ordering once the per-message timestamps are known.
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));

    Ok(Json(ListChatSessionsResponse { sessions: out }))
}

// ─── GET /api/chat/sessions/:id/messages ───────────────────────────────

async fn list_chat_session_messages(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ListChatMessagesResponse>, (StatusCode, String)> {
    let repo = SessionRepository::new(state.db().clone(), state.event_bus());
    let session = repo
        .get_chat_session(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("chat session not found: {id}")))?;

    let msg_repo = SessionMessageRepository::new(state.db().clone(), state.event_bus());
    let rows = sqlx::query_as::<_, (String, String, String, Option<i64>, String)>(
        "SELECT id, role, content_json, token_count, created_at \
         FROM session_messages WHERE session_id = ? ORDER BY created_at ASC",
    )
    .bind(&session.id)
    .fetch_all(state.db().pool())
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = msg_repo; // silence unused-var when only used for naming

    let mut messages = Vec::with_capacity(rows.len());
    for (msg_id, role, content_json, _tokens, created_at) in rows {
        let content_value: Value = serde_json::from_str(&content_json).unwrap_or(Value::Null);
        let tool_calls = extract_tool_calls(&content_value);
        let attachments = extract_attachments(&content_value);
        let content = surface_content(&content_value);
        messages.push(ChatMessageDTO {
            id: msg_id,
            role,
            content,
            attachments,
            tool_calls,
            created_at: parse_to_millis(&created_at),
        });
    }

    Ok(Json(ListChatMessagesResponse { messages }))
}

/// Pull `{ name, input }` entries out of any `tool_use` content blocks.
/// Used by the UI to render an inspectable tool-call card.
fn extract_tool_calls(content: &Value) -> Vec<ChatToolCallDTO> {
    let Some(arr) = content.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in arr {
        if block.get("type").and_then(Value::as_str) == Some("tool_use") {
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let input = block.get("input").cloned();
            // `success` is not known at persist-time for tool_use blocks
            // — it belongs on the follow-up tool_result.  Leave it None
            // and let the UI merge results if it wants.
            out.push(ChatToolCallDTO {
                name,
                success: None,
                input,
            });
        }
    }
    out
}

/// Extract non-text attachment blocks (image/document) for the
/// optional `attachments` field.  Text and tool_use blocks stay in
/// `content`.
fn extract_attachments(content: &Value) -> Option<Value> {
    let arr = content.as_array()?;
    let filtered: Vec<Value> = arr
        .iter()
        .filter(|b| {
            matches!(
                b.get("type").and_then(Value::as_str),
                Some("image") | Some("document")
            )
        })
        .cloned()
        .collect();
    if filtered.is_empty() {
        None
    } else {
        Some(Value::Array(filtered))
    }
}

/// Simplify the stored content array for the UI:
/// * Single text block → plain string.
/// * Anything else → pass the array through verbatim.
fn surface_content(content: &Value) -> Value {
    if let Some(arr) = content.as_array()
        && arr.len() == 1
        && arr[0].get("type").and_then(Value::as_str) == Some("text")
        && let Some(t) = arr[0].get("text").and_then(Value::as_str)
    {
        return Value::String(t.to_string());
    }
    content.clone()
}

// ─── PATCH /api/chat/sessions/:id ──────────────────────────────────────

#[derive(Deserialize)]
struct PatchSessionBody {
    title: String,
}

async fn rename_chat_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchSessionBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let trimmed = body.title.trim();
    if trimmed.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "title must not be empty".into()));
    }
    if trimmed.chars().count() > 255 {
        return Err((StatusCode::BAD_REQUEST, "title too long (max 255)".into()));
    }
    let repo = SessionRepository::new(state.db().clone(), state.event_bus());
    // Ensure the target exists (and is chat-typed) before writing.
    if repo
        .get_chat_session(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .is_none()
    {
        return Err((StatusCode::NOT_FOUND, format!("chat session not found: {id}")));
    }
    repo.update_chat_title(&id, trimmed)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

// ─── DELETE /api/chat/sessions/:id ─────────────────────────────────────

async fn delete_chat_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let repo = SessionRepository::new(state.db().clone(), state.event_bus());
    let affected = repo
        .delete_chat_session(&id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if affected == 0 {
        return Err((StatusCode::NOT_FOUND, format!("chat session not found: {id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}
