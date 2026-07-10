// The `tool_code_graph()` schema uses `serde_json::json!` via the local
// `object!` macro. After Phase 2 added seven new ops, the macro
// expansion outgrew the default 128 recursion budget.
#![recursion_limit = "256"]

//! MCP extension surface for Djinn.
//!
//! This crate owns the tool schemas, shared types, dispatch logic, and
//! `ExtensionContext` capability trait for the Djinn MCP extension layer.
//! It is designed to be consumed by `djinn-agent` (via a façade re-export)
//! without introducing a back-dependency on `djinn-agent` internals.
//!
//! # Module layout
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`context`] | `ExtensionContext` capability trait |
//! | [`dispatch`] | Central tool dispatch with tool-group helpers |
//! | [`handlers`] | Handler implementations for each tool group |
//! | [`helpers`] | Shared utility functions for handlers |
//! | [`types`] | Parameter types for tool-call arguments |
//! | [`fuzzy`] | Multi-layer fuzzy string matching for edit tool |
//! | [`truncate`] | Smart truncation utilities |
//! | [`shared_schemas`] | Shared tool schema builders and safety annotations |
//! | [`tool_defs`] | Per-role tool schema aggregation functions |
//! | [`tool_defs_code_graph`] | Code-graph and PR-review-context tool schemas |
//! | [`finalize_tools`] | Finalize tool schemas (submit_work, submit_review, etc.) |

pub mod command_classifier;
pub mod command_validator;
pub mod context;
pub mod dispatch;
pub mod finalize_tools;
// Fuzzy string matching — used by workspace handlers that will migrate
// to this crate in a follow-up task.
#[allow(dead_code)]
pub mod fuzzy;
// Handler functions that are not yet fully wired into the dispatch path
// may produce dead_code warnings — this is expected during the incremental
// extraction process.
#[allow(dead_code)]
pub mod handlers;
#[allow(dead_code)]
pub mod helpers;
pub mod shared_schemas;
pub mod tool_defs;
pub mod tool_defs_code_graph;
pub mod truncate;
pub mod types;

// Re-export the ExtensionContext trait at crate root for ergonomic imports.
pub use context::ExtensionContext;

// Re-export dispatch result for façade consumers.
pub use dispatch::DispatchResult;

#[cfg(test)]
mod tests;
