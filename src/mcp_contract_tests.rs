//! Consolidated MCP contract and integration tests.
//! These tests exercise MCP tool handlers via the full HTTP stack.

use djinn_mcp::server::DjinnMcpServer;
use tokio_util::sync::CancellationToken;

use crate::server::AppState;
use crate::test_helpers::create_test_db;

#[allow(dead_code)]
fn test_mcp_server() -> DjinnMcpServer {
    DjinnMcpServer::new(AppState::new(create_test_db(), CancellationToken::new()).mcp_state())
}

// ── dispatch tests ────────────────────────────────────────────────────────────
