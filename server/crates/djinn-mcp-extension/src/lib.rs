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
//! | [`shared_schemas`] | Shared tool schema builders and safety annotations |
//! | [`tool_defs`] | Per-role tool schema aggregation functions |
//! | [`tool_defs_code_graph`] | Code-graph and PR-review-context tool schemas |
//! | [`finalize_tools`] | Finalize tool schemas (submit_work, submit_review, etc.) |
//! | [`dispatch`] | Tool dispatch and handler orchestration (future) |
//! | [`types`] | Shared types used across extension handlers (future) |

pub mod context;
pub mod finalize_tools;
pub mod shared_schemas;
pub mod tool_defs;
pub mod tool_defs_code_graph;

// Re-export the ExtensionContext trait at crate root for ergonomic imports.
pub use context::ExtensionContext;

// Skeleton modules for future extraction waves.
pub mod dispatch;
pub mod types;
