//! Migration coverage for 195 — confidence ceiling normalization, legacy
//! access rebase, and the invocation-keyed `note_access_events` era (9xih).
//!
//! Every fixture row in this file is constructed by the test itself. No id,
//! project, or timestamp is taken from any particular deployment: the migration
//! is a structural predicate over whole tables and is asserted as such.

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPool, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 195;
const MIGRATION_FILE: &str = "195_note_confidence_access_repair.sql";
const MIGRATION_OPERATOR_ID: &str = "00000000-0000-7000-8000-000000000195";
const CREATOR_CONTRACT_VERSION: u64 = 142;

/// Must equal `djinn_db::repositories::note::CONFIDENCE_CEILING`.
const CONFIDENCE_CEILING: f64 = 0.975;

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
             ('{MIGRATION_OPERATOR_ID}', 9000000195, 'confidence-access-migration-operator') \
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

async fn apply_migration_195(conn: &mut PgConnection) {
    let sql =
        std::fs::read_to_string(migrations_dir().join(MIGRATION_FILE)).expect("read migration 195");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 195");
}

// ── Fixture helpers. Every id below is minted by the test. ───────────────────

struct SeededNote {
    id: String,
    confidence: f64,
    created_at: String,
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

/// Insert one note with a fully explicit pre-migration state so the assertions
/// below cannot accidentally be reading a schema default.
async fn seed_note(
    conn: &mut PgConnection,
    project_id: &str,
    slug: &str,
    confidence: f64,
    access_count: i64,
    created_at: &str,
    last_accessed: &str,
) -> SeededNote {
    let id = uuid::Uuid::now_v7().to_string();
    conn.execute(
        format!(
            "INSERT INTO notes \
                 (id, project_id, permalink, title, file_path, tags, content, scope_paths, \
                  confidence, access_count, created_at, updated_at, last_accessed) \
             VALUES ('{id}', '{project_id}', 'reference/{slug}', '{slug}', '', '[]', 'body', '[]', \
                     {confidence}, {access_count}, '{created_at}', '{created_at}', '{last_accessed}')"
        )
        .as_str(),
    )
    .await
    .expect("seed note");
    SeededNote {
        id,
        confidence,
        created_at: created_at.to_owned(),
    }
}

/// Insert one pre-9xih ledger row in migration 189's shape (no invocation id).
async fn seed_legacy_access_event(
    conn: &mut PgConnection,
    project_id: &str,
    note_id: &str,
    source: &str,
    created_at: &str,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    conn.execute(
        format!(
            "INSERT INTO note_access_events (id, project_id, note_id, source, created_at) \
             VALUES ('{id}', '{project_id}', '{note_id}', '{source}', '{created_at}')"
        )
        .as_str(),
    )
    .await
    .expect("seed legacy access event");
    id
}

async fn note_row(pool: &PgPool, note_id: &str) -> (f64, i64, String, String) {
    sqlx::query_as::<_, (f64, i64, String, String)>(
        "SELECT confidence, access_count, last_accessed, created_at FROM notes WHERE id = $1",
    )
    .bind(note_id)
    .fetch_one(pool)
    .await
    .expect("read migrated note")
}

async fn count_scalar(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .expect("count query")
}

async fn notes_confidence_column_default(pool: &PgPool) -> String {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = 'notes' AND column_name = 'confidence'",
    )
    .fetch_one(pool)
    .await
    .expect("read confidence column default")
    .unwrap_or_default()
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// AC2 + AC4 on a populated database, then AC2 + AC4 again after an idempotent
/// re-run. Every assertion reads state back out of Postgres.
#[tokio::test]
async fn migration_195_normalizes_confidence_rebases_access_and_is_idempotent() {
    with_temp_database("conf_access", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect migration database");
        apply_prior_migrations(&mut conn).await;

        let project_id = format!("p{}", uuid::Uuid::now_v7().simple());
        seed_project(&mut conn, &project_id).await;

        // Exactly 1.0 — the legacy default that made USER_CONFIRM demoting.
        let at_one = seed_note(
            &mut conn,
            &project_id,
            "at-one",
            1.0,
            7,
            "2026-01-01T00:00:00.000Z",
            "2026-05-05T05:05:05.000Z",
        )
        .await;
        // Below 1.0 — a real posterior. Must survive byte-for-byte.
        let below_one = seed_note(
            &mut conn,
            &project_id,
            "below-one",
            0.4,
            3,
            "2026-02-02T00:00:00.000Z",
            "2026-06-06T06:06:06.000Z",
        )
        .await;
        // Already at the ceiling, and already access-neutral: the migration
        // must be a no-op here even on its FIRST run.
        let already_neutral = seed_note(
            &mut conn,
            &project_id,
            "already-neutral",
            CONFIDENCE_CEILING,
            0,
            "2026-03-03T00:00:00.000Z",
            "2026-03-03T00:00:00.000Z",
        )
        .await;
        // Just above the floor — proves "only exactly 1.0" is not "anything
        // high".
        let near_floor = seed_note(
            &mut conn,
            &project_id,
            "near-floor",
            0.999,
            42,
            "2026-04-04T00:00:00.000Z",
            "2026-07-07T07:07:07.000Z",
        )
        .await;

        // Pre-9xih ledger rows of BOTH sources. These are the join side of the
        // shipped injected-pull-rate report and must survive the migration.
        //
        // CRUCIALLY, the first two share a `note_id`. That is the normal shape
        // of real data (a note read many times), and it is the shape that makes
        // the unique index on `(invocation_id, note_id)` non-trivial: all of
        // these rows will carry `invocation_id IS NULL`, so index creation is
        // only possible because Postgres treats NULLs as DISTINCT. If that
        // assumption were wrong, `apply_migration_195` below would fail
        // outright on a populated deployment.
        let legacy_read_event = seed_legacy_access_event(
            &mut conn,
            &project_id,
            &at_one.id,
            "memory_read",
            "2026-05-05T05:05:05.000Z",
        )
        .await;
        let legacy_duplicate_note_event = seed_legacy_access_event(
            &mut conn,
            &project_id,
            &at_one.id,
            "memory_read",
            "2026-05-05T05:05:06.000Z",
        )
        .await;
        let legacy_search_event = seed_legacy_access_event(
            &mut conn,
            &project_id,
            &below_one.id,
            "memory_search",
            "2026-06-06T06:06:06.000Z",
        )
        .await;
        assert_ne!(
            legacy_read_event, legacy_duplicate_note_event,
            "the two same-note legacy rows must be distinct rows"
        );

        apply_migration_195(&mut conn).await;

        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&db_url)
            .await
            .expect("connect assertion pool");

        // ── AC2: confidence ──────────────────────────────────────────────
        let (confidence, ..) = note_row(&pool, &at_one.id).await;
        assert_eq!(
            confidence, CONFIDENCE_CEILING,
            "exactly-1.0 confidence must normalize to the ceiling"
        );
        let (confidence, ..) = note_row(&pool, &below_one.id).await;
        assert_eq!(
            confidence, below_one.confidence,
            "a confidence below 1.0 is real evidence and must not be rewritten"
        );
        let (confidence, ..) = note_row(&pool, &near_floor.id).await;
        assert_eq!(
            confidence, near_floor.confidence,
            "0.999 is below 1.0 and must not be rewritten"
        );
        let (confidence, ..) = note_row(&pool, &already_neutral.id).await;
        assert_eq!(confidence, CONFIDENCE_CEILING);

        // New rows must now default to the ceiling. Asserted on the actual
        // stored default, not on the migration text.
        let default_expr = notes_confidence_column_default(&pool).await;
        assert!(
            default_expr.starts_with("0.975"),
            "notes.confidence default must be 0.975, got {default_expr:?}"
        );

        // ── AC4: access rebase ───────────────────────────────────────────
        for note in [&at_one, &below_one, &already_neutral, &near_floor] {
            let (_, access_count, last_accessed, created_at) = note_row(&pool, &note.id).await;
            assert_eq!(
                access_count, 0,
                "every legacy access_count must be rebased to 0"
            );
            assert_eq!(
                last_accessed, created_at,
                "last_accessed must be rebased to the note's own created_at"
            );
            assert_eq!(
                created_at, note.created_at,
                "created_at itself must not be rewritten"
            );
        }

        // ── AC4: the invocation-keyed era starts empty ───────────────────
        assert_eq!(
            count_scalar(
                &pool,
                "SELECT COUNT(*) FROM note_access_events WHERE invocation_id IS NOT NULL"
            )
            .await,
            0,
            "the 9xih ledger era must contain no rows immediately after migration"
        );

        // ...but the pre-9xih rows are NOT destroyed: they are the production
        // basis of the injected-pull-rate report.
        assert_eq!(
            count_scalar(
                &pool,
                "SELECT COUNT(*) FROM note_access_events WHERE invocation_id IS NULL"
            )
            .await,
            3,
            "pre-9xih ledger rows must survive with a NULL invocation_id"
        );
        for event_id in [
            &legacy_read_event,
            &legacy_duplicate_note_event,
            &legacy_search_event,
        ] {
            let invocation_id: Option<String> =
                sqlx::query_scalar("SELECT invocation_id FROM note_access_events WHERE id = $1")
                    .bind(event_id)
                    .fetch_one(&pool)
                    .await
                    .expect("read legacy event");
            assert_eq!(invocation_id, None);
        }

        // The shipped consumer must still see exactly what it saw before.
        //
        // `injected_pull_rate.rs` selects its numerator with
        // `source = 'memory_read'` over `(project_id, note_id)` and counts
        // `memory_search` rows separately for contrast. Replaying that exact
        // predicate shape proves the ALTER did not orphan the report's join
        // side — the failure mode that deleting or recreating the table would
        // have caused.
        assert_eq!(
            count_scalar(
                &pool,
                "SELECT COUNT(*) FROM note_access_events WHERE source = 'memory_read'"
            )
            .await,
            2,
            "the report's memory_read numerator rows must survive the migration"
        );
        assert_eq!(
            count_scalar(
                &pool,
                "SELECT COUNT(*) FROM note_access_events WHERE source = 'memory_search'"
            )
            .await,
            1,
            "the report's memory_search contrast rows must survive the migration"
        );
        // ...and the legacy rows are still reachable through the report's own
        // correlated-subquery shape (project + note + source + time ordering).
        assert_eq!(
            count_scalar(
                &pool,
                &format!(
                    "SELECT COUNT(*) FROM note_access_events e \
                      WHERE e.project_id = '{project_id}' \
                        AND e.note_id = '{}' \
                        AND e.source = 'memory_read' \
                        AND e.created_at::timestamptz \
                            > '2026-01-01T00:00:00.000Z'::timestamptz",
                    at_one.id
                )
            )
            .await,
            2,
            "the report's correlated subquery must still match the legacy rows"
        );

        // ── Idempotent re-run ────────────────────────────────────────────
        // First simulate the new era doing real work, so the re-run assertion
        // is about the migration's own statements and not about a table that
        // happens to be empty.
        let post_migration_invocation = format!("inv-{}", uuid::Uuid::now_v7().simple());
        sqlx::query(
            "INSERT INTO note_access_events \
                 (id, project_id, note_id, source, created_at, invocation_id) \
             VALUES ($1, $2, $3, 'memory_read', $4, $5)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&project_id)
        .bind(&at_one.id)
        .bind("2026-08-08T08:08:08.000Z")
        .bind(&post_migration_invocation)
        .execute(&pool)
        .await
        .expect("insert new-era ledger row");

        // The unique key must reject a replay of that exact invocation.
        let replay = sqlx::query(
            "INSERT INTO note_access_events \
                 (id, project_id, note_id, source, created_at, invocation_id) \
             VALUES ($1, $2, $3, 'memory_read', $4, $5)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&project_id)
        .bind(&at_one.id)
        .bind("2026-08-08T08:08:09.000Z")
        .bind(&post_migration_invocation)
        .execute(&pool)
        .await;
        assert!(
            replay.is_err(),
            "(invocation_id, note_id) must be unique so a replay cannot append a second row"
        );

        // Legacy-shaped rows keep working AFTER the index exists: a fourth row
        // with a NULL invocation_id, for a note that already has two, must
        // still insert. This is the runtime half of the "NULLs are distinct"
        // property — the migration proved it at index-creation time, this
        // proves the index does not block ongoing legacy-shaped writes.
        seed_legacy_access_event(
            &mut conn,
            &project_id,
            &at_one.id,
            "memory_read",
            "2026-08-09T09:09:09.000Z",
        )
        .await;

        apply_migration_195(&mut conn).await;

        for note in [&at_one, &below_one, &already_neutral, &near_floor] {
            let (confidence, access_count, last_accessed, created_at) =
                note_row(&pool, &note.id).await;
            let expected_confidence = if note.confidence == 1.0 {
                CONFIDENCE_CEILING
            } else {
                note.confidence
            };
            assert_eq!(
                confidence, expected_confidence,
                "re-running the migration must not move confidence again"
            );
            assert_eq!(access_count, 0);
            assert_eq!(last_accessed, created_at);
        }
        assert_eq!(
            count_scalar(
                &pool,
                "SELECT COUNT(*) FROM note_access_events WHERE invocation_id IS NOT NULL"
            )
            .await,
            1,
            "the re-run must not delete ledger rows written after the first run"
        );
        assert_eq!(
            count_scalar(
                &pool,
                "SELECT COUNT(*) FROM note_access_events WHERE invocation_id IS NULL"
            )
            .await,
            4,
            "the re-run must not delete legacy ledger rows"
        );

        pool.close().await;
        drop(conn);
    })
    .await;
}

/// The same migration against a database with ZERO notes and ZERO ledger rows.
///
/// This is the case that matters for an operator installing djinn for the first
/// time: every statement must be a well-formed no-op, and the resulting schema
/// must be identical to the populated case.
#[tokio::test]
async fn migration_195_is_a_safe_no_op_on_an_empty_database() {
    with_temp_database("conf_empty", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect migration database");
        apply_prior_migrations(&mut conn).await;
        apply_migration_195(&mut conn).await;

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect assertion pool");

        assert_eq!(count_scalar(&pool, "SELECT COUNT(*) FROM notes").await, 0);
        assert_eq!(
            count_scalar(&pool, "SELECT COUNT(*) FROM note_access_events").await,
            0
        );

        // The schema changes still landed.
        let default_expr = notes_confidence_column_default(&pool).await;
        assert!(
            default_expr.starts_with("0.975"),
            "fresh installs must get the 0.975 confidence default, got {default_expr:?}"
        );
        assert_eq!(
            count_scalar(
                &pool,
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_name = 'note_access_events' AND column_name = 'invocation_id'"
            )
            .await,
            1,
            "invocation_id must exist on a fresh install"
        );
        assert_eq!(
            count_scalar(
                &pool,
                "SELECT COUNT(*) FROM pg_indexes \
                 WHERE tablename = 'note_access_events' \
                   AND indexname = 'uq_note_access_events_invocation_note'"
            )
            .await,
            1,
            "the replay-key unique index must exist on a fresh install"
        );

        // A note created purely through the schema default lands at the
        // ceiling, so USER_CONFIRM can never demote it below an untouched peer.
        let project_id = format!("p{}", uuid::Uuid::now_v7().simple());
        seed_project(&mut conn, &project_id).await;
        let default_note_id = uuid::Uuid::now_v7().to_string();
        conn.execute(
            format!(
                "INSERT INTO notes (id, project_id, permalink, title, file_path, tags, content, scope_paths) \
                 VALUES ('{default_note_id}', '{project_id}', 'reference/defaulted', 'Defaulted', '', '[]', 'body', '[]')"
            )
            .as_str(),
        )
        .await
        .expect("insert note relying on the schema default");
        let (confidence, access_count, ..) = note_row(&pool, &default_note_id).await;
        assert_eq!(confidence, CONFIDENCE_CEILING);
        assert_eq!(access_count, 0);

        pool.close().await;
        drop(conn);
    })
    .await;
}
