//! In-crate test helpers — mirrors the sliver of
//! `djinn-server::test_helpers` the canonical-graph / repo-map tests need
//! without dragging the server crate in.
//!
//! Exposed as a top-level module under `#[cfg(test)]` so all siblings can
//! consume `crate::test_helpers::*`.

#![cfg(test)]

use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_db::Database;

use crate::WarmContext;

/// Open a fresh test database (isolated Dolt branch via
/// `Database::open_in_memory`).
pub(crate) fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Create a per-test tempdir rooted under `target/test-tmp` so the tests
/// play nice with our standard clean-up script.
pub(crate) fn workspace_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).expect("create djinn-graph test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create djinn-graph test tempdir")
}

/// Minimal [`WarmContext`] backed by an in-memory DB + no-op event bus +
/// per-test indexer mutex.  Suitable for unit tests that don't go through
/// the full `AppState` constructor.
pub(crate) struct TestWarmContext {
    db: Database,
    indexer_lock: Arc<tokio::sync::Mutex<()>>,
}

impl TestWarmContext {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl WarmContext for TestWarmContext {
    fn db(&self) -> &Database {
        &self.db
    }

    fn event_bus(&self) -> EventBus {
        EventBus::noop()
    }

    fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.indexer_lock.clone()
    }
}
