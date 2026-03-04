use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::sse;
use axum::Router;
use axum::routing::get;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::actors::coordinator::CoordinatorHandle;
use crate::actors::git::{GitActorHandle, GitError};
use crate::actors::supervisor::AgentSupervisorHandle;
use crate::agent::init_session_manager;
use crate::db::connection::Database;
use crate::db::repositories::note::NoteRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::settings::SettingsRepository;
use crate::events::DjinnEvent;
use crate::mcp;
use crate::provider::{CatalogService, HealthTracker};
use crate::sync::SyncManager;

const EVENT_CHANNEL_CAPACITY: usize = 1024;
const SETTINGS_RAW_KEY: &str = "settings.raw";
const MODEL_HEALTH_STATE_KEY: &str = "model_health.state";

/// Shared application state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    pub db: Database,
    pub cancel: CancellationToken,
    pub events: broadcast::Sender<DjinnEvent>,
    pub git_actors: Mutex<HashMap<PathBuf, GitActorHandle>>,
    /// models.dev catalog + custom providers (in-memory, refreshed on startup).
    pub catalog: CatalogService,
    /// Per-model circuit-breaker health tracker.
    pub health_tracker: HealthTracker,
    /// djinn/ namespace git sync manager.
    pub sync: SyncManager,
    /// Long-running coordinator actor handle.
    pub coordinator: Mutex<Option<CoordinatorHandle>>,
    /// Long-running agent supervisor actor handle.
    pub supervisor: Mutex<Option<AgentSupervisorHandle>>,
}

impl AppState {
    pub fn new(db: Database, cancel: CancellationToken) -> Self {
        Self::new_inner(db, cancel)
    }

    fn new_inner(db: Database, cancel: CancellationToken) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let sync = SyncManager::new(db.clone(), events.clone());
        Self {
            inner: Arc::new(Inner {
                db,
                cancel,
                events,
                git_actors: Mutex::new(HashMap::new()),
                catalog: CatalogService::new(),
                health_tracker: HealthTracker::new(),
                sync,
                coordinator: Mutex::new(None),
                supervisor: Mutex::new(None),
            }),
        }
    }

    pub fn db(&self) -> &Database {
        &self.inner.db
    }

    pub fn cancel(&self) -> &CancellationToken {
        &self.inner.cancel
    }

    pub fn events(&self) -> &broadcast::Sender<DjinnEvent> {
        &self.inner.events
    }

    pub fn sync_user_id(&self) -> &str {
        "local"
    }

    /// Get or spawn a `GitActorHandle` for the given project path (GIT-04).
    pub async fn git_actor(&self, path: &Path) -> Result<GitActorHandle, GitError> {
        let mut map = self.inner.git_actors.lock().await;
        crate::actors::git::get_or_spawn(&mut map, path)
    }

    pub fn catalog(&self) -> &CatalogService {
        &self.inner.catalog
    }

    pub fn health_tracker(&self) -> &HealthTracker {
        &self.inner.health_tracker
    }

    pub fn sync_manager(&self) -> &SyncManager {
        &self.inner.sync
    }

    pub async fn coordinator(&self) -> Option<CoordinatorHandle> {
        self.inner.coordinator.lock().await.clone()
    }

    pub async fn supervisor(&self) -> Option<AgentSupervisorHandle> {
        self.inner.supervisor.lock().await.clone()
    }

    /// Spawn long-running agent actors once and keep their handles in AppState.
    pub async fn initialize_agents(&self) {
        if self.supervisor().await.is_some() {
            return;
        }

        let sessions_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".djinn")
            .join("sessions");
        if let Err(e) = std::fs::create_dir_all(&sessions_dir) {
            tracing::warn!(error = %e, path = %sessions_dir.display(), "failed to create sessions directory");
            return;
        }

        let session_manager = init_session_manager(sessions_dir);
        let supervisor =
            AgentSupervisorHandle::spawn(self.clone(), session_manager, self.cancel().clone());
        let coordinator = CoordinatorHandle::spawn(
            self.events().clone(),
            self.cancel().clone(),
            self.db().clone(),
            supervisor.clone(),
            self.catalog().clone(),
            self.health_tracker().clone(),
        );

        *self.inner.supervisor.lock().await = Some(supervisor.clone());
        *self.inner.coordinator.lock().await = Some(coordinator.clone());

        self.apply_runtime_settings_from_db().await;

        // Coordinator starts paused — require explicit `execution_start` to begin dispatching.
        tracing::info!("coordinator spawned (paused — awaiting explicit execution_start)");
    }

    /// Load custom providers from DB into the catalog and trigger a background
    /// catalog refresh from models.dev.  Call once after server startup.
    pub async fn initialize(&self) {
        use crate::db::repositories::custom_provider::CustomProviderRepository;
        use crate::models::provider::{Model, Provider};

        // Load custom providers from DB → merge into in-memory catalog.
        let repo = CustomProviderRepository::new(self.db().clone());
        match repo.list().await {
            Ok(providers) => {
                for cp in providers {
                    let provider = Provider {
                        id: cp.id.clone(),
                        name: cp.name,
                        npm: String::new(),
                        env_vars: vec![cp.env_var],
                        base_url: cp.base_url,
                        docs_url: String::new(),
                        is_openai_compatible: true,
                    };
                    let seed_models: Vec<Model> = cp
                        .seed_models
                        .iter()
                        .map(|s| Model {
                            id: s.id.clone(),
                            provider_id: cp.id.clone(),
                            name: s.name.clone(),
                            tool_call: false,
                            reasoning: false,
                            attachment: false,
                            context_window: 0,
                            output_limit: 0,
                            pricing: crate::models::provider::Pricing::default(),
                        })
                        .collect();
                    self.catalog().add_custom_provider(provider, seed_models);
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to load custom providers from DB"),
        }

        // Kick off background refresh from models.dev.
        let catalog = self.catalog().clone();
        tokio::spawn(async move {
            catalog.refresh().await;
        });

        // Restore sync state from DB and start background auto-export task.
        let sync = self.sync_manager().clone();
        sync.restore().await;
        sync.spawn_background_task(self.cancel().clone(), self.sync_user_id().to_string());

        self.restore_model_health_state().await;

        self.reindex_all_projects_on_startup().await;
        self.reload_settings_and_apply().await;
        self.spawn_settings_file_watcher();
    }

    async fn reindex_all_projects_on_startup(&self) {
        let project_repo = ProjectRepository::new(self.db().clone(), self.events().clone());
        let note_repo = NoteRepository::new(self.db().clone(), self.events().clone());
        let projects = match project_repo.list().await {
            Ok(projects) => projects,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list projects for startup reindex");
                return;
            }
        };

        for project in projects {
            match note_repo
                .reindex_from_disk(&project.id, Path::new(&project.path))
                .await
            {
                Ok(summary) => tracing::info!(
                    project = %project.path,
                    updated = summary.updated,
                    created = summary.created,
                    deleted = summary.deleted,
                    unchanged = summary.unchanged,
                    "startup memory reindex completed"
                ),
                Err(e) => tracing::warn!(
                    project = %project.path,
                    error = %e,
                    "startup memory reindex failed"
                ),
            }
        }
    }

    async fn reload_settings_and_apply(&self) {
        let Some(path) = settings_file_path() else {
            return;
        };
        if let Err(e) = self.reload_settings_from_file(&path).await {
            tracing::warn!(error = %e, path = %path.display(), "settings reload failed");
        }
    }

    pub async fn reload_settings_from_disk(&self) -> Result<(), String> {
        let Some(path) = settings_file_path() else {
            return Ok(());
        };
        self.reload_settings_from_file(&path).await
    }

    pub async fn apply_settings_raw(&self, raw: &str) -> Result<(), String> {
        let json: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("parse settings JSON: {e}"))?;
        self.validate_model_priority_providers_connected(&json)
            .await?;
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        repo.set(SETTINGS_RAW_KEY, raw)
            .await
            .map_err(|e| e.to_string())?;
        self.apply_runtime_settings(&json).await;
        Ok(())
    }

    async fn validate_model_priority_providers_connected(
        &self,
        settings: &serde_json::Value,
    ) -> Result<(), String> {
        let priorities = read_model_priorities(settings).unwrap_or_default();
        if priorities.is_empty() {
            return Ok(());
        }

        let configured_provider_ids: HashSet<String> = priorities
            .values()
            .flat_map(|models| models.iter())
            .map(|model| {
                model
                    .split_once('/')
                    .map(|(provider_id, _)| provider_id)
                    .unwrap_or(model.as_str())
                    .to_string()
            })
            .collect();
        if configured_provider_ids.is_empty() {
            return Ok(());
        }

        let repo = CredentialRepository::new(self.db().clone(), self.events().clone());
        let credentials = repo
            .list()
            .await
            .map_err(|e| format!("list credentials: {e}"))?;
        let connected_provider_ids: HashSet<String> =
            credentials.into_iter().map(|c| c.provider_id).collect();

        let mut missing_provider_ids: Vec<String> = configured_provider_ids
            .difference(&connected_provider_ids)
            .cloned()
            .collect();
        missing_provider_ids.sort();

        if missing_provider_ids.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "model_priority references disconnected providers: {}",
                missing_provider_ids.join(", ")
            ))
        }
    }

    pub async fn reset_runtime_settings(&self) {
        if let Some(coordinator) = self.coordinator().await {
            let _ = coordinator.update_dispatch_limit(50).await;
            let _ = coordinator
                .update_model_priorities(std::collections::HashMap::new())
                .await;
        }
        if let Some(supervisor) = self.supervisor().await {
            let _ = supervisor.update_max_sessions(1).await;
        }
    }

    fn spawn_settings_file_watcher(&self) {
        let Some(path) = settings_file_path() else {
            return;
        };

        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(error = %e, path = %parent.display(), "failed to create settings directory");
            return;
        }

        let cancel = self.cancel().clone();
        let state = self.clone();

        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::unbounded_channel::<notify::Result<notify::Event>>();

            let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |res| {
                let _ = tx.send(res);
            }) {
                Ok(watcher) => watcher,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to start settings watcher");
                    return;
                }
            };

            let watch_target = path
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| path.clone());

            if let Err(e) = watcher.watch(&watch_target, RecursiveMode::NonRecursive) {
                tracing::warn!(error = %e, path = %watch_target.display(), "failed to watch settings directory");
                return;
            }

            tracing::info!(path = %path.display(), "watching settings file");

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    maybe_event = rx.recv() => {
                        let Some(event) = maybe_event else { break };
                        match event {
                            Ok(ev) if (ev.kind.is_modify() || ev.kind.is_create())
                                && ev.paths.iter().any(|p| p == &path) => {
                                // Editors often emit multiple writes; coalesce briefly.
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                if let Err(e) = state.reload_settings_from_file(&path).await {
                                    tracing::warn!(error = %e, path = %path.display(), "settings reload on change failed");
                                }
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!(error = %e, "settings watcher event error"),
                        }
                    }
                }
            }
        });
    }

    async fn reload_settings_from_file(&self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }

        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read settings file {}: {e}", path.display()))?;
        let json: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("parse settings file {}: {e}", path.display()))?;

        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        repo.set(SETTINGS_RAW_KEY, &raw)
            .await
            .map_err(|e| e.to_string())?;
        self.apply_runtime_settings(&json).await;

        Ok(())
    }

    async fn apply_runtime_settings_from_db(&self) {
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        let raw = repo
            .get(SETTINGS_RAW_KEY)
            .await
            .ok()
            .flatten()
            .map(|s| s.value);
        let Some(raw) = raw else {
            self.reset_runtime_settings().await;
            return;
        };
        let parsed: serde_json::Result<serde_json::Value> = serde_json::from_str(&raw);
        match parsed {
            Ok(json) => self.apply_runtime_settings(&json).await,
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse persisted settings.raw");
                self.reset_runtime_settings().await;
            }
        }
    }

    async fn apply_runtime_settings(&self, json: &serde_json::Value) {
        if let Some(coordinator) = self.coordinator().await {
            let _ = coordinator
                .update_dispatch_limit(read_dispatch_limit(json).unwrap_or(50))
                .await;
            let _ = coordinator
                .update_model_priorities(read_model_priorities(json).unwrap_or_default())
                .await;
        }

        if let Some(supervisor) = self.supervisor().await {
            let model_limits = read_model_session_limits(json).unwrap_or_default();
            let _ = supervisor
                .update_session_limits(
                    model_limits,
                    read_default_max_sessions(json).unwrap_or(1),
                )
                .await;
        }
    }

    pub async fn persist_model_health_state(&self) {
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        let snapshot = self.health_tracker().all_health();
        match serde_json::to_string(&snapshot) {
            Ok(raw) => {
                if let Err(e) = repo.set(MODEL_HEALTH_STATE_KEY, &raw).await {
                    tracing::warn!(error = %e, "failed to persist model health state");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to serialize model health state"),
        }
    }

    async fn restore_model_health_state(&self) {
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        let raw = repo
            .get(MODEL_HEALTH_STATE_KEY)
            .await
            .ok()
            .flatten()
            .map(|s| s.value);
        let Some(raw) = raw else {
            return;
        };
        match serde_json::from_str::<Vec<crate::provider::health::ModelHealth>>(&raw) {
            Ok(snapshot) => self.health_tracker().restore_all(snapshot),
            Err(e) => tracing::warn!(error = %e, "failed to parse model health state"),
        }
    }
}

fn settings_file_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".djinn").join("settings.json"))
}

fn read_dispatch_limit(settings: &serde_json::Value) -> Option<usize> {
    // Supported keys:
    // { "coordinator": { "dispatch_limit": 50 } }
    // { "execution": { "dispatch_limit": 50 } }
    settings
        .get("coordinator")
        .and_then(|v| v.get("dispatch_limit"))
        .or_else(|| {
            settings
                .get("execution")
                .and_then(|v| v.get("dispatch_limit"))
        })
        .and_then(serde_json::Value::as_u64)
        .map(|v| v as usize)
}

fn read_default_max_sessions(settings: &serde_json::Value) -> Option<u32> {
    settings
        .get("supervisor")
        .and_then(|v| v.get("max_sessions"))
        .or_else(|| {
            settings
                .get("execution")
                .and_then(|v| v.get("max_sessions"))
        })
        .and_then(serde_json::Value::as_u64)
        .map(|v| v as u32)
}

fn read_model_session_limits(settings: &serde_json::Value) -> Option<HashMap<String, u32>> {
    let map = settings
        .get("max_sessions")
        .or_else(|| {
            settings
                .get("execution")
                .and_then(|v| v.get("max_sessions"))
        })
        .or_else(|| {
            settings
                .get("supervisor")
                .and_then(|v| v.get("max_sessions"))
        })
        .and_then(serde_json::Value::as_object)?;

    let mut out = HashMap::new();
    for (model_id, max) in map {
        let Some(max) = max.as_u64() else {
            continue;
        };
        if max == 0 {
            continue;
        }
        out.insert(model_id.clone(), max as u32);
    }
    Some(out)
}

fn read_model_priorities(
    settings: &serde_json::Value,
) -> Option<std::collections::HashMap<String, Vec<String>>> {
    let root = settings
        .get("coordinator")
        .and_then(|v| v.get("model_priority"))
        .or_else(|| {
            settings
                .get("execution")
                .and_then(|v| v.get("model_priority"))
        })
        .or_else(|| settings.get("models").and_then(|v| v.get("priority")))?
        .as_object()?;

    let mut out = std::collections::HashMap::new();
    for (role, value) in root {
        let Some(arr) = value.as_array() else {
            continue;
        };
        let models: Vec<String> = arr
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
        if !models.is_empty() {
            out.insert(role.clone(), models);
        }
    }
    Some(out)
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> axum::Json<HealthResponse> {
    axum::Json(HealthResponse { status: "ok" })
}

/// Build the application router.
pub fn router(state: AppState) -> Router {
    let mcp_service =
        mcp::server::DjinnMcpServer::into_service(state.clone(), state.cancel().clone());

    let mcp_router = Router::new().fallback_service(mcp_service);

    Router::new()
        .route("/health", get(health))
        .route("/events", get(sse::events_handler))
        .route("/db-info", get(sse::db_info_handler))
        .nest("/mcp", mcp_router)
        .with_state(state)
}

/// Run the server, blocking until shutdown signal.
pub async fn run(router: Router, port: u16, cancel: CancellationToken) {
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind");

    tracing::info!(port, "listening on 0.0.0.0:{port}");

    axum::serve(listener, router)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .expect("server error");

    tracing::info!("server shut down");
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::body::Body;
    use axum::http::header::{ACCEPT, CONTENT_TYPE};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::{read_default_max_sessions, read_model_priorities, read_model_session_limits};
    use crate::db::repositories::credential::CredentialRepository;
    use crate::server::AppState;
    use crate::test_helpers;
    use tokio_util::sync::CancellationToken;

    /// Integration test: hit /health via tower::ServiceExt::oneshot().
    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
    async fn mcp_tool_schemas_avoid_nonstandard_uint_formats() {
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
                    if let Some(Value::String(format)) = map.get("format") {
                        if format == "uint" || format.starts_with("uint") {
                            bad.push(format!(
                                "{tool_name} {schema_kind} {path}/format = {format}"
                            ));
                        }
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
            .cloned()
            .expect("missing mcp-session-id header on initialize response");

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

        let list_payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        let list_req = axum::http::Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .header("mcp-session-id", session_id)
            .body(Body::from(list_payload.to_string()))
            .unwrap();

        let list_resp = app.oneshot(list_req).await.unwrap();
        assert_eq!(list_resp.status(), 200);

        let body = list_resp.into_body().collect().await.unwrap().to_bytes();
        let raw = String::from_utf8(body.to_vec()).expect("sse body should be utf-8");
        let events = parse_sse_json_events(&raw);
        let result = events
            .iter()
            .find(|event| event.get("id") == Some(&Value::from(2)))
            .and_then(|event| event.get("result"))
            .expect("tools/list JSON-RPC result missing in SSE stream");

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

        let tools_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp/tools");
        let mut offenders = Vec::new();
        visit(&tools_dir, &mut offenders);

        assert!(
            offenders.is_empty(),
            "Found bare serde_json::Value in MCP tool structs (use AnyJson/ObjectJson instead):\n  {}",
            offenders.join("\n  ")
        );
    }

    /// Unit test: verify the in-memory test DB has migrations applied.
    #[tokio::test]
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

    #[test]
    fn reads_default_max_sessions_from_supervisor_or_execution() {
        let settings = serde_json::json!({"supervisor": {"max_sessions": 4}});
        assert_eq!(read_default_max_sessions(&settings), Some(4));

        let settings = serde_json::json!({"execution": {"max_sessions": 2}});
        assert_eq!(read_default_max_sessions(&settings), Some(2));
    }

    #[test]
    fn reads_model_session_limits_from_top_level_and_nested_paths() {
        let settings = serde_json::json!({
            "max_sessions": {
                "synthetic/hf:nvidia/Kimi-K2.5-NVFP4": 4,
                "openai/gpt-5.3-codex": 2
            }
        });
        let parsed = read_model_session_limits(&settings).unwrap();
        assert_eq!(
            parsed.get("synthetic/hf:nvidia/Kimi-K2.5-NVFP4"),
            Some(&4)
        );
        assert_eq!(parsed.get("openai/gpt-5.3-codex"), Some(&2));

        let settings = serde_json::json!({
            "execution": {
                "max_sessions": {
                    "synthetic/hf:nvidia/Kimi-K2.5-NVFP4": 3
                }
            }
        });
        let parsed = read_model_session_limits(&settings).unwrap();
        assert_eq!(
            parsed.get("synthetic/hf:nvidia/Kimi-K2.5-NVFP4"),
            Some(&3)
        );
    }

    #[test]
    fn reads_model_priorities_from_supported_paths() {
        let settings = serde_json::json!({
            "coordinator": {
                "model_priority": {
                    "worker": ["openai/gpt-4o"],
                    "task_reviewer": ["anthropic/claude-opus-4-6"]
                }
            }
        });
        let parsed = read_model_priorities(&settings).unwrap();
        assert_eq!(parsed.get("worker").unwrap(), &vec!["openai/gpt-4o"]);

        let settings = serde_json::json!({
            "models": {
                "priority": {
                    "epic_reviewer": ["openai/o3"]
                }
            }
        });
        let parsed = read_model_priorities(&settings).unwrap();
        assert_eq!(parsed.get("epic_reviewer").unwrap(), &vec!["openai/o3"]);
    }

    #[tokio::test]
    async fn apply_settings_raw_rejects_disconnected_model_priority_provider() {
        let db = test_helpers::create_test_db();
        let state = AppState::new(db, CancellationToken::new());

        let err = state
            .apply_settings_raw(
                r#"{"coordinator":{"model_priority":{"worker":["nvidia/moonshotai/kimi-k2-instruct"]}}}"#,
            )
            .await
            .expect_err("should reject disconnected provider");

        assert!(err.contains("disconnected providers"));
        assert!(err.contains("nvidia"));
    }

    #[tokio::test]
    async fn apply_settings_raw_accepts_connected_model_priority_provider() {
        let db = test_helpers::create_test_db();
        let state = AppState::new(db, CancellationToken::new());

        let cred_repo = CredentialRepository::new(state.db().clone(), state.events().clone());
        cred_repo
            .set("synthetic", "SYNTHETIC_API_KEY", "sk-test")
            .await
            .unwrap();

        state
            .apply_settings_raw(
                r#"{"coordinator":{"model_priority":{"worker":["synthetic/hf:moonshotai/Kimi-K2.5"]}}}"#,
            )
            .await
            .expect("connected provider should be accepted");
    }
}
