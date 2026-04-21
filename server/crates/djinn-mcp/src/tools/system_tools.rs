use rmcp::{Json, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::Serialize;

use crate::server::DjinnMcpServer;

#[derive(Serialize, JsonSchema)]
pub struct PingResponse {
    pub status: &'static str,
    pub version: &'static str,
}

#[tool_router(router = system_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Ping the server. Returns {status: ok, version}.
    #[tool(description = "Ping the server to check if it's alive")]
    pub async fn system_ping(&self) -> Json<PingResponse> {
        Json(PingResponse {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
        })
    }
}
