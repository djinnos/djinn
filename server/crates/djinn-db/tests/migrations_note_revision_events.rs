//! Focused contract tests for migration 109's retained, project-scoped note ledger.

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

fn database_url() -> String {
    std::env::var("DJINN_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("TEST_POSTGRES_URL"))
        .expect("Postgres URL is required for migration tests")
}

async fn with_database(f: impl AsyncFnOnce(sqlx::PgPool)) {
    let base = database_url();
    let prefix = base.rsplit_once('/').expect("database URL path").0;
    let name = format!("djinn_revision_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect admin");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create database");
    drop(admin);

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&format!("{prefix}/{name}"))
        .await
        .expect("connect test database");
    sqlx::migrate!("./migrations_postgres")
        .run(&pool)
        .await
        .expect("apply migrations");
    f(pool.clone()).await;
    pool.close().await;

    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("reconnect admin");
    let _ = admin
        .execute(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{name}' AND pid <> pg_backend_pid()").as_str())
        .await;
    admin
        .execute(format!(r#"DROP DATABASE "{name}""#).as_str())
        .await
        .expect("drop database");
}

async fn seed_project(pool: &sqlx::PgPool, id: &str) {
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $1, 'test', $1)",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("seed project");
}

async fn insert_created(
    pool: &sqlx::PgPool,
    id: &str,
    project: &str,
    note: &str,
    seq: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO note_revision_events \
         (id, project_id, note_id, note_seq, event_kind, content_after, confidence_after, actor_kind, actor_id, reason) \
         VALUES ($1, $2, $3, $4, 'created', 'content', 0.8, 'agent', 'agent-1', 'created note')",
    )
    .bind(id)
    .bind(project)
    .bind(note)
    .bind(seq)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn migration_109_enforces_ledger_shape_indexes_and_retention() {
    with_database(async |pool| {
        seed_project(&pool, "revision-project").await;
        insert_created(
            &pool,
            "11111111-1111-1111-1111-111111111111",
            "revision-project",
            "note-retained",
            1,
        )
        .await
        .expect("valid created event");

        // Per-note sequences are positive and unique inside the project.
        assert!(insert_created(&pool, "22222222-2222-2222-2222-222222222222", "revision-project", "note-retained", 1).await.is_err());
        assert!(insert_created(&pool, "33333333-3333-3333-3333-333333333333", "revision-project", "note-retained", 0).await.is_err());

        // Extraction skips have no logical note identity, require provenance,
        // and the actor/subsystem shape is closed by the database CHECKs.
        let invalid_skip = sqlx::query(
            "INSERT INTO note_revision_events (id, project_id, event_kind, actor_kind, subsystem, reason) \
             VALUES ('44444444-4444-4444-4444-444444444444', 'revision-project', 'extraction_skipped', 'system', 'extractor', 'no output')",
        )
        .execute(&pool)
        .await;
        assert!(invalid_skip.is_err());
        let invalid_reason = sqlx::query(
            "INSERT INTO note_revision_events (id, project_id, event_kind, actor_kind, subsystem, session_id, reason) \
             VALUES ('55555555-5555-5555-5555-555555555555', 'revision-project', 'extraction_skipped', 'system', 'extractor', 'session-1', ' ') ",
        )
        .execute(&pool)
        .await;
        assert!(invalid_reason.is_err());
        // The database, not just the Rust reason constructor, must reject the
        // standard whitespace characters that PostgreSQL's default btrim does
        // not remove.
        for (id, reason) in [
            ("66666666-6666-6666-6666-666666666666", "\t"),
            ("77777777-7777-7777-7777-777777777777", "valid\n"),
        ] {
            let invalid_whitespace_reason = sqlx::query(
                "INSERT INTO note_revision_events \
                 (id, project_id, event_kind, actor_kind, subsystem, session_id, reason) \
                 VALUES ($1, 'revision-project', 'extraction_skipped', 'system', 'extractor', 'session-1', $2)",
            )
            .bind(id)
            .bind(reason)
            .execute(&pool)
            .await;
            assert!(
                invalid_whitespace_reason.is_err(),
                "reason {reason:?} must be non-blank and trimmed"
            );
        }

        for index in [
            "note_revision_events_project_note_history",
            "note_revision_events_project_session_cursor",
            "note_revision_events_project_task_cursor",
            "note_revision_events_project_task_run_cursor",
        ] {
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pg_indexes WHERE tablename = 'note_revision_events' AND indexname = $1")
                .bind(index)
                .fetch_one(&pool)
                .await
                .expect("inspect ledger index");
            assert_eq!(count, 1, "missing {index}");
        }

        // No FK to notes: direct note deletion retains its logical history.
        sqlx::query("INSERT INTO notes (id, project_id, permalink, title, file_path) VALUES ('note-retained', 'revision-project', 'retained', 'Retained', 'retained.md')")
            .execute(&pool).await.expect("seed note");
        sqlx::query("DELETE FROM notes WHERE id = 'note-retained'").execute(&pool).await.expect("delete note");
        let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM note_revision_events WHERE note_id = 'note-retained'").fetch_one(&pool).await.expect("count retained history");
        assert_eq!(retained, 1);

        // The project boundary is the sole cascade owner for ledger rows.
        sqlx::query("DELETE FROM projects WHERE id = 'revision-project'").execute(&pool).await.expect("delete project");
        let erased: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM note_revision_events").fetch_one(&pool).await.expect("count erased history");
        assert_eq!(erased, 0);
    }).await;
}
