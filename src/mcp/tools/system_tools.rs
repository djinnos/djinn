use rmcp::{tool, tool_router};

use crate::mcp::server::DjinnMcpServer;

#[tool_router(router = system_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Ping the server to check if it's alive. Returns "pong".
    #[tool(description = "Ping the server to check if it's alive")]
    pub async fn system_ping(&self) -> String {
        "pong".to_string()
    }
}
