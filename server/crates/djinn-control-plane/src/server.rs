use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
};

use futures::Stream;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{
        Annotated, ClientJsonRpcMessage, Implementation, ListResourceTemplatesResult,
        ListResourcesResult, PaginatedRequestParams, ProtocolVersion, RawResourceTemplate,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
        ServerInfo, ServerJsonRpcMessage,
    },
    service::{RequestContext, RoleServer},
    tool_handler,
    transport::{
        WorkerTransport,
        common::server_side_http::{ServerSseMessage, session_id},
        streamable_http_server::{
            SessionId, SessionManager, StreamableHttpServerConfig, StreamableHttpService,
            session::local::{
                LocalSessionManager, LocalSessionManagerError, SessionConfig, create_local_session,
            },
        },
    },
};
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::state::McpState;
use crate::tools::memory_tools::contradiction::{
    ContradictionAnalysisInput, spawn_contradiction_analysis_worker,
};
use crate::tools::memory_tools::summaries::spawn_summary_backfill_worker;

const HIGH_CONFIDENCE_THRESHOLD: f64 = 0.8;
const GRAPH_SCHEMA_RESOURCE_TEMPLATE_URI: &str = "djinn://project/{id}/graph-schema";
const GRAPH_SCHEMA_RESOURCE_MIME_TYPE: &str = "application/json";

#[derive(Clone, Default)]
pub(crate) struct CoAccessBatch {
    note_ids: Vec<String>,
    note_ids_set: HashSet<String>,
}

impl CoAccessBatch {
    pub(crate) fn record_read(&mut self, note_id: &str) {
        if self.note_ids_set.insert(note_id.to_string()) {
            self.note_ids.push(note_id.to_string());
        }
    }

    pub(crate) async fn flush(&self, state: &McpState) {
        if self.note_ids.len() < 2 {
            return;
        }

        let repo = djinn_db::NoteRepository::new(state.db().clone(), state.event_bus());
        for (index, note_a) in self.note_ids.iter().enumerate() {
            for note_b in self.note_ids.iter().skip(index + 1) {
                if let Err(error) = repo.upsert_association(note_a, note_b, 1).await {
                    warn!(%error, note_a, note_b, "failed to flush co-access association");
                }
            }
        }

        let confidence_map = match repo.note_confidence_map(&self.note_ids).await {
            Ok(map) => map,
            Err(error) => {
                warn!(%error, "failed to load note confidence map for co-access flush");
                return;
            }
        };

        let high_confidence_notes: HashSet<&str> = self
            .note_ids
            .iter()
            .filter_map(|note_id| {
                confidence_map
                    .get(note_id)
                    .copied()
                    .filter(|confidence| *confidence > HIGH_CONFIDENCE_THRESHOLD)
                    .map(|_| note_id.as_str())
            })
            .collect();

        if high_confidence_notes.is_empty() {
            return;
        }

        for note_id in self.note_ids.iter().filter(|note_id| {
            confidence_map
                .get(*note_id)
                .is_some_and(|confidence| *confidence <= HIGH_CONFIDENCE_THRESHOLD)
        }) {
            let has_high_confidence_partner = self.note_ids.iter().any(|candidate| {
                candidate != note_id && high_confidence_notes.contains(candidate.as_str())
            });

            if !has_high_confidence_partner {
                continue;
            }

            if let Err(error) = repo
                .update_confidence(note_id, djinn_db::repositories::note::CO_ACCESS_HIGH)
                .await
            {
                warn!(%error, note_id, "failed to update co-access confidence");
            } else {
                debug!(note_id, "applied high-confidence co-access boost");
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn recorded_note_ids(&self) -> &[String] {
        &self.note_ids
    }
}

/// Per-session MCP server instance. Cloned for each new session.
#[derive(Clone)]
pub struct DjinnMcpServer {
    pub state: McpState,
    co_access_batch: Arc<RwLock<CoAccessBatch>>,
    summary_backfill_tx: mpsc::Sender<String>,
    pub(crate) contradiction_analysis_tx: mpsc::Sender<ContradictionAnalysisInput>,
    tool_router: ToolRouter<Self>,
}

impl DjinnMcpServer {
    pub fn all_tool_schemas(&self) -> Vec<serde_json::Value> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|tool| {
                let mut value = serde_json::to_value(tool)
                    .expect("MCP tool definitions must serialize to JSON");
                annotate_server_tool_schema_safety(&mut value);
                value
            })
            .collect()
    }

    pub(crate) fn all_resource_templates(&self) -> ListResourceTemplatesResult {
        ListResourceTemplatesResult::with_all_items(vec![graph_schema_resource_template()])
    }

    pub(crate) fn read_resource_uri(&self, uri: String) -> Result<ReadResourceResult, McpError> {
        let result = if let Some(project_id) = parse_graph_schema_project_id(&uri) {
            let text = serde_json::to_string_pretty(&graph_schema_payload(project_id))
                .expect("graph schema payload must serialize");
            Some(ReadResourceResult {
                contents: vec![ResourceContents::TextResourceContents {
                    uri,
                    mime_type: Some(GRAPH_SCHEMA_RESOURCE_MIME_TYPE.to_string()),
                    text,
                    meta: None,
                }],
            })
        } else {
            None
        };

        result.ok_or_else(|| {
            McpError::resource_not_found(
                "unknown resource; expected djinn://project/{id}/graph-schema",
                None,
            )
        })
    }

    pub fn new(state: McpState) -> Self {
        Self::new_with_batch(state, Arc::new(RwLock::new(CoAccessBatch::default())))
    }

    fn new_with_batch(state: McpState, co_access_batch: Arc<RwLock<CoAccessBatch>>) -> Self {
        let summary_backfill_tx = spawn_summary_backfill_worker(state.db().clone());
        let contradiction_analysis_tx = spawn_contradiction_analysis_worker(state.db().clone());
        Self {
            state: state.clone(),
            co_access_batch,
            summary_backfill_tx,
            contradiction_analysis_tx,
            tool_router: Self::system_tool_router()
                + Self::project_tool_router()
                + Self::memory_tool_router()
                + Self::provider_tool_router()
                + Self::credential_tool_router()
                + Self::dispatch_pause_tool_router()
                + Self::doctor_tool_router()
                + Self::execution_tool_router()
                + Self::settings_tool_router()
                + Self::user_settings_tool_router()
                + Self::session_tool_router()
                + Self::task_tool_router()
                + Self::epic_tool_router()
                + Self::proposal_blocks_tool_router()
                + Self::proposal_tool_router()
                + Self::agent_tool_router()
                + Self::graph_tool_router()
                + Self::pr_review_tool_router()
                + Self::github_tool_router()
                + Self::github_app_tool_router()
                + Self::image_tool_router()
                + Self::service_tool_router(),
        }
    }

    /// Build a `StreamableHttpService` that creates one `DjinnMcpServer` per session.
    pub fn into_service(
        state: McpState,
        cancel: CancellationToken,
    ) -> StreamableHttpService<Self, SessionEndHookSessionManager> {
        let session_manager = Arc::new(SessionEndHookSessionManager::new(state));
        StreamableHttpService::new(
            {
                let session_manager = Arc::clone(&session_manager);
                move || {
                    session_manager
                        .create_server_for_new_session()
                        .ok_or_else(|| io::Error::other("session server not staged"))
                }
            },
            session_manager,
            StreamableHttpServerConfig {
                cancellation_token: cancel.child_token(),
                ..Default::default()
            },
        )
    }

    pub(crate) async fn record_memory_read(&self, note_id: &str) {
        self.co_access_batch.write().await.record_read(note_id);
    }

    pub(crate) async fn flush_co_access_batch(&self) {
        let batch = self.co_access_batch.read().await.clone();
        batch.flush(&self.state).await;
    }

    pub(crate) async fn enqueue_missing_summary_backfill(&self, note_id: &str) {
        if let Err(error) = self.summary_backfill_tx.try_send(note_id.to_string()) {
            debug!(%error, note_id, "dropping missing-summary backfill request");
        }
    }

    #[cfg(test)]
    pub(crate) async fn recorded_note_ids(&self) -> Vec<String> {
        self.co_access_batch
            .read()
            .await
            .recorded_note_ids()
            .to_vec()
    }
}

fn annotate_server_tool_schema_safety(value: &mut serde_json::Value) {
    if let Some(obj) = value.as_object_mut() {
        let annotations = obj
            .get("annotations")
            .and_then(serde_json::Value::as_object)
            .cloned();
        if let Some(annotations) = annotations {
            if let Some(read_only) = annotations
                .get("readOnlyHint")
                .and_then(serde_json::Value::as_bool)
            {
                obj.insert("readOnly".to_string(), serde_json::Value::Bool(read_only));
            }
            if let Some(destructive) = annotations
                .get("destructiveHint")
                .and_then(serde_json::Value::as_bool)
            {
                obj.insert(
                    "destructive".to_string(),
                    serde_json::Value::Bool(destructive),
                );
            }
            if let Some(idempotent) = annotations
                .get("idempotentHint")
                .and_then(serde_json::Value::as_bool)
            {
                obj.insert(
                    "idempotent".to_string(),
                    serde_json::Value::Bool(idempotent),
                );
            }
            if let Some(open_world) = annotations
                .get("openWorldHint")
                .and_then(serde_json::Value::as_bool)
            {
                obj.insert("openWorld".to_string(), serde_json::Value::Bool(open_world));
            }
        }
        // This first-party server-wide schema path is produced directly by the
        // rmcp router, not by the agent role schema serializer that owns typed
        // safety classifications. Publish conservative fail-closed defaults so
        // host agents can consume explicit retry/approval hints rather than
        // treating missing metadata as safe.
        obj.entry("readOnly")
            .or_insert(serde_json::Value::Bool(false));
        obj.entry("destructive")
            .or_insert(serde_json::Value::Bool(true));
        obj.entry("idempotent")
            .or_insert(serde_json::Value::Bool(false));
        obj.entry("openWorld")
            .or_insert(serde_json::Value::Bool(false));
        obj.entry("concurrent_safe")
            .or_insert(serde_json::Value::Bool(false));
    }
}

fn graph_schema_resource_template() -> Annotated<RawResourceTemplate> {
    Annotated::new(
        RawResourceTemplate {
            uri_template: GRAPH_SCHEMA_RESOURCE_TEMPLATE_URI.to_string(),
            name: "project_graph_schema".to_string(),
            title: Some("Project code graph schema".to_string()),
            description: Some(
                "Stable read-only schema/context for the code_graph MCP tool. Hosts can preload \
                 this resource instead of adding another tool for graph discovery."
                    .to_string(),
            ),
            mime_type: Some(GRAPH_SCHEMA_RESOURCE_MIME_TYPE.to_string()),
            icons: None,
        },
        None,
    )
}

fn parse_graph_schema_project_id(uri: &str) -> Option<&str> {
    const PREFIX: &str = "djinn://project/";
    const SUFFIX: &str = "/graph-schema";
    let id = uri.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

fn graph_schema_payload(project_id: &str) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "resource": {
            "uri_template": GRAPH_SCHEMA_RESOURCE_TEMPLATE_URI,
            "project_id_or_slug": project_id,
            "read_only": true,
            "mime_type": GRAPH_SCHEMA_RESOURCE_MIME_TYPE
        },
        "tool": {
            "name": "code_graph",
            "relationship": "This resource describes the stable graph concepts and operation surface for the existing code_graph tool; it does not add a new MCP tool."
        },
        "operations": [
            { "name": "status", "requires": [], "common_fields": ["project", "workspace"], "purpose": "Inspect graph availability, freshness, and warm status before deeper graph reads." },
            { "name": "search", "requires": ["query"], "common_fields": ["limit", "tests", "workspace"], "purpose": "Find symbol/file nodes by query before resolving a precise node." },
            { "name": "describe", "requires": ["key"], "common_fields": ["workspace"], "purpose": "Return identity, metadata, and summary details for one resolved graph node." },
            { "name": "neighbors", "requires": ["key"], "common_fields": ["direction", "limit", "tests", "workspace"], "purpose": "Traverse direct incoming/outgoing edges around a symbol or file." },
            { "name": "context", "requires": ["key"], "common_fields": ["limit", "tests", "workspace"], "purpose": "Return a 360-degree symbol view with callers, callees, implementations, and related nodes." },
            { "name": "impact", "requires": ["key"], "common_fields": ["depth", "limit", "tests", "workspace"], "purpose": "Estimate blast radius and risk from dependents reachable through graph edges." },
            { "name": "path", "requires": ["from", "to"], "common_fields": ["max_depth", "workspace"], "purpose": "Find a dependency path between two nodes." },
            { "name": "edges", "requires": ["from_glob", "to_glob"], "common_fields": ["limit", "workspace"], "purpose": "List cross-boundary graph edges matching source and target globs." },
            { "name": "ranked", "requires": [], "common_fields": ["limit", "tests", "workspace"], "purpose": "List high-importance nodes by graph ranking." },
            { "name": "implementations", "requires": ["key"], "common_fields": ["limit", "workspace"], "purpose": "Find implementation nodes for an interface/trait-like symbol." },
            { "name": "query_subgraph", "requires": ["query"], "common_fields": ["budget", "limit", "workspace"], "purpose": "Budgeted natural-language graph/subgraph query for focused exploration." },
            { "name": "cycles", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Detect cyclic dependencies." },
            { "name": "orphans", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Find isolated or weakly connected nodes." },
            { "name": "symbols_at", "requires": ["file"], "common_fields": ["line", "workspace"], "purpose": "Resolve symbols at a source location." },
            { "name": "diff_touches", "requires": [], "common_fields": ["base", "head", "workspace"], "purpose": "Map changed files/regions to graph nodes." },
            { "name": "detect_changes", "requires": [], "common_fields": ["base", "head", "workspace"], "purpose": "Summarize graph-relevant changes between revisions." },
            { "name": "api_surface", "requires": [], "common_fields": ["path", "limit", "workspace"], "purpose": "Inspect exposed/public surface nodes." },
            { "name": "boundary_check", "requires": ["from_glob", "to_glob"], "common_fields": ["workspace"], "purpose": "Check whether dependencies cross an intended architecture boundary." },
            { "name": "hotspots", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Identify high-risk/high-activity graph areas." },
            { "name": "complexity", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Rank nodes/files by complexity metrics." },
            { "name": "refactor_candidates", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Find nodes combining complexity, churn, coupling, or centrality signals." },
            { "name": "metrics_at", "requires": ["key"], "common_fields": ["workspace"], "purpose": "Return graph/complexity/churn metrics for one node." },
            { "name": "dead_symbols", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Find symbols without observed inbound usage." },
            { "name": "deprecated_callers", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Find callers of deprecated symbols." },
            { "name": "touches_hot_path", "requires": [], "common_fields": ["base", "head", "workspace"], "purpose": "Determine whether a diff touches hot-path nodes." },
            { "name": "coupling", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Measure coupling between modules/areas." },
            { "name": "churn", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Rank graph nodes by change frequency." },
            { "name": "coupling_hotspots", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Find high-coupling, high-risk nodes or modules." },
            { "name": "coupling_hubs", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Find highly connected dependency hubs." },
            { "name": "snapshot", "requires": [], "common_fields": ["limit", "workspace"], "purpose": "Return a capped graph snapshot for preloading or visualization." }
        ],
        "node_concepts": [
            { "name": "symbol", "description": "Function, method, type, trait/interface, module, or similar code entity addressable by a stable graph key/uid." },
            { "name": "file", "description": "Repository file node that contains or groups symbols." },
            { "name": "workspace", "description": "Optional workspace slug for monorepos; requests can scope reads with the workspace field." },
            { "name": "test node", "description": "Node classified as test code; many operations accept tests filters to include/exclude it." }
        ],
        "edge_concepts": [
            { "name": "contains", "description": "File/module containment relationships for symbols." },
            { "name": "calls", "description": "Caller-to-callee executable dependency." },
            { "name": "imports", "description": "Module/file import or dependency relationship." },
            { "name": "implements", "description": "Implementation relationship between concrete symbols and interfaces/traits." },
            { "name": "references", "description": "General symbol/file reference edge used for navigation and impact analysis." }
        ],
        "common_request_fields": {
            "project": "Project id, short id, or slug used by code_graph tool calls; this resource embeds the id/slug from its URI.",
            "workspace": "Optional monorepo workspace slug.",
            "key": "Resolved node key or uid from search/describe/context.",
            "query": "Text query for search or query_subgraph.",
            "direction": "incoming, outgoing, or both for edge traversals.",
            "limit": "Maximum result count/budget cap for list-like operations.",
            "tests": "Test-code filter where supported."
        },
        "recommended_flow": [
            "Read this graph-schema resource once per project to prime client context.",
            "Call code_graph status before relying on warmed graph answers.",
            "Use search to find candidate nodes, then describe or context to inspect one node.",
            "Use neighbors/path/impact for traversal and blast-radius analysis.",
            "Use query_subgraph with a budget for focused natural-language graph exploration."
        ]
    })
}

#[derive(Default)]
pub struct SessionEndHookSessionManager {
    local: LocalSessionManager,
    state: Option<McpState>,
    session_servers: RwLock<HashMap<SessionId, DjinnMcpServer>>,
    staged_server: RwLock<Option<DjinnMcpServer>>,
}

impl SessionEndHookSessionManager {
    pub fn new(state: McpState) -> Self {
        Self {
            local: LocalSessionManager {
                sessions: Default::default(),
                session_config: SessionConfig::default(),
            },
            state: Some(state),
            session_servers: RwLock::new(HashMap::new()),
            staged_server: RwLock::new(None),
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self::default()
    }

    fn state(&self) -> &McpState {
        self.state
            .as_ref()
            .expect("session manager state is configured")
    }

    async fn insert_session_server(&self, session_id: SessionId, server: DjinnMcpServer) {
        self.session_servers
            .write()
            .await
            .insert(session_id, server);
    }

    #[cfg(test)]
    pub(crate) async fn server_for_session(
        &self,
        session_id: &SessionId,
    ) -> Option<DjinnMcpServer> {
        self.session_servers.read().await.get(session_id).cloned()
    }

    fn build_session_server(&self) -> DjinnMcpServer {
        DjinnMcpServer::new(self.state().clone())
    }

    pub(crate) fn create_server_for_new_session(&self) -> Option<DjinnMcpServer> {
        self.staged_server.blocking_write().take()
    }
}

impl SessionManager for SessionEndHookSessionManager {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let id: SessionId = session_id();
        let (handle, worker) = create_local_session(id.clone(), self.local.session_config.clone());
        self.local.sessions.write().await.insert(id.clone(), handle);

        let server = self.build_session_server();
        self.insert_session_server(id.clone(), server.clone()).await;
        *self.staged_server.write().await = Some(server);

        Ok((id, WorkerTransport::spawn(worker)))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.local.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.local.has_session(id).await
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        if let Some(server) = self.session_servers.write().await.remove(id) {
            server.flush_co_access_batch().await;
        }
        self.local.close_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.local.create_stream(id, message).await
    }

    async fn accept_message(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<(), Self::Error> {
        self.local.accept_message(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.local.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.local.resume(id, last_event_id).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DjinnMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
            server_info: Implementation {
                name: "djinn-server".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: None,
        }
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListResourcesResult::default()))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, McpError>> + Send + '_ {
        std::future::ready(Ok(self.all_resource_templates()))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        std::future::ready(self.read_resource_uri(request.uri))
    }
}
