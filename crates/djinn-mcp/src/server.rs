use std::{
    collections::{HashMap, HashSet},
    io,
    sync::Arc,
};

use futures::Stream;
use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{
        ClientJsonRpcMessage, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
        ServerJsonRpcMessage,
    },
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
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::state::McpState;

const HIGH_CONFIDENCE_THRESHOLD: f64 = 0.8;

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
            let has_high_confidence_partner = self
                .note_ids
                .iter()
                .any(|candidate| candidate != note_id && high_confidence_notes.contains(candidate.as_str()));

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
    tool_router: ToolRouter<Self>,
}

impl DjinnMcpServer {
    pub fn all_tool_schemas(&self) -> Vec<serde_json::Value> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|tool| {
                serde_json::to_value(tool).expect("MCP tool definitions must serialize to JSON")
            })
            .collect()
    }

    pub fn new(state: McpState) -> Self {
        Self::new_with_batch(state, Arc::new(RwLock::new(CoAccessBatch::default())))
    }

    fn new_with_batch(state: McpState, co_access_batch: Arc<RwLock<CoAccessBatch>>) -> Self {
        Self {
            state: state.clone(),
            co_access_batch,
            tool_router: Self::system_tool_router()
                + Self::project_tool_router()
                + Self::memory_tool_router()
                + Self::provider_tool_router()
                + Self::credential_tool_router()
                + Self::sync_tool_router()
                + Self::execution_tool_router()
                + Self::settings_tool_router()
                + Self::session_tool_router()
                + Self::task_tool_router()
                + Self::epic_tool_router(),
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

    #[cfg(test)]
    pub(crate) async fn recorded_note_ids(&self) -> Vec<String> {
        self.co_access_batch
            .read()
            .await
            .recorded_note_ids()
            .to_vec()
    }
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
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "djinn-server".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: None,
        }
    }
}
