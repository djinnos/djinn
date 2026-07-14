use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use djinn_db::{
    Database, DatabaseBackendKind, DatabaseBootstrapInfo, DatabaseConnectConfig,
    PostgresDatabaseConfig,
};

use crate::db::postgres::{PostgresRuntimeError, ensure_postgres_runtime_for_connect_config};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseRuntimeConfig {
    pub connect: DatabaseConnectConfig,
}

impl DatabaseRuntimeConfig {
    pub fn from_cli_and_env(
        _db_path: Option<PathBuf>,
        database_url: Option<String>,
    ) -> Result<Self, DatabaseRuntimeError> {
        // Postgres is a sibling service (compose or StatefulSet). The
        // operator must supply a URL via DJINN_DATABASE_URL.
        let url = database_url.ok_or(DatabaseRuntimeError::MissingDatabaseUrl)?;

        Ok(Self {
            connect: DatabaseConnectConfig::Postgres(PostgresDatabaseConfig { url }),
        })
    }

    pub fn backend_kind(&self) -> DatabaseBackendKind {
        self.connect.backend_kind()
    }

    /// Build a `DatabaseRuntimeConfig` targeting Postgres at the given URL.
    /// Used by tests and CLI fallback paths.
    pub fn postgres(url: String) -> Self {
        Self {
            connect: DatabaseConnectConfig::Postgres(PostgresDatabaseConfig { url }),
        }
    }
}

#[derive(Clone)]
pub struct DatabaseRuntimeManager {
    config: Arc<DatabaseRuntimeConfig>,
}

impl DatabaseRuntimeManager {
    pub fn new(config: DatabaseRuntimeConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    pub fn config(&self) -> &DatabaseRuntimeConfig {
        &self.config
    }

    pub fn bootstrap(&self) -> Result<Database, DatabaseRuntimeError> {
        Database::open_with_config(self.config.connect.clone()).map_err(DatabaseRuntimeError::Open)
    }

    /// Probe the postgres service over TCP. Under compose / Helm-managed deploy
    /// the postgres container is a sibling service; if it isn't reachable we
    /// surface an actionable error.
    pub fn ensure_runtime_available(&self) -> Result<(), DatabaseRuntimeError> {
        ensure_postgres_runtime_for_connect_config(&self.config.connect)?;
        Ok(())
    }

    pub fn startup_mode(&self) -> DatabaseRuntimeMode {
        match &self.config.connect {
            DatabaseConnectConfig::Postgres(config) => DatabaseRuntimeMode {
                backend_kind: DatabaseBackendKind::Postgres,
                backend_label: config.display_backend().to_owned(),
                target: redact_postgres_target(&config.url),
                managed_process: false,
            },
        }
    }

    pub fn health_snapshot(&self, db: &Database) -> DatabaseRuntimeHealth {
        let bootstrap = db.bootstrap_info().clone();
        let detail = runtime_detail_for_bootstrap(&bootstrap);
        let DatabaseBootstrapInfo {
            backend_kind,
            backend_label,
            target,
            ..
        } = bootstrap;
        let target = match backend_kind {
            DatabaseBackendKind::Postgres => redact_postgres_target(&target),
        };
        DatabaseRuntimeHealth {
            backend_kind,
            backend_label,
            target,
            runtime_status: DatabaseRuntimeStatus::Ready,
            detail,
        }
    }

    pub fn planned_health_snapshot(&self) -> DatabaseRuntimeHealth {
        let mode = self.startup_mode();
        let detail = match mode.backend_kind {
            DatabaseBackendKind::Postgres => {
                "postgres backend selected; runtime will use the postgres sqlx pool".to_owned()
            }
        };

        DatabaseRuntimeHealth {
            backend_kind: mode.backend_kind,
            backend_label: mode.backend_label,
            target: mode.target,
            runtime_status: DatabaseRuntimeStatus::Pending,
            detail,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseRuntimeStatus {
    Pending,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DatabaseRuntimeHealth {
    pub backend_kind: DatabaseBackendKind,
    pub backend_label: String,
    pub target: String,
    pub runtime_status: DatabaseRuntimeStatus,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseRuntimeMode {
    pub backend_kind: DatabaseBackendKind,
    pub backend_label: String,
    pub target: String,
    pub managed_process: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DatabaseRuntimeError {
    #[error("postgres backend requires DJINN_DATABASE_URL to be set")]
    MissingDatabaseUrl,
    #[error("postgres runtime bootstrap failed: {0}")]
    PostgresRuntime(#[from] PostgresRuntimeError),
    #[error("database bootstrap failed: {0}")]
    Open(#[from] djinn_db::Error),
}

fn redact_postgres_target(url: &str) -> String {
    // Query parameters may contain credentials (for example `password=`),
    // and fragments are never useful in health output. Drop both before
    // inspecting the authority so an `@` in either suffix cannot be mistaken
    // for the userinfo delimiter.
    let target = url.split(['?', '#']).next().unwrap_or_default();
    let Some((scheme, remainder)) = target.split_once("://") else {
        return "postgres://<configured>".to_owned();
    };
    if !scheme.eq_ignore_ascii_case("postgres") && !scheme.eq_ignore_ascii_case("postgresql") {
        return "postgres://<configured>".to_owned();
    }

    let (authority, path) = remainder
        .split_once('/')
        .map_or((remainder, ""), |(authority, path)| (authority, path));
    let Some((_, host)) = authority.rsplit_once('@') else {
        return "postgres://<configured>".to_owned();
    };
    if host.is_empty() {
        return "postgres://<configured>".to_owned();
    }

    if path.is_empty() {
        format!("postgres://<redacted>@{host}")
    } else {
        format!("postgres://<redacted>@{host}/{path}")
    }
}

fn runtime_detail_for_bootstrap(bootstrap: &DatabaseBootstrapInfo) -> String {
    match bootstrap.backend_kind {
        DatabaseBackendKind::Postgres => format!(
            "{} backend ready via postgres sqlx pool",
            bootstrap.backend_label
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_is_default_backend_selection() {
        let config = DatabaseRuntimeConfig::from_cli_and_env(
            None,
            Some("postgres://postgres@127.0.0.1:5433/djinn".to_owned()),
        )
        .unwrap();
        assert_eq!(config.backend_kind(), DatabaseBackendKind::Postgres);
        let DatabaseConnectConfig::Postgres(cfg) = &config.connect;
        assert!(cfg.url.contains("127.0.0.1:5433"));
    }

    #[test]
    fn postgres_backend_requires_explicit_url() {
        let error = DatabaseRuntimeConfig::from_cli_and_env(None, None)
            .expect_err("postgres without url should fail");
        assert!(error.to_string().contains("DJINN_DATABASE_URL"));
    }

    #[test]
    fn postgres_target_is_redacted_for_health_output() {
        let target = redact_postgres_target("postgres://user:secret@127.0.0.1:5432/djinn");
        assert_eq!(target, "postgres://<redacted>@127.0.0.1:5432/djinn");
    }

    #[test]
    fn postgres_target_drops_query_and_fragment_credentials() {
        let target = redact_postgres_target(
            "postgres://user:userinfo-secret@db.internal:5432/djinn?sslmode=require&password=query-secret#fragment-secret",
        );
        assert_eq!(target, "postgres://<redacted>@db.internal:5432/djinn");
        for secret in ["userinfo-secret", "query-secret", "fragment-secret"] {
            assert!(!target.contains(secret));
        }
    }

    #[test]
    fn postgres_target_only_treats_authority_at_sign_as_userinfo() {
        assert_eq!(
            redact_postgres_target("postgres://user:p@ss@db.internal/djinn"),
            "postgres://<redacted>@db.internal/djinn"
        );
        assert_eq!(
            redact_postgres_target("postgres://db.internal/djinn?contact=ops@example.com"),
            "postgres://<configured>"
        );
    }

    #[test]
    fn postgres_target_handles_postgresql_scheme_and_rejects_malformed_targets() {
        assert_eq!(
            redact_postgres_target("postgresql://user:secret@[::1]:5432/djinn"),
            "postgres://<redacted>@[::1]:5432/djinn"
        );
        assert_eq!(
            redact_postgres_target("user:secret@db.internal/djinn"),
            "postgres://<configured>"
        );
        assert_eq!(
            redact_postgres_target("https://user:secret@db.internal/djinn"),
            "postgres://<configured>"
        );
    }
}
