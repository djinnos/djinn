use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use djinn_db::{
    Database, DatabaseBackendKind, DatabaseBootstrapInfo, DatabaseConnectConfig,
    MysqlDatabaseConfig,
};

use crate::db::mysql::{MysqlRuntimeError, ensure_mysql_runtime_for_connect_config};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseRuntimeConfig {
    pub connect: DatabaseConnectConfig,
}

impl DatabaseRuntimeConfig {
    pub fn from_cli_and_env(
        _db_path: Option<PathBuf>,
        backend: Option<String>,
        mysql_url: Option<String>,
        _mysql_flavor: Option<String>,
    ) -> Result<Self, DatabaseRuntimeError> {
        let backend = backend
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("mysql")
            .to_ascii_lowercase();

        match backend.as_str() {
            "mysql" => {
                // MySQL is a sibling service (compose or StatefulSet). The
                // operator must supply a URL via DJINN_MYSQL_URL when not
                // running against the host-exposed compose stack.
                let url = mysql_url.ok_or_else(|| DatabaseRuntimeError::MissingMysqlUrl {
                    backend: backend.clone(),
                })?;

                Ok(Self {
                    connect: DatabaseConnectConfig::Mysql(MysqlDatabaseConfig { url }),
                })
            }
            other => Err(DatabaseRuntimeError::UnknownBackend(other.to_owned())),
        }
    }

    pub fn backend_kind(&self) -> DatabaseBackendKind {
        self.connect.backend_kind()
    }

    /// Build a `DatabaseRuntimeConfig` targeting MySQL at the given URL.
    /// Used by tests and CLI fallback paths.
    pub fn mysql(url: String) -> Self {
        Self {
            connect: DatabaseConnectConfig::Mysql(MysqlDatabaseConfig { url }),
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

    /// Probe the mysql service over TCP. Under compose / Helm-managed deploy
    /// the mysql container is a sibling service; if it isn't reachable we
    /// surface an actionable error.
    pub fn ensure_runtime_available(&self) -> Result<(), DatabaseRuntimeError> {
        ensure_mysql_runtime_for_connect_config(&self.config.connect)?;
        Ok(())
    }

    pub fn startup_mode(&self) -> DatabaseRuntimeMode {
        match &self.config.connect {
            DatabaseConnectConfig::Mysql(config) => DatabaseRuntimeMode {
                backend_kind: DatabaseBackendKind::Mysql,
                backend_label: config.display_backend().to_owned(),
                target: redact_mysql_target(&config.url),
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
            DatabaseBackendKind::Mysql => {
                "mysql backend selected; runtime will use the mysql-compatible sqlx pool"
                    .to_owned()
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
    #[error("unknown database backend `{0}`; expected mysql")]
    UnknownBackend(String),
    #[error("database backend `{backend}` requires DJINN_MYSQL_URL to be set")]
    MissingMysqlUrl { backend: String },
    #[error("mysql runtime bootstrap failed: {0}")]
    MysqlRuntime(#[from] MysqlRuntimeError),
    #[error("database bootstrap failed: {0}")]
    Open(#[from] djinn_db::Error),
}

fn redact_mysql_target(url: &str) -> String {
    match url.rsplit('@').next() {
        Some(host_part) if host_part != url => format!("mysql://<redacted>@{host_part}"),
        _ => "mysql://<configured>".to_owned(),
    }
}

fn runtime_detail_for_bootstrap(bootstrap: &DatabaseBootstrapInfo) -> String {
    match bootstrap.backend_kind {
        DatabaseBackendKind::Mysql => format!(
            "{} backend ready via mysql-compatible sqlx pool",
            bootstrap.backend_label
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_is_default_backend_selection() {
        let config = DatabaseRuntimeConfig::from_cli_and_env(
            None,
            None,
            Some("mysql://root@127.0.0.1:3308/djinn".to_owned()),
            None,
        )
        .unwrap();
        assert_eq!(config.backend_kind(), DatabaseBackendKind::Mysql);
        let DatabaseConnectConfig::Mysql(cfg) = &config.connect;
        assert!(cfg.url.contains("127.0.0.1:3308"));
    }

    #[test]
    fn mysql_backend_requires_explicit_url() {
        let error = DatabaseRuntimeConfig::from_cli_and_env(None, Some("mysql".to_owned()), None, None)
            .expect_err("mysql without url should fail");
        assert!(error.to_string().contains("DJINN_MYSQL_URL"));
    }

    #[test]
    fn mysql_target_is_redacted_for_health_output() {
        let target = redact_mysql_target("mysql://user:secret@127.0.0.1:3306/djinn");
        assert_eq!(target, "mysql://<redacted>@127.0.0.1:3306/djinn");
    }
}
