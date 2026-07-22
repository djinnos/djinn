//! Migration 137 — immutable proposal-revision lint results and nullable
//! doctor-finding deduplication keys.
//!
//! The migration is additive: existing doctor findings keep a NULL key, while
//! lint rows are tied to their concrete immutable proposal revision and are
//! removed with that revision.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 137;
const MIGRATION_FILE: &str = "137_proposal_revision_lint_results.sql";

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
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read migrations directory")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let version = path
                .file_name()?
                .to_str()?
                .split_once('_')?
                .0
                .parse::<u64>()
                .ok()?;
            (path.extension().and_then(|extension| extension.to_str()) == Some("sql"))
                .then_some((version, path))
        })
        .collect();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    entries
}

async fn with_temp_database<T, Fut>(suffix: &str, f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let database_name = format!(
        "djinn_migration_{}_{}",
        suffix,
        uuid::Uuid::now_v7().simple()
    );
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect Postgres admin database");
    admin
        .execute(format!(r#"CREATE DATABASE "{database_name}""#).as_str())
        .await
        .expect("create migration database");
    drop(admin);

    let database_url = format!("{prefix}/{database_name}");
    let result = f(database_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect Postgres admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = '{database_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{database_name}""#).as_str())
        .await
        .expect("drop migration database");

    result
}

async fn apply_prior_migrations(connection: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        let sql = std::fs::read_to_string(&path).expect("read prior migration");
        connection
            .execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("apply {}: {error}", path.display()));
    }
}

async fn apply_migration_137(connection: &mut PgConnection) {
    let sql =
        std::fs::read_to_string(migrations_dir().join(MIGRATION_FILE)).expect("read migration 137");
    connection
        .execute(sql.as_str())
        .await
        .expect("apply migration 137");
}

async fn assert_schema(pool: &sqlx::PgPool) {
    for (column, data_type, nullable) in [
        ("proposal_id", "character varying", "NO"),
        ("revision_seq", "integer", "NO"),
        ("linter_version", "character varying", "NO"),
        ("revision_id", "character varying", "NO"),
        ("body_sha256", "character varying", "NO"),
        ("result_json", "jsonb", "NO"),
    ] {
        let (actual_type, actual_nullable): (String, String) = sqlx::query_as(
            "SELECT data_type, is_nullable FROM information_schema.columns \
             WHERE table_name = 'proposal_revision_lint_results' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|error| panic!("inspect lint result column {column}: {error}"));
        assert_eq!(actual_type, data_type, "lint result column {column} type");
        assert_eq!(
            actual_nullable, nullable,
            "lint result column {column} nullability"
        );
    }

    let primary_key: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname FROM pg_index i \
         JOIN pg_class t ON t.oid = i.indrelid \
         JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ANY(i.indkey) \
         WHERE t.relname = 'proposal_revision_lint_results' AND i.indisprimary \
         ORDER BY array_position(i.indkey, a.attnum)",
    )
    .fetch_all(pool)
    .await
    .expect("inspect lint result primary key");
    assert_eq!(
        primary_key,
        ["proposal_id", "revision_seq", "linter_version"]
    );

    let fk: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conname = 'proposal_revision_lint_results_revision_fk'",
    )
    .fetch_one(pool)
    .await
    .expect("inspect lint result foreign key");
    assert!(
        fk.contains("ON DELETE CASCADE"),
        "lint result FK must cascade: {fk}"
    );

    let (data_type, nullable): (String, String) = sqlx::query_as(
        "SELECT data_type, is_nullable FROM information_schema.columns \
         WHERE table_name = 'doctor_findings' AND column_name = 'deduplication_key'",
    )
    .fetch_one(pool)
    .await
    .expect("inspect doctor deduplication key");
    assert_eq!(data_type, "character varying");
    assert_eq!(nullable, "YES");

    let predicate: String = sqlx::query_scalar(
        "SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname = 'doctor_findings_deduplication_key_unique'",
    )
    .fetch_one(pool)
    .await
    .expect("inspect doctor deduplication partial index");
    assert!(
        predicate.contains("deduplication_key IS NOT NULL"),
        "doctor deduplication index must be partial: {predicate}"
    );
}

async fn assert_constraints_and_cascades(pool: &sqlx::PgPool) {
    sqlx::query(
        "INSERT INTO proposals (id, short_id, title) VALUES \
         ('proposal-lint-one', 'lint-one', 'Lint one'), \
         ('proposal-lint-two', 'lint-two', 'Lint two')",
    )
    .execute(pool)
    .await
    .expect("seed proposals");
    sqlx::query(
        "INSERT INTO proposal_revisions (id, proposal_id, seq, title, body) VALUES \
         ('revision-lint-one', 'proposal-lint-one', 1, 'Lint one', 'body one'), \
         ('revision-lint-two', 'proposal-lint-two', 1, 'Lint two', 'body two')",
    )
    .execute(pool)
    .await
    .expect("seed proposal revisions");

    for (proposal_id, revision_id) in [
        ("proposal-lint-one", "revision-lint-one"),
        ("proposal-lint-two", "revision-lint-two"),
    ] {
        sqlx::query(
            "INSERT INTO proposal_revision_lint_results \
             (proposal_id, revision_seq, linter_version, revision_id, body_sha256, result_json) \
             VALUES ($1, 1, 'spec-lint-v1', $2, $3, '{\"status\":\"clean\"}'::jsonb)",
        )
        .bind(proposal_id)
        .bind(revision_id)
        .bind("a".repeat(64))
        .execute(pool)
        .await
        .expect("insert lint result");
    }

    let duplicate = sqlx::query(
        "INSERT INTO proposal_revision_lint_results \
         (proposal_id, revision_seq, linter_version, revision_id, body_sha256, result_json) \
         VALUES ('proposal-lint-one', 1, 'spec-lint-v1', 'revision-lint-one', $1, '{}'::jsonb)",
    )
    .bind("b".repeat(64))
    .execute(pool)
    .await;
    assert!(duplicate.is_err(), "lint-result lookup key must be unique");

    for id in ["doctor-null-one", "doctor-null-two"] {
        sqlx::query(
            "INSERT INTO doctor_findings (id, check_name, severity, deduplication_key) \
             VALUES ($1, 'migration-test', 'info', NULL)",
        )
        .bind(id)
        .execute(pool)
        .await
        .expect("NULL doctor deduplication key should remain compatible");
    }
    sqlx::query(
        "INSERT INTO doctor_findings (id, check_name, severity, deduplication_key) \
         VALUES ('doctor-key-one', 'migration-test', 'info', 'same-key')",
    )
    .execute(pool)
    .await
    .expect("insert doctor finding with deduplication key");
    let duplicate = sqlx::query(
        "INSERT INTO doctor_findings (id, check_name, severity, deduplication_key) \
         VALUES ('doctor-key-two', 'migration-test', 'info', 'same-key')",
    )
    .execute(pool)
    .await;
    assert!(
        duplicate.is_err(),
        "non-NULL doctor deduplication keys must be unique"
    );

    sqlx::query("DELETE FROM proposal_revisions WHERE id = 'revision-lint-one'")
        .execute(pool)
        .await
        .expect("delete proposal revision");
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM proposal_revision_lint_results WHERE proposal_id = 'proposal-lint-one'",
    )
    .fetch_one(pool)
    .await
    .expect("count cascaded lint rows");
    assert_eq!(
        remaining, 0,
        "deleting a revision must cascade its lint result"
    );

    sqlx::query("DELETE FROM proposals WHERE id = 'proposal-lint-two'")
        .execute(pool)
        .await
        .expect("delete proposal");
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM proposal_revision_lint_results WHERE proposal_id = 'proposal-lint-two'",
    )
    .fetch_one(pool)
    .await
    .expect("count proposal-cascaded lint rows");
    assert_eq!(
        remaining, 0,
        "deleting a proposal must cascade lint results via revisions"
    );
}

#[tokio::test]
async fn migration_137_applies_cleanly_and_migrator_rerun_is_idempotent() {
    with_temp_database("fresh_lint_results", |database_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect migration database");
        djinn_db::test_support::apply_all_migrations_to_fresh_database(&database_url).await;
        sqlx::migrate!("./migrations_postgres")
            .run(&pool)
            .await
            .expect("rerun applied migrator");

        assert_schema(&pool).await;
        assert_constraints_and_cascades(&pool).await;
        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_137_is_additive_and_raw_rerun_preserves_null_doctor_rows() {
    with_temp_database("prior_lint_results", |database_url| async move {
        let mut connection = PgConnection::connect(&database_url)
            .await
            .expect("connect migration database");
        apply_prior_migrations(&mut connection).await;
        connection
            .execute(
                "INSERT INTO doctor_findings (id, check_name, severity) \
                 VALUES ('doctor-legacy-null', 'migration-test', 'info')",
            )
            .await
            .expect("seed legacy doctor finding before migration");

        apply_migration_137(&mut connection).await;
        apply_migration_137(&mut connection).await;
        drop(connection);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect migrated database");
        assert_schema(&pool).await;
        let legacy_key: Option<String> = sqlx::query_scalar(
            "SELECT deduplication_key FROM doctor_findings WHERE id = 'doctor-legacy-null'",
        )
        .fetch_one(&pool)
        .await
        .expect("read legacy doctor finding");
        assert_eq!(
            legacy_key, None,
            "migration must not backfill legacy doctor rows"
        );
        assert_constraints_and_cascades(&pool).await;
        pool.close().await;
    })
    .await;
}
