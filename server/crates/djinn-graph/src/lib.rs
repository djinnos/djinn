//! `djinn-graph` — canonical-graph building and repo-map rendering
//! extracted out of `djinn-server` so the warm worker binary
//! (`djinn-agent-worker warm-graph ...`) can link just the graph pipeline
//! without pulling in the full server binary.
//!
//! The crate intentionally exposes a tiny seam, [`WarmContext`], to decouple
//! the `canonical_graph` pipeline from the server's `AppState`.  Anything
//! that can lend us a `&Database`, an `EventBus`, and an async single-flight
//! lock can drive `run_warm_graph_command` / `ensure_canonical_graph`.

#![warn(unreachable_pub)]

use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_db::Database;

pub mod architect;
pub mod canonical_graph;
pub mod index_tree;
pub mod process;
pub mod repo_graph;
pub mod repo_map;
pub mod repo_map_personalization;
pub mod scip_parser;

#[cfg(test)]
mod test_helpers;

/// Minimal seam required to drive the canonical-graph warm pipeline.
///
/// Both the server's `AppState` and the agent-worker's local bootstrap
/// implement this so neither has to depend on the other.
pub trait WarmContext: Send + Sync {
    /// The shared database pool used by all `djinn-db` repositories.
    fn db(&self) -> &Database;

    /// Fresh `EventBus` handle for repositories that emit domain events.
    /// Cloned cheaply — callers re-fetch per use.
    fn event_bus(&self) -> EventBus;

    /// Process-wide single-flight gate around the SCIP indexer subprocess
    /// fan-out (ADR-050 §3).  Must return the same `Arc` each call so all
    /// callers serialize through one mutex.
    fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>>;
}
