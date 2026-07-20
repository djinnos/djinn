//! Migration 135 is additive: it must not infer a lifecycle transition time
//! from `updated_at` for existing inactive notes.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 135;
const MIGRATION_FILE: &str = "135_note_lifecycle_changed_at.sql";

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
        .expect("read migrations dir")
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

async fn apply_prior_migrations(conn: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        let sql = std::fs::read_to_string(&path).expect("read prior migration");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("apply {}: {error}", path.display()));
    }
}

#[tokio::test]
async fn existing_inactive_note_retains_null_lifecycle_transition_time() {
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let database_name = format!("djinn_note_lifecycle_{}", uuid::Uuid::now_v7().simple());
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
    let mut connection = PgConnection::connect(&database_url)
        .await
        .expect("connect migration database");
    apply_prior_migrations(&mut connection).await;
    connection
        .execute(
            "INSERT INTO projects (id, name, github_owner, github_repo) \
             VALUES ('lifecycle-project', 'lifecycle-project', 'lifecycle-owner', 'lifecycle-repo')",
        )
        .await
        .expect("seed project");
    connection
        .execute(
            "INSERT INTO notes (id, project_id, permalink, title, file_path, status, tags, content, scope_paths) \
             VALUES ('inactive-note', 'lifecycle-project', 'reference/inactive', 'Inactive', '', 'archived', '[]', 'body', '[]')",
        )
        .await
        .expect("seed archived note before migration");
    let migration =
        std::fs::read_to_string(migrations_dir().join(MIGRATION_FILE)).expect("read migration 135");
    connection
        .execute(migration.as_str())
        .await
        .expect("apply migration 135");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect assertion pool");
    let lifecycle_changed_at: Option<String> =
        sqlx::query_scalar("SELECT lifecycle_changed_at FROM notes WHERE id = 'inactive-note'")
            .fetch_one(&pool)
            .await
            .expect("read migrated inactive note");
    assert_eq!(lifecycle_changed_at, None);
    drop(pool);
    drop(connection);

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect Postgres admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{database_name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{database_name}""#).as_str())
        .await
        .expect("drop migration database");
}
