//! Dedicated test-database acquisition for deterministic QA scenarios.
//!
//! This module adapts `djinn-db`'s existing template-clone lifecycle. It does
//! not bootstrap a template or acquire PostgreSQL advisory locks itself.

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use djinn_db::Database;
use thiserror::Error;

/// Non-secret identity for a dedicated template-cloned database lease.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct DatabaseLeaseIdentity(String);

impl DatabaseLeaseIdentity {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DatabaseLeaseIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// An owned isolated database lease.
///
/// Dropping the contained [`Database`] triggers its existing template-clone
/// cleanup ownership semantics. A scenario must retain this lease for all work
/// that uses the database.
pub struct DatabaseLease {
    identity: DatabaseLeaseIdentity,
    database: Option<Database>,
    cleanup: Option<Arc<dyn LeaseCleanup>>,
}

impl DatabaseLease {
    fn from_database(database: Database) -> Self {
        let identity = database_identity(&database);
        Self {
            identity,
            database: Some(database),
            cleanup: None,
        }
    }

    #[cfg(test)]
    fn for_test(identity: impl Into<String>, cleanup: Arc<dyn LeaseCleanup>) -> Self {
        Self {
            identity: DatabaseLeaseIdentity(identity.into()),
            database: None,
            cleanup: Some(cleanup),
        }
    }

    pub fn identity(&self) -> &DatabaseLeaseIdentity {
        &self.identity
    }

    /// The concrete database handle for database-requiring scenarios.
    ///
    /// Injected test leases intentionally have no live database, preserving
    /// deterministic unit tests without privileged Postgres infrastructure.
    pub fn database(&self) -> Option<&Database> {
        self.database.as_ref()
    }
}

impl Drop for DatabaseLease {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup.cleanup();
        }
    }
}

trait LeaseCleanup: Send + Sync {
    fn cleanup(&self);
}

/// Injectable async seam around the existing `djinn-db` test database
/// lifecycle. Factories return a fresh owned lease for every call.
#[async_trait]
pub trait DatabaseLeaseFactory: Send + Sync {
    async fn acquire(&self) -> Result<DatabaseLease, DatabaseAcquisitionError>;
}

/// Production factory backed solely by `djinn_db::Database`'s existing
/// template-cloned ephemeral database facility.
#[derive(Clone, Debug, Default)]
pub struct DjinnDatabaseLeaseFactory;

#[async_trait]
impl DatabaseLeaseFactory for DjinnDatabaseLeaseFactory {
    async fn acquire(&self) -> Result<DatabaseLease, DatabaseAcquisitionError> {
        // `open_in_memory` is djinn-db's UUID template-clone test lifecycle.
        // It is deliberately the only database mechanism used by this harness.
        let database =
            Database::open_in_memory().map_err(|source| DatabaseAcquisitionError::Open {
                detail: source.to_string(),
            })?;
        database.ensure_initialized().await.map_err(|source| {
            DatabaseAcquisitionError::Initialize {
                detail: source.to_string(),
            }
        })?;
        Ok(DatabaseLease::from_database(database))
    }
}

/// Explicit diagnostics for a scenario database failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum DatabaseAcquisitionError {
    #[error("could not acquire dedicated test database: {detail}")]
    Open { detail: String },
    #[error("could not initialize dedicated test database: {detail}")]
    Initialize { detail: String },
}

/// Database setup is represented as execution evidence, never as a skip.
pub enum DatabaseScenarioOutcome {
    Ready(DatabaseLease),
    Failed(DatabaseAcquisitionError),
}

impl DatabaseScenarioOutcome {
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    pub fn diagnostic(&self) -> Option<&DatabaseAcquisitionError> {
        match self {
            Self::Ready(_) => None,
            Self::Failed(error) => Some(error),
        }
    }
}

/// Acquire a database for exactly one scenario execution.
///
/// Any opening or migration error is preserved as a failed outcome. There is
/// deliberately no skipped branch, preventing unavailable Postgres from
/// producing vacuous smoke evidence.
pub async fn acquire_for_scenario(factory: &dyn DatabaseLeaseFactory) -> DatabaseScenarioOutcome {
    match factory.acquire().await {
        Ok(lease) => DatabaseScenarioOutcome::Ready(lease),
        Err(error) => DatabaseScenarioOutcome::Failed(error),
    }
}

fn database_identity(database: &Database) -> DatabaseLeaseIdentity {
    // Retain only the final database path segment so diagnostics never expose
    // credentials embedded in the bootstrap target URL.
    let target = &database.bootstrap_info().target;
    let name = target
        .rsplit('/')
        .next()
        .unwrap_or("unknown")
        .split('?')
        .next()
        .unwrap_or("unknown");
    DatabaseLeaseIdentity(name.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct CleanupProbe(Mutex<usize>);
    impl LeaseCleanup for CleanupProbe {
        fn cleanup(&self) {
            *self.0.lock().expect("probe mutex") += 1;
        }
    }

    struct ScriptedFactory {
        outcomes: Mutex<Vec<Result<DatabaseLease, DatabaseAcquisitionError>>>,
    }

    #[async_trait]
    impl DatabaseLeaseFactory for ScriptedFactory {
        async fn acquire(&self) -> Result<DatabaseLease, DatabaseAcquisitionError> {
            self.outcomes.lock().expect("script mutex").remove(0)
        }
    }

    #[tokio::test]
    async fn injected_factory_returns_unique_owned_leases_and_drop_cleans_each() {
        let first_cleanup = Arc::new(CleanupProbe::default());
        let second_cleanup = Arc::new(CleanupProbe::default());
        let factory = ScriptedFactory {
            outcomes: Mutex::new(vec![
                Ok(DatabaseLease::for_test(
                    "djinn_test_first",
                    first_cleanup.clone(),
                )),
                Ok(DatabaseLease::for_test(
                    "djinn_test_second",
                    second_cleanup.clone(),
                )),
            ]),
        };

        let first = acquire_for_scenario(&factory).await;
        let second = acquire_for_scenario(&factory).await;
        let (DatabaseScenarioOutcome::Ready(first), DatabaseScenarioOutcome::Ready(second)) =
            (first, second)
        else {
            panic!("both database acquisitions should be ready")
        };
        assert_ne!(first.identity(), second.identity());
        assert!(first.database().is_none());
        drop(first);
        assert_eq!(*first_cleanup.0.lock().unwrap(), 1);
        assert_eq!(*second_cleanup.0.lock().unwrap(), 0);
        drop(second);
        assert_eq!(*second_cleanup.0.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn acquisition_error_is_a_failed_outcome_not_a_skip() {
        let factory = ScriptedFactory {
            outcomes: Mutex::new(vec![Err(DatabaseAcquisitionError::Initialize {
                detail: "template migration rejected".into(),
            })]),
        };

        let outcome = acquire_for_scenario(&factory).await;
        assert!(outcome.is_failed());
        assert_eq!(
            outcome.diagnostic().unwrap().to_string(),
            "could not initialize dedicated test database: template migration rejected"
        );
    }

    #[tokio::test]
    async fn opening_error_is_a_failed_outcome_with_its_diagnostic() {
        let factory = ScriptedFactory {
            outcomes: Mutex::new(vec![Err(DatabaseAcquisitionError::Open {
                detail: "Postgres is unavailable".into(),
            })]),
        };

        let outcome = acquire_for_scenario(&factory).await;
        assert!(outcome.is_failed());
        assert_eq!(
            outcome.diagnostic().unwrap().to_string(),
            "could not acquire dedicated test database: Postgres is unavailable"
        );
    }
}
