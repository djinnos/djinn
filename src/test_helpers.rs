use tokio_util::sync::CancellationToken;

use crate::db::connection::Database;
use crate::server::{self, AppState};

/// Create an in-memory database with all migrations applied.
pub fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Create an Axum router wired to a fresh in-memory database.
pub fn create_test_app() -> axum::Router {
    let db = create_test_db();
    let cancel = CancellationToken::new();
    let state = AppState::new(db, cancel);
    server::router(state)
}
