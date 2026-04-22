// djinn-control-plane: MCP tool handler crate

pub mod bridge;
pub mod dispatch;
pub mod process;
pub mod server;
pub mod state;
pub mod tools;

#[cfg(test)]
mod server_tests;

pub use state::McpState;
