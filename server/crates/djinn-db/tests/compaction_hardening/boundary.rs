//! Migration-134 compatibility coverage for nullable compaction telemetry.
//!
//! This fixture intentionally builds a version-133 database, seeds a boundary
//! using the old column set, and then applies migration 134 before exercising
//! the public repository against the migrated database.

use std::path::PathBuf;

use djinn_db::{
    BeginCompactionParams, CompactionPhase, CompactionTrigger, CompleteCompactionParams, Database,
    DatabaseConnectConfig, PostgresDatabaseConfig, SessionCompactionBoundaryRepository,
};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

const MIGRATION_134: u64 = 134;
const PROJECT_ID: &str = "13400000-0000-0000-0000-000000000001";
const TASK_ID: &str = "13400000-0000-0000-0000-000000000002";
const SESSION_ID: &str = "13400000-0000-0000-0000-000000000003";
const HISTORIC_BOUNDARY_ID: &str = "13400000-0000-0000-0000-000000000004";

fn base_database_url() -> String {
    // `DATABASE_URL` is deliberately not consulted: it is a generic name that
    // can point at a real deployment. Task-run Pods set it to the same
    // sidecar value as `TEST_POSTGRES_URL`, so nothing is lost by ignoring it.
    djinn_db::test_database_base_url()
}

async fn apply_migration(conn: &mut PgConnection, target: u64) {
    let (_, path) = migration_entries()
        .into_iter()
        .find(|(version, _)| *version == target)
        .unwrap_or_else(|| panic!("migration {target} exists"));
    let sql = std::fs::read_to_string(&path).expect("read migration SQL");
    // The creator-contract migration refuses to run without a validated
    // designated operator bound to the session, so provision one before
    // crossing that boundary and unbind it afterwards.
    let is_creator_contract =
        target as i64 == djinn_db::migrations::CREATOR_CONTRACT_MIGRATION_VERSION;
    if is_creator_contract {
        sqlx::query(
            "INSERT INTO users (id, github_id, github_login) \
             VALUES ('user-boundary-operator', 9000000134, 'compaction-boundary-operator') \
             ON CONFLICT DO NOTHING",
        )
        .execute(&mut *conn)
        .await
        .expect("seed designated operator user");
        sqlx::query("SELECT set_config('djinn.migration_designated_operator_user_id', 'user-boundary-operator', false)")
            .execute(&mut *conn)
            .await
            .expect("bind designated operator GUC");
    }
    conn.execute(sql.as_str())
        .await
        .unwrap_or_else(|error| panic!("apply {}: {error}", path.display()));
    if is_creator_contract {
        sqlx::query("RESET djinn.migration_designated_operator_user_id")
            .execute(&mut *conn)
            .await
            .expect("unbind designated operator GUC");
    }
}

fn server_prefix(base: &str) -> String {
    base.rsplit_once('/')
        .map(|(prefix, _)| prefix)
        .unwrap_or(base)
        .trim_end_matches('/')
        .to_owned()
}

fn migration_entries() -> Vec<(u64, PathBuf)> {
    let mut entries: Vec<_> =
        std::fs::read_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres"))
            .expect("read migrations directory")
            .map(|entry| {
                let path = entry.expect("migration directory entry").path();
                let version = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.split_once('_'))
                    .and_then(|(version, _)| version.parse::<u64>().ok())
                    .unwrap_or_default();
                (version, path)
            })
            .filter(|(_, path)| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
            .collect();
    entries.sort_by_key(|(version, _)| *version);
    entries
}

async fn with_temp_database<T, Fut>(f: impl FnOnce(String) -> Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    let prefix = server_prefix(&base_database_url());
    let name = format!("djinn_compaction_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect PostgreSQL admin database");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create temporary compaction database");
    drop(admin);

    let database_url = format!("{prefix}/{name}");
    let result = f(database_url).await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect PostgreSQL admin database");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = '{name}' AND pid <> pg_backend_pid()"
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{name}""#).as_str())
        .await
        .expect("drop temporary compaction database");

    result
}

async fn apply_migrations_through(conn: &mut PgConnection, target: u64) {
    for (version, path) in migration_entries() {
        if version == 0 || version > target {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read migration SQL");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("apply {}: {error}", path.display()));
    }
}

async fn record_applied_migrations(conn: &mut PgConnection) {
    conn.execute(
        "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, success BOOLEAN NOT NULL)",
    )
    .await
    .expect("create migration ledger");
    for (version, _) in migration_entries() {
        sqlx::query("INSERT INTO _sqlx_migrations(version, success) VALUES ($1, TRUE)")
            .bind(version as i64)
            .execute(&mut *conn)
            .await
            .expect("record applied migration");
    }
}

async fn seed_pre_134_boundary(conn: &mut PgConnection) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ($1, 'compaction migration fixture', 'djinnos', 'compaction-fixture')",
    )
    .bind(PROJECT_ID)
    .execute(&mut *conn)
    .await
    .expect("seed project");
    sqlx::query(
        "INSERT INTO tasks \
         (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs) \
         VALUES ($1, $2, 'c134', 'compaction fixture', '', '', '[]'::jsonb, '[]'::jsonb, '[]'::jsonb)",
    )
    .bind(TASK_ID)
    .bind(PROJECT_ID)
    .execute(&mut *conn)
    .await
    .expect("seed task");
    sqlx::query(
        "INSERT INTO sessions (id, project_id, task_id, model_id, agent_type, status) \
         VALUES ($1, $2, $3, 'fixture-model', 'worker', 'active')",
    )
    .bind(SESSION_ID)
    .bind(PROJECT_ID)
    .bind(TASK_ID)
    .execute(&mut *conn)
    .await
    .expect("seed session");
    // This exact INSERT deliberately names no migration-134 telemetry column.
    sqlx::query(
        "INSERT INTO session_compaction_boundaries \
         (id, session_id, phase, schema_version, first_message_id, last_compacted_message_id, \
          first_retained_message_id, retained_tail_hash, summary_text, marker_metadata, created_at, completed_at) \
         VALUES ($1, $2, 'ended', 7, 'old-first', 'old-last', 'old-retained', 'old-tail-hash', \
                 'historic summary', '{\"source\":\"pre-134\"}'::jsonb, \
                 '2026-01-02T03:04:05.006Z', '2026-01-02T03:05:06.007Z')",
    )
    .bind(HISTORIC_BOUNDARY_ID)
    .bind(SESSION_ID)
    .execute(&mut *conn)
    .await
    .expect("seed pre-134 compaction boundary");
}

fn assert_historic_boundary(boundary: &djinn_db::CompactionBoundary) {
    assert_eq!(boundary.id, HISTORIC_BOUNDARY_ID);
    assert_eq!(boundary.session_id, SESSION_ID);
    assert_eq!(boundary.phase, CompactionPhase::Ended);
    assert_eq!(boundary.schema_version, 7);
    assert_eq!(boundary.first_message_id.as_deref(), Some("old-first"));
    assert_eq!(
        boundary.last_compacted_message_id.as_deref(),
        Some("old-last")
    );
    assert_eq!(
        boundary.first_retained_message_id.as_deref(),
        Some("old-retained")
    );
    assert_eq!(
        boundary.retained_tail_hash.as_deref(),
        Some("old-tail-hash")
    );
    assert_eq!(boundary.summary_text.as_deref(), Some("historic summary"));
    assert_eq!(
        boundary.marker_metadata.as_ref().unwrap()["source"],
        "pre-134"
    );
    assert_eq!(boundary.created_at, "2026-01-02T03:04:05.006Z");
    assert_eq!(
        boundary.completed_at.as_deref(),
        Some("2026-01-02T03:05:06.007Z")
    );
    assert_eq!(boundary.trigger, None);
    assert_eq!(boundary.current_context_tokens_before, None);
    assert_eq!(boundary.current_context_tokens_after, None);
}

#[tokio::test]
async fn nullable_trigger_telemetry_round_trips() {
    with_temp_database(|database_url| async move {
        let mut migration = PgConnection::connect(&database_url)
            .await
            .expect("connect temporary compaction database");

        apply_migrations_through(&mut migration, MIGRATION_134 - 1).await;
        seed_pre_134_boundary(&mut migration).await;
        apply_migration(&mut migration, MIGRATION_134).await;

        // Repository initialization validates the current migration ledger.
        // Apply any later, unrelated migrations only after migration 134 has
        // transformed the pre-change row, then expose that schema to the API.
        let latest_version = migration_entries()
            .last()
            .map(|(version, _)| *version)
            .expect("at least one migration");
        for version in (MIGRATION_134 + 1)..=latest_version {
            apply_migration(&mut migration, version).await;
        }
        record_applied_migrations(&mut migration).await;
        drop(migration);

        let database =
            Database::open_with_config(DatabaseConnectConfig::Postgres(PostgresDatabaseConfig {
                url: database_url,
            }))
            .expect("open migrated database");
        database
            .verify_and_mark_initialized()
            .await
            .expect("verify migrated database");
        let repository = SessionCompactionBoundaryRepository::new(database.clone());

        let historic = repository
            .fetch_by_id(HISTORIC_BOUNDARY_ID)
            .await
            .expect("read historic boundary through repository");
        assert_historic_boundary(&historic);
        let latest_historic = repository
            .latest_completed_boundary(SESSION_ID)
            .await
            .expect("read historic latest completed boundary")
            .expect("historic completed boundary");
        assert_historic_boundary(&latest_historic);

        for (index, trigger) in CompactionTrigger::ALL.into_iter().enumerate() {
            let before = 11_000 + (index as i64 * 137);
            let after = 2_000 + (index as i64 * 41);
            let started = repository
                .record_compaction_started(BeginCompactionParams {
                    session_id: SESSION_ID,
                    schema_version: 8 + index as i32,
                    first_message_id: Some("new-first"),
                    last_compacted_message_id: Some("new-last"),
                    first_retained_message_id: Some("new-retained"),
                    retained_tail_hash: Some("new-tail-hash"),
                    marker_metadata: None,
                    trigger: Some(trigger),
                    current_context_tokens_before: Some(before),
                })
                .await
                .expect("begin telemetry boundary");
            let completed = repository
                .complete_compaction_boundary(CompleteCompactionParams {
                    boundary_id: &started.id,
                    schema_version: 8 + index as i32,
                    first_message_id: Some("new-first"),
                    last_compacted_message_id: Some("new-last"),
                    first_retained_message_id: Some("new-retained"),
                    retained_tail_hash: Some("new-tail-hash"),
                    summary_text: trigger.as_str(),
                    marker_metadata: None,
                    current_context_tokens_after: Some(after),
                })
                .await
                .expect("complete telemetry boundary");
            assert_eq!(completed.trigger, Some(trigger));
            assert_eq!(completed.current_context_tokens_before, Some(before));
            assert_eq!(completed.current_context_tokens_after, Some(after));

            let fetched = repository
                .fetch_by_id(&started.id)
                .await
                .expect("direct telemetry boundary read");
            assert_eq!(fetched.trigger, Some(trigger));
            assert_eq!(fetched.current_context_tokens_before, Some(before));
            assert_eq!(fetched.current_context_tokens_after, Some(after));

            let latest = repository
                .latest_completed_boundary(SESSION_ID)
                .await
                .expect("compatible latest completed read")
                .expect("completed telemetry boundary");
            assert_eq!(latest.id, started.id);
            assert_eq!(latest.trigger, Some(trigger));
            assert_eq!(latest.current_context_tokens_before, Some(before));
            assert_eq!(latest.current_context_tokens_after, Some(after));
        }

        drop(repository);
        database.pool().close().await;
    })
    .await;
}
