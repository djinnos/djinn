//! Migration coverage for the durable, constrained session failure-cause label.
//!
//! The column is deliberately nullable for legacy sessions, while only stable
//! machine-readable labels may be persisted.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 183;
const MIGRATION_FILE: &str = "183_session_failure_cause.sql";
const MIGRATION_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000183";
const CREATOR_CONTRACT_VERSION: u64 = 142;

fn base_database_url() -> String {
    djinn_db::test_database_base_url()
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

fn migrations_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres")
}

fn migration_entries(dir: &Path) -> Vec<(u64, PathBuf)> {
    let mut entries: Vec<(u64, PathBuf)> = std::fs::read_dir(dir)
        .expect("read migrations dir")
        .map(|entry| {
            let path = entry.expect("migration dir entry").path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let version = name
                .split_once('_')
                .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
                .unwrap_or(0);
            (version, path)
        })
        .filter(|(_, path)| {
            path.extension().and_then(|extension| extension.to_str()) == Some("sql")
        })
        .collect();
    entries.sort_by(|(left_version, left_path), (right_version, right_path)| {
        left_version.cmp(right_version).then_with(|| {
            left_path
                .file_name()
                .unwrap_or_default()
                .cmp(right_path.file_name().unwrap_or_default())
        })
    });
    entries
}

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let db_name = format!("djinn_migration_{suffix}_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect postgres admin database");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("create migration test database");
    drop(admin);

    let db_url = format!("{prefix}/{db_name}");
    let result = f(db_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect postgres admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{db_name}""#).as_str())
        .await
        .expect("drop migration test database");

    result
}

async fn seed_migration_operator(conn: &mut PgConnection) {
    conn.execute(
        format!(
            "INSERT INTO users (id, github_id, github_login) VALUES \
             ('{MIGRATION_OPERATOR_ID}', 9000000183, 'failure-cause-migration-operator') \
             ON CONFLICT DO NOTHING"
        )
        .as_str(),
    )
    .await
    .expect("seed designated migration operator");
}

async fn apply_prior_migrations(conn: &mut PgConnection) {
    conn.execute(
        format!(
            "SELECT set_config('djinn.migration_designated_operator_user_id', '{MIGRATION_OPERATOR_ID}', false)"
        )
        .as_str(),
    )
    .await
    .expect("set designated operator GUC");

    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        if version == CREATOR_CONTRACT_VERSION {
            seed_migration_operator(conn).await;
        }
        let sql = std::fs::read_to_string(&path).expect("read prior migration sql");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("apply migration {} failed: {error}", path.display()));
    }
}

async fn apply_migration_183(conn: &mut PgConnection) {
    let sql = std::fs::read_to_string(migrations_dir().join(MIGRATION_FILE))
        .expect("read migration 183 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 183 after prior migrations");
}

async fn insert_session(pool: &PgPool, id: &str) {
    // projects.id is VARCHAR(36); use a compact unique identifier rather than
    // deriving it from the descriptive session fixture ID.
    let project_id = uuid::Uuid::now_v7().simple().to_string();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
    )
    .bind(&project_id)
    .bind(&project_id)
    .bind("djinnos")
    .bind(&project_id)
    .execute(pool)
    .await
    .expect("insert session project");

    sqlx::query(
        "INSERT INTO sessions (id, project_id, model_id, agent_type, status) \
         VALUES ($1, $2, 'test-model', 'worker', 'failed')",
    )
    .bind(id)
    .bind(&project_id)
    .execute(pool)
    .await
    .expect("insert session");
}

async fn assert_failure_cause_schema_and_values(pool: &PgPool) {
    let column: (String, String, Option<String>) = sqlx::query_as(
        "SELECT data_type, is_nullable, column_default \
         FROM information_schema.columns \
         WHERE table_name = 'sessions' AND column_name = 'failure_cause'",
    )
    .fetch_one(pool)
    .await
    .expect("inspect sessions.failure_cause column");
    assert_eq!(column.0, "text");
    assert_eq!(column.1, "YES");
    assert_eq!(column.2, None, "failure_cause must not have a default");

    insert_session(pool, "failure-cause-null").await;
    let null_cause: Option<String> =
        sqlx::query_scalar("SELECT failure_cause FROM sessions WHERE id = 'failure-cause-null'")
            .fetch_one(pool)
            .await
            .expect("read null failure cause");
    assert_eq!(
        null_cause, None,
        "new and legacy rows remain cause-free by default"
    );

    for (index, cause) in [
        "cancelled",
        "provider",
        "harness",
        "infrastructure",
        "protocol",
        "finalization",
        "unknown",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("failure-cause-accepted-{index}");
        insert_session(pool, &id).await;
        sqlx::query("UPDATE sessions SET failure_cause = $1 WHERE id = $2")
            .bind(cause)
            .bind(&id)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("{cause} should be accepted: {error}"));
    }

    for (index, invalid_cause) in [
        "legacy_unclassified",
        "arbitrary_label",
        "provider request failed: Authorization: Bearer secret-token",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("failure-cause-rejected-{index}");
        insert_session(pool, &id).await;
        let result = sqlx::query("UPDATE sessions SET failure_cause = $1 WHERE id = $2")
            .bind(invalid_cause)
            .bind(&id)
            .execute(pool)
            .await;
        assert!(
            result.is_err(),
            "{invalid_cause:?} must not be persisted as a failure cause"
        );
    }
}

#[tokio::test]
async fn migration_183_applies_on_fresh_database() {
    with_temp_database("failure_cause_fresh", |db_url| async move {
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&db_url).await;
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");

        assert_failure_cause_schema_and_values(&pool).await;
        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_183_applies_after_prior_migrations_without_backfill() {
    with_temp_database("failure_cause_prior", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect prior migration database");
        insert_session(&pool, "failure-cause-existing").await;
        pool.close().await;

        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("reconnect prior migration database");
        apply_migration_183(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");
        let existing_cause: Option<String> = sqlx::query_scalar(
            "SELECT failure_cause FROM sessions WHERE id = 'failure-cause-existing'",
        )
        .fetch_one(&pool)
        .await
        .expect("read existing session failure cause");
        assert_eq!(
            existing_cause, None,
            "migration must not invent a backfill cause"
        );

        assert_failure_cause_schema_and_values(&pool).await;
        pool.close().await;
    })
    .await;
}
