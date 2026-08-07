//! Migration coverage for 201 — return every stored `notes.confidence` to
//! `[CONFIDENCE_FLOOR, CONFIDENCE_CEILING]` (proposal 9xih follow-up).
//!
//! Migration 197 normalized the column default and the exactly-1.0 rows on the
//! premise that the ceiling bounds `notes.confidence`. It did not, because
//! `mutate_with_revision` wrote caller-supplied values with no range check and
//! the extraction duplicate boost re-derived the Bayesian posterior without
//! `bayesian_update`'s clamp. This migration repairs the rows that produced.
//!
//! Every fixture row in this file is constructed by the test itself. No id,
//! project, or timestamp is taken from any particular deployment: the migration
//! is a structural predicate over a whole table and is asserted as such.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 201;
const MIGRATION_FILE: &str = "201_note_confidence_range_repair.sql";
const MIGRATION_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000201";
const CREATOR_CONTRACT_VERSION: u64 = 142;

/// Must equal `djinn_db::repositories::note::CONFIDENCE_CEILING`.
const CONFIDENCE_CEILING: f64 = 0.975;
/// Must equal `djinn_db::repositories::note::CONFIDENCE_FLOOR`.
const CONFIDENCE_FLOOR: f64 = 0.025;

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
    let base = djinn_db::test_database_base_url();
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
             ('{MIGRATION_OPERATOR_ID}', 9000000201, 'confidence-range-migration-operator') \
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

async fn apply_migration_201(conn: &mut PgConnection) {
    let sql =
        std::fs::read_to_string(migrations_dir().join(MIGRATION_FILE)).expect("read migration 201");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 201");
}

async fn seed_project(conn: &mut PgConnection, project_id: &str) {
    conn.execute(
        format!(
            "INSERT INTO projects (id, name, github_owner, github_repo) \
             VALUES ('{project_id}', '{project_id}', 'owner-{project_id}', 'repo-{project_id}')"
        )
        .as_str(),
    )
    .await
    .expect("seed project");
}

/// Insert one note with an explicit confidence, bypassing the column default so
/// the assertions cannot accidentally be reading it.
async fn seed_note(
    conn: &mut PgConnection,
    project_id: &str,
    slug: &str,
    confidence: f64,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    conn.execute(
        format!(
            "INSERT INTO notes \
                 (id, project_id, permalink, title, file_path, tags, content, scope_paths, \
                  confidence, created_at, updated_at, last_accessed) \
             VALUES ('{id}', '{project_id}', 'reference/{slug}', '{slug}', '', '[]', 'body', '[]', \
                     {confidence}, '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z', \
                     '2026-01-01T00:00:00.000Z')"
        )
        .as_str(),
    )
    .await
    .expect("seed note");
    id
}

async fn confidence_of(pool: &PgPool, note_id: &str) -> f64 {
    sqlx::query_scalar::<_, f64>("SELECT confidence FROM notes WHERE id = $1")
        .bind(note_id)
        .fetch_one(pool)
        .await
        .expect("read migrated note confidence")
}

/// Every stored confidence lands inside `[FLOOR, CEILING]`; nothing already
/// inside the range is touched; and a second application changes nothing.
#[tokio::test]
async fn migration_201_clamps_out_of_range_confidence_and_is_idempotent() {
    with_temp_database("conf_range", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect migration database");
        apply_prior_migrations(&mut conn).await;

        let project_id = format!("p{}", uuid::Uuid::now_v7().simple());
        seed_project(&mut conn, &project_id).await;

        // The exact orbit the unclamped extraction duplicate boost produced:
        // bayesian_update applied repeatedly to a ceiling-normalized note with
        // the 0.65 duplicate signal, with the clamp missing.
        let just_over = seed_note(&mut conn, &project_id, "just-over", 0.9863813229571984).await;
        let far_over = seed_note(&mut conn, &project_id, "far-over", 0.999999999999202).await;
        let at_one = seed_note(&mut conn, &project_id, "at-one", 1.0).await;
        // The enrichment entity/claim writer's old below-floor value.
        let at_zero = seed_note(&mut conn, &project_id, "at-zero", 0.0).await;

        // In-range rows. These are real posteriors and must survive byte for
        // byte — including the two endpoints, which the WHERE clauses use
        // strict comparisons to exclude.
        let at_ceiling = seed_note(&mut conn, &project_id, "at-ceiling", CONFIDENCE_CEILING).await;
        let at_floor = seed_note(&mut conn, &project_id, "at-floor", CONFIDENCE_FLOOR).await;
        let extraction_prior = seed_note(&mut conn, &project_id, "prior", 0.5).await;
        let decayed = seed_note(&mut conn, &project_id, "decayed", 0.15).await;

        apply_migration_201(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .expect("connect migrated pool");

        for (id, label) in [
            (&just_over, "just over the ceiling"),
            (&far_over, "far over the ceiling"),
            (&at_one, "exactly 1.0"),
        ] {
            assert_eq!(
                confidence_of(&pool, id).await,
                CONFIDENCE_CEILING,
                "{label} must be clamped to CONFIDENCE_CEILING"
            );
        }
        assert_eq!(
            confidence_of(&pool, &at_zero).await,
            CONFIDENCE_FLOOR,
            "a below-floor row must be clamped to CONFIDENCE_FLOOR"
        );

        for (id, expected, label) in [
            (&at_ceiling, CONFIDENCE_CEILING, "a row at the ceiling"),
            (&at_floor, CONFIDENCE_FLOOR, "a row at the floor"),
            (&extraction_prior, 0.5, "the session-extraction prior"),
            (&decayed, 0.15, "a decayed posterior"),
        ] {
            assert_eq!(
                confidence_of(&pool, id).await,
                expected,
                "{label} is already in range and must not be rewritten"
            );
        }

        // Whole-table predicate, not just the seeded ids.
        let out_of_range: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM notes WHERE confidence < $1 OR confidence > $2",
        )
        .bind(CONFIDENCE_FLOOR)
        .bind(CONFIDENCE_CEILING)
        .fetch_one(&pool)
        .await
        .expect("count out-of-range rows");
        assert_eq!(
            out_of_range, 0,
            "no note may sit outside the epistemic range"
        );

        // Idempotence: a second application is a no-op over every row.
        let before: Vec<(String, f64)> =
            sqlx::query_as("SELECT id, confidence FROM notes ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("snapshot before re-run");
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("reconnect for idempotence run");
        apply_migration_201(&mut conn).await;
        drop(conn);
        let after: Vec<(String, f64)> =
            sqlx::query_as("SELECT id, confidence FROM notes ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("snapshot after re-run");
        assert_eq!(
            before, after,
            "re-applying migration 201 must change nothing"
        );

        pool.close().await;
    })
    .await;
}

/// The other half of AC2: the stored `notes.confidence` column default must
/// equal the constant the production creation path writes.
///
/// These two numbers are structurally independent. The column default lives in
/// migration 197; the value new authored notes actually receive is a Rust
/// constant bound explicitly by `mutate_with_revision`, so the default is never
/// exercised on that path and cannot enforce anything. They silently disagreed
/// from #2168 until this change — the default said `0.975`, every authored note
/// was created at `0.5`, and the migration test stayed green because it only
/// ever asked whether the default EXISTED.
///
/// The behavioural half — that `memory_write` persists `CONFIDENCE_CEILING` —
/// is asserted through the repository layer in djinn-control-plane's
/// `memory_write_creates_notes_at_the_confidence_ceiling`. This is the half
/// that needs `information_schema`, so it lives here rather than crossing the
/// raw-SQL boundary out of djinn-db.
#[tokio::test]
async fn stored_confidence_default_equals_the_constant_production_writes() {
    // Pin the local copy to the real constant first, so a change to
    // `CONFIDENCE_CEILING` cannot leave this file asserting a stale literal.
    assert_eq!(
        CONFIDENCE_CEILING,
        djinn_db::repositories::note::CONFIDENCE_CEILING,
        "this file's CONFIDENCE_CEILING copy has drifted from the production constant"
    );
    assert_eq!(
        CONFIDENCE_FLOOR,
        djinn_db::repositories::note::CONFIDENCE_FLOOR,
        "this file's CONFIDENCE_FLOOR copy has drifted from the production constant"
    );

    with_temp_database("conf_default", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect migration database");
        apply_prior_migrations(&mut conn).await;
        apply_migration_201(&mut conn).await;

        let column_default: Option<String> = sqlx::query_scalar(
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name = 'notes' AND column_name = 'confidence'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("read stored column default");
        let column_default = column_default.expect("notes.confidence has a stored default");

        // Postgres renders the default with a type annotation, e.g. `0.975`.
        // Compare on the leading numeric literal rather than the whole string.
        let rendered = column_default
            .split("::")
            .next()
            .unwrap_or(&column_default)
            .trim()
            .to_owned();
        let parsed: f64 = rendered.parse().unwrap_or_else(|_| {
            panic!("notes.confidence default `{column_default}` is not numeric")
        });
        assert_eq!(
            parsed,
            djinn_db::repositories::note::CONFIDENCE_CEILING,
            "stored notes.confidence default `{column_default}` disagrees with the constant \
             the production creation path writes"
        );
    })
    .await;
}

/// Negative control for the whole file: on a database with no notes at all the
/// migration still applies cleanly. A deployment-neutral migration must be
/// correct for an operator whose `notes` table is empty.
#[tokio::test]
async fn migration_201_applies_to_an_empty_notes_table() {
    with_temp_database("conf_range_empty", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect migration database");
        apply_prior_migrations(&mut conn).await;
        apply_migration_201(&mut conn).await;

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM notes")
            .fetch_one(&mut conn)
            .await
            .expect("count notes");
        assert_eq!(count, 0);
    })
    .await;
}
