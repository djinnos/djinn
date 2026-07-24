use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    ContainerName, EvictionReason, EvictionTransition, FilesystemCapacity, LogStore, Namespace,
    PodUid, StoreError, StreamIdentity, WritableState,
};

/// The request cap applies before deserializing so clients cannot make the
/// rotator retain arbitrarily large malformed records in memory.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The narrow store surface used by the HTTP process. Keeping it typed here
/// ensures the handler has no alternate write path around `LogStore::append`.
pub trait StoreBackend: Send + Sync {
    fn append(&self, stream: &StreamIdentity, record: &Value) -> Result<u64, StoreError>;
    fn writable_state(&self) -> Result<WritableState, StoreError>;
    fn eviction_transitions(&self) -> Vec<EvictionTransition>;
}

impl<C, Z, F> StoreBackend for LogStore<C, Z, F>
where
    C: crate::Clock + Send + Sync,
    Z: crate::Compressor + Send + Sync,
    F: FilesystemCapacity + Send + Sync,
{
    fn append(&self, stream: &StreamIdentity, record: &Value) -> Result<u64, StoreError> {
        self.append(stream, record)
    }

    fn writable_state(&self) -> Result<WritableState, StoreError> {
        self.writable_state()
    }

    fn eviction_transitions(&self) -> Vec<EvictionTransition> {
        self.eviction_transitions()
    }
}

#[derive(Clone)]
pub struct AppState {
    store: Arc<dyn StoreBackend>,
    live: Arc<AtomicBool>,
}

impl AppState {
    pub fn new(store: Arc<dyn StoreBackend>) -> Self {
        Self {
            store,
            live: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Used by orderly shutdown and lifecycle fixtures to prevent health from
    /// claiming that a stopped ingest state machine is still accepting writes.
    pub fn mark_unhealthy(&self) {
        self.live.store(false, Ordering::Release);
    }
}

pub fn router(store: Arc<impl StoreBackend + 'static>) -> Router {
    let state = AppState::new(store);
    ingest_router(state)
}

pub fn ingest_router(state: AppState) -> Router {
    Router::new()
        .route("/ingest", post(ingest))
        .route("/healthz", get(health))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_RECORD_BYTES))
        .with_state(state)
}

pub fn metrics_router(state: AppState) -> Router {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/healthz", get(health))
        .with_state(state)
}

async fn ingest(State(state): State<AppState>, body: Bytes) -> Response {
    // DefaultBodyLimit rejects larger bodies before this handler. Keep this
    // check for direct/unit invocation and future router composition.
    if body.len() > MAX_RECORD_BYTES {
        return client_error(StatusCode::PAYLOAD_TOO_LARGE, "record too large");
    }
    let record: Value = match serde_json::from_slice(&body) {
        Ok(record) => record,
        Err(_) => return client_error(StatusCode::BAD_REQUEST, "malformed record"),
    };
    let stream = match validate_record(&record) {
        Ok(stream) => stream,
        Err(()) => return client_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid record"),
    };
    match state.store.append(&stream, &record) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        // Reserve is checked by the store before it creates a stream directory
        // or opens a segment, so 507 has no partial append and is safe to retry.
        Err(StoreError::ReserveExhausted) => client_error(
            StatusCode::INSUFFICIENT_STORAGE,
            "store temporarily unwritable",
        ),
        Err(_) => client_error(StatusCode::INTERNAL_SERVER_ERROR, "store failure"),
    }
}

async fn health(State(state): State<AppState>) -> Response {
    if state.live.load(Ordering::Acquire) && state.store.writable_state().is_ok() {
        StatusCode::OK.into_response()
    } else {
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    }
}

async fn metrics(State(state): State<AppState>) -> Response {
    let writable = state
        .store
        .writable_state()
        .map(|state| u8::from(state.writable))
        .unwrap_or(0);
    let transitions = state.store.eviction_transitions();
    let stream = count_reason(&transitions, EvictionReason::StreamQuota);
    let global = count_reason(&transitions, EvictionReason::GlobalQuota);
    let reserve = count_reason(&transitions, EvictionReason::Reserve);
    let text = format!(
        "# HELP djinn_log_store_writable Whether the log store may accept appends.\n\
# TYPE djinn_log_store_writable gauge\n\
djinn_log_store_writable {writable}\n\
# HELP djinn_log_store_evictions_total Log-store eviction and reserve state transitions.\n\
# TYPE djinn_log_store_evictions_total counter\n\
djinn_log_store_evictions_total{{reason=\"stream\"}} {stream}\n\
djinn_log_store_evictions_total{{reason=\"global\"}} {global}\n\
djinn_log_store_evictions_total{{reason=\"reserve\"}} {reserve}\n\
# HELP djinn_log_rotator_build_info Build information for the log rotator.\n\
# TYPE djinn_log_rotator_build_info gauge\n\
djinn_log_rotator_build_info{{version=\"{BUILD_VERSION}\"}} 1\n"
    );
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
        .into_response()
}

fn count_reason(transitions: &[EvictionTransition], reason: EvictionReason) -> usize {
    transitions
        .iter()
        .filter(|transition| transition.reason == reason)
        .count()
}

fn client_error(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        message,
    )
        .into_response()
}

fn validate_record(record: &Value) -> Result<StreamIdentity, ()> {
    let object = record.as_object().ok_or(())?;
    let timestamp = required_string(object, "timestamp")?;
    OffsetDateTime::parse(timestamp, &Rfc3339).map_err(|_| ())?;
    let namespace = Namespace::new(required_string(object, "namespace")?).map_err(|_| ())?;
    validate_pod_name(required_string(object, "pod_name")?)?;
    let pod_uid = PodUid::new(required_string(object, "pod_uid")?).map_err(|_| ())?;
    let container = ContainerName::new(required_string(object, "container")?).map_err(|_| ())?;
    match required_string(object, "stream")? {
        "stdout" | "stderr" => {}
        _ => return Err(()),
    }
    if required_string(object, "message")?.is_empty() {
        return Err(());
    }
    Ok(StreamIdentity::new(namespace, pod_uid, container))
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, ()> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or(())
}

fn validate_pod_name(value: &str) -> Result<(), ()> {
    if value.len() > 253 || value.is_empty() {
        return Err(());
    }
    value.split('.').try_for_each(|label| {
        let valid = !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        valid.then_some(()).ok_or(())
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;

    #[derive(Default)]
    struct FixtureStore {
        records: Mutex<Vec<Value>>,
        reserve: AtomicBool,
        transitions: Mutex<Vec<EvictionTransition>>,
    }
    impl StoreBackend for FixtureStore {
        fn append(&self, _: &StreamIdentity, record: &Value) -> Result<u64, StoreError> {
            if self.reserve.load(Ordering::Acquire) {
                return Err(StoreError::ReserveExhausted);
            }
            self.records.lock().unwrap().push(record.clone());
            Ok(1)
        }
        fn writable_state(&self) -> Result<WritableState, StoreError> {
            let writable = !self.reserve.load(Ordering::Acquire);
            Ok(WritableState {
                writable,
                required_reserve_bytes: 1,
                available_bytes: u64::from(u8::from(writable)),
            })
        }
        fn eviction_transitions(&self) -> Vec<EvictionTransition> {
            self.transitions.lock().unwrap().clone()
        }
    }

    fn record() -> &'static str {
        r#"{"timestamp":"2026-07-24T12:00:00Z","namespace":"prod","pod_name":"api-0","pod_uid":"550e8400-e29b-41d4-a716-446655440000","container":"api","stream":"stdout","message":"hello"}"#
    }

    #[tokio::test]
    async fn accepts_one_complete_validated_record() {
        let store = Arc::new(FixtureStore::default());
        let app = router(store.clone());
        let response = app
            .oneshot(Request::post("/ingest").body(Body::from(record())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.records.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_malformed_missing_and_oversized_records() {
        let app = router(Arc::new(FixtureStore::default()));
        for body in ["{", "{}"] {
            let response = app
                .clone()
                .oneshot(Request::post("/ingest").body(Body::from(body)).unwrap())
                .await
                .unwrap();
            assert!(response.status().is_client_error());
        }
        let response = app
            .oneshot(
                Request::post("/ingest")
                    .body(Body::from(vec![b'x'; MAX_RECORD_BYTES + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn reserve_returns_507_without_append_then_recovers() {
        let store = Arc::new(FixtureStore::default());
        store.reserve.store(true, Ordering::Release);
        let app = router(store.clone());
        let response = app
            .clone()
            .oneshot(Request::post("/ingest").body(Body::from(record())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INSUFFICIENT_STORAGE);
        assert!(store.records.lock().unwrap().is_empty());
        store.reserve.store(false, Ordering::Release);
        let response = app
            .oneshot(Request::post("/ingest").body(Body::from(record())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn health_metrics_and_build_version_follow_state() {
        let store = Arc::new(FixtureStore::default());
        store.transitions.lock().unwrap().push(EvictionTransition {
            reason: EvictionReason::Reserve,
            logical_bytes: 0,
        });
        let state = AppState::new(store);
        let app = metrics_router(state.clone());
        assert_eq!(
            app.clone()
                .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let response = app
            .clone()
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("djinn_log_store_evictions_total{reason=\"reserve\"} 1"));
        assert!(text.contains(BUILD_VERSION));
        state.mark_unhealthy();
        assert_eq!(
            app.oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn release_image_is_non_root_and_never_uses_latest() {
        let dockerfile = include_str!("../../../../deploy/docker/Dockerfile.log-rotator");
        assert!(dockerfile.contains("USER 10002:10002"));
        assert!(!dockerfile.contains(":latest"));
    }
}
