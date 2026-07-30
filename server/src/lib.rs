#![warn(unreachable_pub)]

pub mod admin;
pub mod allocator;
pub mod codex_keepalive;
pub mod db;
pub mod error;
pub mod events;
pub mod git_maintenance;
pub mod graph_retention;
pub mod kueue_workload_reconcile;
pub mod leadership;
pub mod logging;
mod mcp_bridge;
pub mod mirror_fetcher;
pub mod readiness_pin_resolver;
pub mod scip_index_watcher;
pub mod server;
pub mod server_memory;
pub mod sse;

#[cfg(any(test, feature = "test-support"))]
pub mod test_helpers;

#[cfg(test)]
mod mcp_contract_tests;
