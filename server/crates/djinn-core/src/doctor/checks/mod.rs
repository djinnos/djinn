//! Doctor seed-check submodules.
//!
//! Each submodule groups a small set of `DoctorCheck` impls from a single
//! incident class. T1 owns the first two (sessions + slots); T2 owns
//! `disposition`, T3 owns `k8s` (which also houses the `force_close_orphan`
//! additions), and T4 owns the `live_mover_predicate` check on the
//! `djinn-agent` side. T5 wires all of them into the framework registry
//! and ships the cross-crate registration bridge.

pub mod disposition;
pub mod k8s;
pub mod retrieval;
pub mod sessions;
pub mod slots;
