#![warn(unreachable_pub)]

pub mod db;
pub mod embedded;
pub mod error;
pub mod events;
pub mod mirror_fetcher;
pub mod logging;
mod mcp_bridge;
pub mod memory_fs;
pub mod memory_mount;
pub mod server;
pub mod sse;

#[cfg(test)]
pub mod test_helpers;

#[cfg(test)]
mod mcp_contract_tests;
