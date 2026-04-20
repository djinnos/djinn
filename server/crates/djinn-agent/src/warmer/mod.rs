//! Concrete [`djinn_runtime::GraphWarmerService`] implementations that live in
//! djinn-agent.
//!
//! Phase 3 PR 7 hosts [`InProcessGraphWarmer`] here — the single-process
//! warmer used by `AppState` (production) and `TestRuntime` (the
//! `phase1_supervisor` integration test).  It intentionally avoids depending
//! on the server crate by taking a callback for the heavy
//! `ensure_canonical_graph` call.

pub mod in_process;

pub use in_process::{
    FreshnessProbe, InProcessGraphWarmer, InProcessWarmerDeps, ProjectRootResolver, WarmCallback,
};
