use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Serialize;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::db::connection::default_db_path;
use crate::events::DjinnEvent;
use crate::server::AppState;

// ── SSE envelope ─────────────────────────────────────────────────────────────

/// Wire format sent over the SSE stream for every repository mutation.
///
/// `type` + `action` form the SSE event name (e.g. `project.created`).
/// `data` is present for creates/updates; `id` for deletes.
/// `project_id` is present for scoped events like `git_settings.updated`.
#[derive(Serialize)]
struct Envelope {
    #[serde(rename = "type")]
    entity_type: &'static str,
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
}

fn to_envelope(evt: DjinnEvent) -> Envelope {
    match evt {
        DjinnEvent::SettingUpdated(v) => Envelope {
            entity_type: "setting",
            action: "updated",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::ProjectCreated(v) => Envelope {
            entity_type: "project",
            action: "created",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::ProjectUpdated(v) => Envelope {
            entity_type: "project",
            action: "updated",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::ProjectDeleted { id } => Envelope {
            entity_type: "project",
            action: "deleted",
            data: None,
            id: Some(id),
            project_id: None,
        },
        DjinnEvent::ProjectConfigUpdated { project_id, config } => Envelope {
            entity_type: "project_config",
            action: "updated",
            data: serde_json::to_value(config).ok(),
            id: None,
            project_id: Some(project_id),
        },
        DjinnEvent::EpicCreated(v) => Envelope {
            entity_type: "epic",
            action: "created",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::EpicUpdated(v) => Envelope {
            entity_type: "epic",
            action: "updated",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::EpicDeleted { id } => Envelope {
            entity_type: "epic",
            action: "deleted",
            data: None,
            id: Some(id),
            project_id: None,
        },
        DjinnEvent::TaskCreated { task, .. } => Envelope {
            entity_type: "task",
            action: "created",
            data: serde_json::to_value(task).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::TaskUpdated { task, .. } => Envelope {
            entity_type: "task",
            action: "updated",
            data: serde_json::to_value(task).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::TaskDeleted { id } => Envelope {
            entity_type: "task",
            action: "deleted",
            data: None,
            id: Some(id),
            project_id: None,
        },
        DjinnEvent::NoteCreated(v) => Envelope {
            entity_type: "note",
            action: "created",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::NoteUpdated(v) => Envelope {
            entity_type: "note",
            action: "updated",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::NoteDeleted { id } => Envelope {
            entity_type: "note",
            action: "deleted",
            data: None,
            id: Some(id),
            project_id: None,
        },
        DjinnEvent::GitSettingsUpdated {
            project_id,
            settings,
        } => Envelope {
            entity_type: "git_settings",
            action: "updated",
            data: serde_json::to_value(settings).ok(),
            id: None,
            project_id: Some(project_id),
        },
        DjinnEvent::CredentialCreated(v) => Envelope {
            entity_type: "credential",
            action: "created",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::CredentialUpdated(v) => Envelope {
            entity_type: "credential",
            action: "updated",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::CredentialDeleted { id } => Envelope {
            entity_type: "credential",
            action: "deleted",
            data: None,
            id: Some(id),
            project_id: None,
        },
        DjinnEvent::SessionCreated(v) => Envelope {
            entity_type: "session",
            action: "started",
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::SessionUpdated(v) => Envelope {
            entity_type: "session",
            action: match v.status.as_str() {
                "completed" => "completed",
                "interrupted" => "interrupted",
                "failed" => "failed",
                _ => "updated",
            },
            data: serde_json::to_value(v).ok(),
            id: None,
            project_id: None,
        },
        DjinnEvent::SessionTokenUpdate {
            session_id,
            task_id,
            tokens_in,
            tokens_out,
            context_window,
            usage_pct,
        } => Envelope {
            entity_type: "session",
            action: "token_update",
            data: Some(serde_json::json!({
                "session_id": session_id,
                "task_id": task_id,
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
                "context_window": context_window,
                "usage_pct": usage_pct,
            })),
            id: None,
            project_id: None,
        },
        DjinnEvent::SessionMessage {
            session_id,
            task_id,
            agent_type,
            message,
        } => Envelope {
            entity_type: "session",
            action: "message",
            data: Some(serde_json::json!({
                "session_id": session_id,
                "task_id": task_id,
                "agent_type": agent_type,
                "message": message,
            })),
            id: None,
            project_id: None,
        },
        DjinnEvent::SessionMessageInserted {
            session_id,
            task_id,
            role,
        } => Envelope {
            entity_type: "session_message",
            action: "inserted",
            data: Some(serde_json::json!({
                "session_id": session_id,
                "task_id": task_id,
                "role": role,
            })),
            id: None,
            project_id: None,
        },
        DjinnEvent::SyncCompleted {
            channel,
            direction,
            count,
            error,
        } => Envelope {
            entity_type: "sync",
            action: "completed",
            data: Some(serde_json::json!({
                "channel": channel,
                "direction": direction,
                "count": count,
                "error": error,
            })),
            id: None,
            project_id: None,
        },
        DjinnEvent::ProjectHealthChanged {
            project_id,
            healthy,
            error,
        } => Envelope {
            entity_type: "project",
            action: if healthy { "health_ok" } else { "health_error" },
            data: Some(
                serde_json::json!({ "project_id": project_id, "healthy": healthy, "error": error }),
            ),
            id: Some(project_id),
            project_id: None,
        },
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `GET /events` — Real-time SSE change feed.
///
/// Streams every repository mutation as a Server-Sent Event.  Event name is
/// `<type>.<action>` (e.g. `project.created`).  Body is a JSON envelope with
/// `type`, `action`, and either `data` (full entity) or `id` (for deletes).
///
/// The stream never terminates normally — clients stay connected and receive
/// all future mutations.  If the internal broadcast buffer overflows the client
/// receives a synthetic `lagged` event and should re-fetch state via MCP tools
/// before continuing.
///
/// Keep-alive comment frames are sent every 15 s so load-balancers / proxies
/// do not close idle connections.
pub async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events().subscribe();
    let cancel = state.cancel().clone();

    let event_stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(evt) => {
            let envelope = to_envelope(evt);
            let event_name = format!("{}.{}", envelope.entity_type, envelope.action);
            serde_json::to_string(&envelope)
                .ok()
                .map(|data| Ok::<Event, Infallible>(Event::default().event(event_name).data(data)))
        }
        Err(e) => {
            // Receiver lagged — some events were dropped from the buffer.
            // Notify the client so it can re-fetch and re-subscribe.
            tracing::warn!("SSE subscriber lagged: {e}");
            Some(Ok(Event::default().event("lagged").data("{}")))
        }
    });

    // End the SSE stream when the server is shutting down so axum's graceful
    // shutdown doesn't hang waiting for long-lived connections to close.
    let stream = futures::StreamExt::take_until(event_stream, cancel.cancelled_owned());

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── DB info ───────────────────────────────────────────────────────────────────

/// Response body for `GET /db-info`.
#[derive(Serialize)]
pub struct DbInfo {
    /// Absolute path to the SQLite database file.
    ///
    /// The desktop can open this path with a read-only rusqlite connection
    /// (WAL mode allows concurrent reads alongside the server writer).
    path: String,

    /// `true` when the server detects it is running inside WSL.
    wsl: bool,

    /// `true` when direct SQLite file access is expected to work from the
    /// desktop process.
    ///
    /// In WSL2, the Windows desktop can access Linux-FS paths via the
    /// `\\wsl$\<Distro>` UNC path.  `false` only when the DB path is on a
    /// Windows-mounted drive (`/mnt/c`, `/mnt/d`, …), which we disallow by
    /// convention (ADR-006).  When `false` the desktop should fall back to
    /// MCP tool reads instead of direct file access.
    direct_access_likely: bool,
}

/// `GET /db-info` — Expose DB path and WSL direct-access capability hints.
///
/// The desktop calls this on connect to decide whether it can open the DB
/// file directly (local mode) or must use MCP tool reads (WSL-04).
pub async fn db_info_handler() -> axum::Json<DbInfo> {
    let path = default_db_path();
    let wsl = is_wsl();
    // A DB on a Windows-mounted drive (/mnt/x/) cannot be opened by the
    // server's rusqlite anyway (ADR-006), so direct_access_likely is only
    // false in that degenerate case.
    let direct_access_likely = !path
        .components()
        .take(3)
        .any(|c| c.as_os_str().len() == 1 && path.starts_with("/mnt/"));

    axum::Json(DbInfo {
        path: path.to_string_lossy().into_owned(),
        wsl,
        direct_access_likely,
    })
}

fn is_wsl() -> bool {
    std::env::var("WSL_DISTRO_NAME").is_ok()
        || std::env::var("WSL_INTEROP").is_ok()
        || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::test_helpers;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_returns_200_with_sse_content_type() {
        let app = test_helpers::create_test_app();

        let req = axum::http::Request::builder()
            .uri("/events")
            .header("Accept", "text/event-stream")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("text/event-stream"), "got: {ct}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn db_info_returns_path_and_flags() {
        let app = test_helpers::create_test_app();

        let req = axum::http::Request::builder()
            .uri("/db-info")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), 200);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert!(json["path"].is_string(), "path must be a string");
        assert!(json["wsl"].is_boolean(), "wsl must be a boolean");
        assert!(
            json["direct_access_likely"].is_boolean(),
            "direct_access_likely must be a boolean"
        );
    }
}
