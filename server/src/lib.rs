#![warn(unreachable_pub)]

pub mod allocator;
pub mod codex_keepalive;
pub mod db;
pub mod error;
pub mod events;
pub mod git_maintenance;
pub mod leadership;
pub mod logging;
mod mcp_bridge;
pub mod mirror_fetcher;
pub mod server;
pub mod server_memory;
pub mod sse;

#[cfg(any(test, feature = "test-support"))]
pub mod test_helpers;

#[cfg(test)]
mod mcp_contract_tests;
