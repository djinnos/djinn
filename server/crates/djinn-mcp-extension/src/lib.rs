//! MCP extension surface for Djinn.
//!
//! This crate owns the tool schemas, shared types, dispatch logic, and
//! `ExtensionContext` capability trait for the Djinn MCP extension layer.
//! It is designed to be consumed by `djinn-agent` (via a façade re-export)
//! without introducing a back-dependency on `djinn-agent` internals.
//!
//! # Module layout
//!
//! | Module | Future contents |
//! |--------|----------------|
//! | [`schema`] | Tool schema definitions and `shared_schemas` |
//! | [`context`] | `ExtensionContext` capability trait |
//! | [`dispatch`] | Tool dispatch and handler orchestration |
//! | [`types`] | Shared types used across extension handlers |

pub mod context;
pub mod dispatch;
pub mod schema;
pub mod types;
