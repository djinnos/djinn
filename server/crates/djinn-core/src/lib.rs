pub mod auth_context;
pub mod auto_submit_decision;
pub mod canonical_verify;
pub mod cargo_target_runs;
pub mod clock;
pub mod commands;
pub mod doctor;
pub mod error;
pub mod events;
pub mod extension_diagnostics;
pub mod index_tree;
pub mod live_state_migration;
pub mod liveness;
pub mod message;
pub mod models;
pub mod paths;
pub mod run_progress;
pub mod test_paths;
pub mod tool_call;
pub mod tool_error;

#[cfg(test)]
mod lane_roundtrip_proof;
