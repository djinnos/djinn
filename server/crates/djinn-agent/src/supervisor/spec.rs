//! Supervisor input/output types — moved to [`djinn_runtime::spec`] as part
//! of Phase 2 PR 1.
//!
//! This file now re-exports the moved types from their new home so that
//! every existing call site (including `mod.rs`, `stage.rs`, `pr.rs`,
//! `actors/slot/supervisor_runner.rs`, and the `phase1_supervisor`
//! integration test) keeps compiling without edits.  PR 2 extracts the
//! supervisor into its own crate and drops these shims.

// NOTE: `#[deprecated]` was deliberately dropped to keep the workspace
// warning count identical to pre-move; PR 2 deletes these shims outright.
pub use djinn_runtime::spec::{TaskRunOutcome, TaskRunReport, TaskRunSpec};

// The `#[deprecated]` attribute above only fires if a call site names the
// path `djinn_agent::supervisor::spec::TaskRunSpec` directly.  `mod.rs`
// also re-exports these at `djinn_agent::supervisor::TaskRunSpec` (without
// `spec`), which is the path every in-tree consumer uses, so we do not
// expect any deprecation warnings during PR 1.
