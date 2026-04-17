//! Supervisor flow templates — moved to [`djinn_runtime::spec`] as part of
//! Phase 2 PR 1.
//!
//! Re-exported here so existing `use super::flow::{RoleKind, SupervisorFlow}`
//! call sites in `mod.rs`, `stage.rs`, `pr.rs`, and
//! `actors/slot/supervisor_runner.rs` keep compiling without edits.  The
//! tests that used to live here moved into `djinn-runtime/src/spec.rs`.

// NOTE: `#[deprecated]` was deliberately dropped to keep the workspace
// warning count identical to pre-move; PR 2 deletes these shims outright.
pub use djinn_runtime::spec::{RoleKind, SupervisorFlow};
