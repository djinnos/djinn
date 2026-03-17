use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Serialize;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use djinn_db::default_db_path;
use crate::server::AppState;

pub async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events().subscribe();
    let cancel = state.cancel().clone();
    tracing::info!("SSE: new client connected");

    let event_stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(envelope) => {
            let event_name = format!("{}.{}", envelope.entity_type, envelope.action);
            if envelope.entity_type == "session" {
                tracing::debug!(event_name = %event_name, "SSE: sending session event to client");
            }
            serde_json::to_string(&envelope)
                .ok()
                .map(|data| Ok::<Event, Infallible>(Event::default().event(event_name).data(data)))
        }
        Err(e) => {
            tracing::warn!("SSE subscriber lagged: {e}");
            Some(Ok(Event::default().event("lagged").data("{}")))
        }
    });

    let stream = futures::StreamExt::take_until(event_stream, cancel.cancelled_owned());
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Serialize)]
pub struct DbInfo {
    path: String,
    wsl: bool,
    direct_access_likely: bool,
}

pub async fn db_info_handler() -> axum::Json<DbInfo> {
    let path = default_db_path();
    let wsl = is_wsl();
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

#[cfg(test)]
mod tests {
    use crate::events::DjinnEventEnvelope;

    #[test]
    fn sse_event_name_uses_entity_type_and_action() {
        let envelope = DjinnEventEnvelope::task_deleted("t1");
        let event_name = format!("{}.{}", envelope.entity_type, envelope.action);
        assert_eq!(event_name, "task.deleted");
    }

    #[test]
    fn sse_payload_serializes_djinn_event_shape() {
        let envelope = DjinnEventEnvelope::task_deleted("t1");
        let json = serde_json::to_value(&envelope).expect("serialize envelope");

        assert_eq!(
            json.get("entity_type").and_then(|v| v.as_str()),
            Some("task")
        );
        assert_eq!(json.get("action").and_then(|v| v.as_str()), Some("deleted"));
        assert_eq!(
            json.get("payload")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str()),
            Some("t1")
        );
    }
}
