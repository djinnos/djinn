// djinn-control-plane: MCP tool handler crate

pub mod bridge;
pub mod dispatch;
pub mod process;
/// Default-disabled branch and draft-PR lifecycle for direct build attempts.
pub mod proposal_attempt_lifecycle;
pub mod readiness_kickoff;
pub mod readiness_query;
pub mod server;
pub mod state;
pub mod toolchain_versions;
pub mod tools;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

#[cfg(test)]
mod server_tests;

pub use state::McpState;
