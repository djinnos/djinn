#![warn(unreachable_pub)]

pub mod daemon;
pub mod db;
pub mod embedded;
pub mod error;
pub mod events;
pub mod logging;
pub mod mcp_bridge;
pub mod process;
pub mod repo_graph;
pub mod repo_map;
pub mod repo_map_personalization;
pub mod scip_parser;
pub mod server;
pub mod sse;
pub mod sync;
mod task_confidence;
pub mod watchers;

#[cfg(test)]
pub mod test_helpers;

#[cfg(test)]
mod mcp_contract_tests;
