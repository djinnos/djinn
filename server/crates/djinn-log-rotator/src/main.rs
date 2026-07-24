use std::sync::Arc;

use djinn_log_rotator::{AppState, LogStore, metrics_router, router};
use tokio::net::TcpListener;

const INGEST_ADDRESS: &str = "127.0.0.1:8687";
const METRICS_ADDRESS: &str = "127.0.0.1:9091";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::var("DJINN_LOG_STORE_DIR")
        .unwrap_or_else(|_| "/var/lib/djinn-observability/logs".to_owned());
    let store = Arc::new(LogStore::new(root)?);
    let app = router(store.clone());
    let metrics = metrics_router(AppState::new(store));
    let ingest = TcpListener::bind(INGEST_ADDRESS).await?;
    let metrics_listener = TcpListener::bind(METRICS_ADDRESS).await?;
    tokio::select! {
        result = axum::serve(ingest, app) => result?,
        result = axum::serve(metrics_listener, metrics) => result?,
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
