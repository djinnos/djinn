use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    tool_handler,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use tokio_util::sync::CancellationToken;

use crate::server::AppState;

/// Per-session MCP server instance. Cloned for each new session.
#[derive(Clone)]
pub struct DjinnMcpServer {
    pub state: AppState,
    tool_router: ToolRouter<Self>,
}

impl DjinnMcpServer {
    pub fn new(state: AppState) -> Self {
        Self {
            state: state.clone(),
            tool_router: Self::system_tool_router()
                + Self::project_tool_router()
                + Self::memory_tool_router()
                + Self::provider_tool_router()
                + Self::credential_tool_router()
                + Self::sync_tool_router()
                + Self::execution_tool_router()
                + Self::session_tool_router()
                + Self::task_tool_router()
                + Self::epic_tool_router(),
        }
    }

    /// Build a `StreamableHttpService` that creates one `DjinnMcpServer` per session.
    pub fn into_service(state: AppState, cancel: CancellationToken) -> StreamableHttpService<Self> {
        StreamableHttpService::new(
            move || Ok(DjinnMcpServer::new(state.clone())),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                cancellation_token: cancel.child_token(),
                ..Default::default()
            },
        )
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
