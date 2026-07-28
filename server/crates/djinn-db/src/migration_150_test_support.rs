//! Cleanup-safe migration-150 compatibility fixture for Postgres consumers.
//!
//! This intentionally builds the historical state with embedded sqlx migration
//! objects, so `_sqlx_migrations` contains the same checksums as a real upgrade.

use std::borrow::Cow;
use std::future::Future;
use std::panic::{AssertUnwindSafe, resume_unwind};

use futures::FutureExt;
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

use crate::database::{
    Database, DatabaseConnectConfig, PostgresDatabaseConfig, test_database_base_url,
};
use crate::error::{DbError, DbResult};
use crate::migrations::{
    DesignatedOperatorBootstrap, MigrationContext, bootstrap_designated_operator,
    run_postgres_migrations_on_connection,
};

const OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000150";
const PRESET_ID: &str = "preset-postgres-18";

/// Expected ordinary preset fields, which remain the sole application-facing
/// service configuration after the historical wrapper columns were retired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration150OrdinaryPreset {
    pub image: &'static str,
    pub service_type: &'static str,
    pub port: i32,
    pub env: &'static str,
    pub resources: &'static str,
    pub conn_template: &'static str,
    pub conn_env_var: &'static str,
}

/// Historical values deliberately populated after migration 150.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration150HistoricalWrapperValues {
    pub wrapper_image: &'static str,
    pub image_digest: &'static str,
    pub verification_protocol_revision: i32,
}

/// An upgraded isolated database and the deterministic values it contains.
#[derive(Clone, Debug)]
pub struct Migration150Fixture {
    pub database: Database,
    pub database_url: String,
    pub preset_id: &'static str,
    pub ordinary_preset: Migration150OrdinaryPreset,
    pub historical_wrapper: Migration150HistoricalWrapperValues,
}

/// Set up a migration-150-populated database, pass it to `consumer`, and
/// terminate sessions and drop it before returning or resuming a panic.
///
/// The callback owns the fixture so consumers cannot accidentally retain an
/// uncleaned setup handle. The cleaner has an independent `Database` clone and
/// administrative connection, allowing cleanup after successful, error, and
/// panic outcomes alike.
pub async fn with_migration_150_fixture<T, F, Fut>(consumer: F) -> DbResult<T>
where
    F: FnOnce(Migration150Fixture) -> Fut,
    Fut: Future<Output = DbResult<T>>,
{
    let (fixture, cleaner) = create_migration_150_fixture().await?;
    let outcome = AssertUnwindSafe(consumer(fixture)).catch_unwind().await;
    let cleanup = cleaner.cleanup().await;

    match outcome {
        Ok(Ok(value)) => {
            cleanup?;
            Ok(value)
        }
        Ok(Err(error)) => {
            let _ = cleanup;
            Err(error)
        }
        Err(payload) => {
            let _ = cleanup;
            resume_unwind(payload);
        }
    }
}

async fn create_migration_150_fixture() -> DbResult<(Migration150Fixture, FixtureCleaner)> {
    let base_url = test_database_base_url();
    let server_prefix = server_prefix(&base_url);
    let database_name = format!("djinn_migration_150_{}", uuid::Uuid::now_v7().simple());
    let database_url = format!("{server_prefix}/{database_name}");
    let admin_url = format!("{server_prefix}/postgres");

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .map_err(DbError::from)?;
    admin
        .execute(format!(r#"CREATE DATABASE "{database_name}""#).as_str())
        .await
        .map_err(DbError::from)?;
    admin.close().await.map_err(DbError::from)?;

    bootstrap_designated_operator(
        &database_url,
        &DesignatedOperatorBootstrap {
            user_id: OPERATOR_ID.to_owned(),
            github_id: 9_000_000_150,
            github_login: "migration-150-operator".to_owned(),
            github_name: None,
            github_avatar_url: None,
        },
    )
    .await?;

    let mut connection = PgConnection::connect(&database_url)
        .await
        .map_err(DbError::from)?;
    let setup_result = async {
        // `bootstrap_designated_operator` uses a separate connection. Migration
        // 142 consumes this session-scoped GUC, so configure it again on the
        // connection that owns the remainder of the historical migration run.
        sqlx::query("SET statement_timeout = 0")
            .execute(&mut connection)
            .await
            .map_err(DbError::from)?;
        sqlx::query("SELECT set_config('djinn.migration_designated_operator_user_id', $1, false)")
            .bind(OPERATOR_ID)
            .execute(&mut connection)
            .await
            .map_err(DbError::from)?;

        let embedded = sqlx::migrate!("./migrations_postgres");
        let through_150 = sqlx::migrate::Migrator {
            migrations: Cow::Owned(
                embedded
                    .migrations
                    .iter()
                    .filter(|migration| migration.version <= 150)
                    .cloned()
                    .collect(),
            ),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        through_150
            .run_direct(&mut connection)
            .await
            .map_err(|error| DbError::InvalidData(error.to_string()))?;

        sqlx::query(
            "UPDATE service_presets \
             SET wrapper_image = $1, image_digest = $2, verification_protocol_revision = $3 \
             WHERE id = $4",
        )
        .bind(HISTORICAL_WRAPPER.wrapper_image)
        .bind(HISTORICAL_WRAPPER.image_digest)
        .bind(HISTORICAL_WRAPPER.verification_protocol_revision)
        .bind(PRESET_ID)
        .execute(&mut connection)
        .await
        .map_err(DbError::from)?;

        run_postgres_migrations_on_connection(
            &mut connection,
            &MigrationContext {
                designated_operator_user_id: Some(OPERATOR_ID.to_owned()),
            },
        )
        .await
    }
    .await;
    let close_result = connection.close().await.map_err(DbError::from);
    setup_result?;
    close_result?;

    let database =
        Database::open_with_config(DatabaseConnectConfig::Postgres(PostgresDatabaseConfig {
            url: database_url.clone(),
        }))?;
    database.verify_and_mark_initialized().await?;

    Ok((
        Migration150Fixture {
            database: database.clone(),
            database_url: database_url.clone(),
            preset_id: PRESET_ID,
            ordinary_preset: ORDINARY_PRESET,
            historical_wrapper: HISTORICAL_WRAPPER,
        },
        FixtureCleaner {
            database,
            admin_url,
            database_name,
        },
    ))
}

#[derive(Debug)]
struct FixtureCleaner {
    database: Database,
    admin_url: String,
    database_name: String,
}

impl FixtureCleaner {
    async fn cleanup(self) -> DbResult<()> {
        self.database.pool().close().await;
        let mut admin = PgConnection::connect(&self.admin_url)
            .await
            .map_err(DbError::from)?;
        let result = async {
            sqlx::query(&format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = '{}' AND pid <> pg_backend_pid()",
                self.database_name
            ))
            .execute(&mut admin)
            .await
            .map_err(DbError::from)?;
            sqlx::query(&format!(
                r#"DROP DATABASE IF EXISTS "{}""#,
                self.database_name
            ))
            .execute(&mut admin)
            .await
            .map_err(DbError::from)
        }
        .await;
        let close_result = admin.close().await.map_err(DbError::from);
        result?;
        close_result
    }
}

fn server_prefix(base_url: &str) -> String {
    base_url
        .rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base_url)
        .trim_end_matches('/')
        .to_owned()
}

const ORDINARY_PRESET: Migration150OrdinaryPreset = Migration150OrdinaryPreset {
    image: "postgres:18-alpine",
    service_type: "postgres",
    port: 5432,
    env: r#"{"POSTGRES_PASSWORD":"postgres","POSTGRES_USER":"postgres","POSTGRES_DB":"app_test","POSTGRES_INITDB_ARGS":"-c shared_buffers=64MB"}"#,
    resources: r#"{"cpu_request":"100m","memory_request":"256Mi","cpu_limit":"500m","memory_limit":"512Mi"}"#,
    conn_template: "postgres://postgres:postgres@{host}:5432/app_test?sslmode=disable",
    conn_env_var: "DATABASE_URL,TEST_POSTGRES_URL",
};

const HISTORICAL_WRAPPER: Migration150HistoricalWrapperValues =
    Migration150HistoricalWrapperValues {
        wrapper_image: "example.invalid/djinn/migration-150-wrapper",
        image_digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        verification_protocol_revision: 1,
    };
