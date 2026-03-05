use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::sse;
use axum::Router;
use axum::routing::get;
use tower_http::cors::CorsLayer;
use serde::Serialize;
use tokio::sync::{Mutex, broadcast};
use tokio_util::sync::CancellationToken;

use crate::actors::coordinator::CoordinatorHandle;
use crate::actors::git::{GitActorHandle, GitError};
use crate::actors::supervisor::AgentSupervisorHandle;
use crate::agent::init_session_manager;
use crate::db::connection::Database;
use crate::db::repositories::credential::CredentialRepository;
use crate::db::repositories::note::NoteRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::settings::SettingsRepository;
use crate::events::DjinnEvent;
use crate::mcp;
use crate::models::settings::DjinnSettings;
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

        // Inject synthetic catalog entries for Goose-only providers (e.g.
        // chatgpt_codex, gcp_vertex_ai) that aren't in models.dev.
        let goose_entries = goose::providers::providers().await;
        self.catalog().inject_goose_providers(&goose_entries);

        // Kick off background refresh from models.dev.
        let catalog = self.catalog().clone();
        let goose_entries_for_refresh = goose_entries.clone();
        tokio::spawn(async move {
            catalog.refresh().await;
            // Re-inject after refresh so Goose-only providers survive the replace.
            catalog.inject_goose_providers(&goose_entries_for_refresh);
        });

        // Restore sync state from DB and start background auto-export task.
        let sync = self.sync_manager().clone();
        sync.restore().await;
        sync.spawn_background_task(self.cancel().clone(), self.sync_user_id().to_string());

        self.restore_model_health_state().await;

        self.reindex_all_projects_on_startup().await;
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

    pub async fn apply_settings(&self, settings: &DjinnSettings) -> Result<(), String> {
        self.validate_model_priority_providers_connected(settings)
            .await?;
        let raw = serde_json::to_string(settings)
            .map_err(|e| format!("serialize settings: {e}"))?;
        let repo = SettingsRepository::new(self.db().clone(), self.events().clone());
        repo.set(SETTINGS_RAW_KEY, &raw)
            .await
            .map_err(|e| e.to_string())?;
        self.apply_runtime_settings(settings).await;
        Ok(())
    }

    async fn validate_model_priority_providers_connected(
        &self,
        settings: &DjinnSettings,
    ) -> Result<(), String> {
        let priorities = settings.model_priority_or_default();
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
        let mut connected_provider_ids: HashSet<String> =
            credentials.into_iter().map(|c| c.provider_id).collect();

        // Also consider OAuth-connected providers (e.g. chatgpt_codex, github_copilot).
        let goose_entries = crate::mcp::tools::provider_tools::goose_provider_entries().await;
        let catalog_providers = self.catalog().list_providers();
        for provider in &catalog_providers {
            let oauth_keys =
                crate::mcp::tools::provider_tools::oauth_keys_for_provider(&provider.id, &goose_entries);
            if !oauth_keys.is_empty()
                && crate::mcp::tools::provider_tools::is_oauth_key_present(&oauth_keys)
            {
                connected_provider_ids.insert(provider.id.clone());
            }
        }

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
            let _ = supervisor.update_session_limits(std::collections::HashMap::new(), 1).await;
        }
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
        let settings = DjinnSettings::from_db_value(&raw);
        self.apply_runtime_settings(&settings).await;
    }

    async fn apply_runtime_settings(&self, settings: &DjinnSettings) {
        let mut coordinator_handle = None;
        if let Some(coordinator) = self.coordinator().await {
            let _ = coordinator
                .update_dispatch_limit(settings.dispatch_limit_or_default())
                .await;
            let _ = coordinator
                .update_model_priorities(settings.model_priority_or_default())
                .await;
            coordinator_handle = Some(coordinator);
        }

        if let Some(supervisor) = self.supervisor().await {
            let _ = supervisor
                .update_session_limits(settings.max_sessions_or_default(), 1)
                .await;
        }

        // Capacity/model changes can make additional tasks dispatchable immediately.
        // Trigger a dispatch pass now instead of waiting for the next event/tick.
        if let Some(coordinator) = coordinator_handle {
            let _ = coordinator.trigger_dispatch().await;
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
        .layer(CorsLayer::permissive())
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

    use crate::db::repositories::credential::CredentialRepository;
    use crate::models::settings::DjinnSettings;
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

    #[tokio::test]
    async fn apply_settings_rejects_disconnected_model_priority_provider() {
        let db = test_helpers::create_test_db();
        let state = AppState::new(db, CancellationToken::new());

        let settings = DjinnSettings {
            model_priority: Some(
                [("worker".into(), vec!["nvidia/moonshotai/kimi-k2-instruct".into()])]
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

    #[tokio::test]
    async fn apply_settings_accepts_connected_model_priority_provider() {
        let db = test_helpers::create_test_db();
        let state = AppState::new(db, CancellationToken::new());

        let cred_repo = CredentialRepository::new(state.db().clone(), state.events().clone());
        cred_repo
            .set("synthetic", "SYNTHETIC_API_KEY", "sk-test")
            .await
            .unwrap();

        let settings = DjinnSettings {
            model_priority: Some(
                [("worker".into(), vec!["synthetic/hf:moonshotai/Kimi-K2.5".into()])]
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
