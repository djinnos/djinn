#![warn(unreachable_pub)]

pub(crate) mod canonical_graph;
pub mod daemon;
pub mod db;
pub mod embedded;
pub mod error;
pub mod events;
pub mod housekeeping;
pub mod index_tree;
pub mod logging;
mod mcp_bridge;
pub mod memory_fs;
pub mod process;
pub mod repo_graph;
pub mod repo_map;
pub mod repo_map_personalization;
pub(crate) mod scip_parser;
pub mod semantic_memory;
pub mod server;
pub mod sse;
pub mod sync;
mod task_confidence;
pub(crate) mod watchers;

#[cfg(test)]
pub mod test_helpers;

#[cfg(test)]
mod mcp_contract_tests;
