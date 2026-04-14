use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use djinn_db::{
    Database, DatabaseBackendKind, DatabaseBootstrapInfo, DatabaseConnectConfig,
    MysqlBackendFlavor, MysqlDatabaseConfig, SqliteDatabaseConfig,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseRuntimeConfig {
    pub connect: DatabaseConnectConfig,
}

impl DatabaseRuntimeConfig {
    pub fn sqlite(path: PathBuf) -> Self {
        Self {
            connect: DatabaseConnectConfig::Sqlite(SqliteDatabaseConfig {
                path,
                readonly: false,
                create_if_missing: true,
            }),
        }
    }

    pub fn from_cli_and_env(
        db_path: Option<PathBuf>,
        backend: Option<String>,
        mysql_url: Option<String>,
        mysql_flavor: Option<String>,
    ) -> Result<Self, DatabaseRuntimeError> {
        let backend = backend
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("sqlite")
            .to_ascii_lowercase();

        match backend.as_str() {
            "sqlite" => Ok(Self::sqlite(
                db_path.unwrap_or_else(djinn_db::default_db_path),
            )),
            "mysql" | "dolt" => {
                let url = mysql_url.ok_or_else(|| DatabaseRuntimeError::MissingMysqlUrl {
                    backend: backend.clone(),
                })?;
                let flavor = match mysql_flavor
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(backend.as_str())
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "mysql" => MysqlBackendFlavor::Mysql,
                    "dolt" => MysqlBackendFlavor::Dolt,
                    other => {
                        return Err(DatabaseRuntimeError::InvalidMysqlFlavor(other.to_owned()));
                    }
                };

                Ok(Self {
                    connect: DatabaseConnectConfig::Mysql(MysqlDatabaseConfig { url, flavor }),
                })
            }
            other => Err(DatabaseRuntimeError::UnknownBackend(other.to_owned())),
        }
    }

    pub fn backend_kind(&self) -> DatabaseBackendKind {
        self.connect.backend_kind()
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

    pub fn startup_mode(&self) -> DatabaseRuntimeMode {
        match &self.config.connect {
            DatabaseConnectConfig::Sqlite(config) => DatabaseRuntimeMode {
                backend_kind: DatabaseBackendKind::Sqlite,
                backend_label: "sqlite".to_owned(),
                target: config.path.display().to_string(),
                managed_process: false,
            },
            DatabaseConnectConfig::Mysql(config) => DatabaseRuntimeMode {
                backend_kind: DatabaseBackendKind::Mysql,
                backend_label: config.display_backend().to_owned(),
                target: redact_mysql_target(&config.url),
                managed_process: matches!(config.flavor, MysqlBackendFlavor::Dolt),
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
            DatabaseBackendKind::Sqlite => {
                "sqlite backend selected; no external SQL server process required".to_owned()
            }
            DatabaseBackendKind::Mysql => {
                if mode.managed_process {
                    "dolt sql-server backend selected; runtime will use the mysql-compatible connection seam"
                        .to_owned()
                } else {
                    "mysql backend selected; runtime will use the mysql-compatible connection seam"
                        .to_owned()
                }
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

    pub fn should_spawn_sqlite_checkpoint(&self) -> bool {
        matches!(self.config.backend_kind(), DatabaseBackendKind::Sqlite)
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
    #[error("unknown database backend `{0}`; expected sqlite, mysql, or dolt")]
    UnknownBackend(String),
    #[error("database backend `{backend}` requires DJINN_MYSQL_URL to be set")]
    MissingMysqlUrl { backend: String },
    #[error("unknown mysql/dolt flavor `{0}`; expected mysql or dolt")]
    InvalidMysqlFlavor(String),
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
        DatabaseBackendKind::Sqlite => {
            if bootstrap.readonly {
                "sqlite backend ready in read-only mode".to_owned()
            } else {
                "sqlite backend ready; SQLite-specific pragmas and migrations applied locally"
                    .to_owned()
            }
        }
        DatabaseBackendKind::Mysql => {
            format!(
                "{} backend ready via mysql-compatible sqlx pool",
                bootstrap.backend_label
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_is_default_backend_selection() {
        let config = DatabaseRuntimeConfig::from_cli_and_env(None, None, None, None).unwrap();
        assert_eq!(config.backend_kind(), DatabaseBackendKind::Sqlite);
    }

    #[test]
    fn dolt_requires_mysql_url() {
        let error =
            DatabaseRuntimeConfig::from_cli_and_env(None, Some("dolt".to_owned()), None, None)
                .expect_err("dolt selection without url should fail");
        assert!(error.to_string().contains("DJINN_MYSQL_URL"));
    }

    #[test]
    fn mysql_target_is_redacted_for_health_output() {
        let target = redact_mysql_target("mysql://user:secret@127.0.0.1:3306/djinn");
        assert_eq!(target, "mysql://<redacted>@127.0.0.1:3306/djinn");
    }

    #[test]
    fn sqlite_checkpoint_loop_only_runs_for_sqlite() {
        let sqlite = DatabaseRuntimeManager::new(DatabaseRuntimeConfig::sqlite(
            std::path::Path::new("/tmp/test.db").to_path_buf(),
        ));
        assert!(sqlite.should_spawn_sqlite_checkpoint());

        let mysql = DatabaseRuntimeManager::new(DatabaseRuntimeConfig {
            connect: DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
                url: "mysql://root@127.0.0.1:3306/djinn".to_owned(),
                flavor: MysqlBackendFlavor::Dolt,
            }),
        });
        assert!(!mysql.should_spawn_sqlite_checkpoint());
    }
}
