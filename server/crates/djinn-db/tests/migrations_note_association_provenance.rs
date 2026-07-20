//! Migration tests for migration 97 (`97_note_association_provenance.sql`):
//! the source-aware note-association substrate for epic ao5x / proposal j9j1.
//!
//! Verifies the provenance-ready migration applies cleanly on a fresh database
//! AND on top of the prior schema (additive ordering), and that:
//!   - the `source` column exists (NOT NULL DEFAULT 'session_co_access');
//!   - the embedding-provenance columns (`confidence`, `algorithm_version`,
//!     `embedding_model`, `embedding_dim`, `last_refreshed_at`) exist and are
//!     NULL-able;
//!   - the widened `kind` CHECK constraint accepts `authored` and
//!     `embedding_related`;
//!   - the primary key is the four-column `(note_a_id, note_b_id, kind,
//!     source)` tuple;
//!   - the `source` index exists;
//!   - legacy rows written against the pre-97 schema (no `source` column) are
//!     backfilled with `source = 'session_co_access'` and remain readable, so
//!     the existing Hebbian co-access substrate is preserved.
//!
//! Mirrors the harness in `migrations_liveness_evidence_outcomes.rs`
//! (migration 95) and `migrations_sessions_parked_reason.rs` (migration 59).

use std::path::{Path, PathBuf};

use sqlx::postgres::{PgConnection, PgPoolOptions};
use sqlx::{Connection, Executor};

const MIGRATION_VERSION: u64 = 97;
const MIGRATION_FILE: &str = "97_note_association_provenance.sql";

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
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let version = name
                .split_once('_')
                .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
                .unwrap_or(0);
            (version, path)
        })
        .filter(|(_, path)| path.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    entries.sort_by(|(av, ap), (bv, bp)| {
        av.cmp(bv).then_with(|| {
            ap.file_name()
                .unwrap_or_default()
                .cmp(bp.file_name().unwrap_or_default())
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
    let db_name = format!(
        "djinn_migration_{}_{}",
        suffix,
        uuid::Uuid::now_v7().simple()
    );
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

/// Apply every migration whose version prefix is strictly less than
/// `MIGRATION_VERSION`. This is the "prior migrations" path used to verify
/// that migration 97 is additive on top of the entire V1..V96 chain.
async fn apply_prior_migrations(conn: &mut PgConnection) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version >= MIGRATION_VERSION {
            break;
        }
        if version == 0 {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read migration sql");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|err| panic!("apply migration {} failed: {err}", path.display()));
    }
}

async fn apply_migration_97(conn: &mut PgConnection) {
    let migration = migrations_dir().join(MIGRATION_FILE);
    let sql = std::fs::read_to_string(&migration).expect("read migration 97 sql");
    conn.execute(sql.as_str())
        .await
        .expect("apply migration 97 after prior migrations");
}

/// Assert every column / constraint / index this migration is supposed to
/// install is present. Shared by the `fresh` and `prior` tests so any drift
/// is caught in both code paths.
///
/// This helper seeds its own throwaway `projects` / `notes` rows so the probe
/// `note_associations` inserts satisfy the `note_a_id`/`note_b_id` FOREIGN KEY
/// constraints, then cleans them all up at the end so they don't interfere
/// with `assert_legacy_rows_backfilled` in the `prior` test.
async fn assert_provenance_schema(pool: &sqlx::PgPool) {
    // Seed the project + notes the probe inserts reference.
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('proj-schema-probe', 'proj-schema-probe', 'djinnos', 'probe') \
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed schema-probe project");
    for id in [
        "check-authored-a",
        "check-authored-b",
        "check-emb-a",
        "check-emb-b",
        "pk-a",
        "pk-b",
    ] {
        sqlx::query(
            "INSERT INTO notes (id, project_id, permalink, title, file_path) \
             VALUES ($1, 'proj-schema-probe', $2, $1, 'probe.md') \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id)
        .bind(format!("reference/{id}"))
        .execute(pool)
        .await
        .expect("seed schema-probe note");
    }

    // ── source column: NOT NULL DEFAULT 'session_co_access' ─────────────────
    let (data_type, is_nullable, column_default): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT data_type, is_nullable, column_default \
             FROM information_schema.columns \
             WHERE table_name = 'note_associations' AND column_name = 'source'",
        )
        .fetch_one(pool)
        .await
        .expect("inspect note_associations.source");
    assert_eq!(
        data_type.as_deref(),
        Some("character varying"),
        "source column should be VARCHAR, got {data_type:?}"
    );
    assert_eq!(
        is_nullable.as_deref(),
        Some("NO"),
        "source column should be NOT NULL, got {is_nullable:?}"
    );
    assert!(
        column_default
            .as_deref()
            .unwrap_or_default()
            .contains("'session_co_access'"),
        "source column should DEFAULT 'session_co_access', got {column_default:?}"
    );

    // ── embedding-provenance columns: all NULL-able ────────────────────────
    for (column, data_type) in [
        ("confidence", "double precision"),
        ("algorithm_version", "character varying"),
        ("embedding_model", "character varying"),
        ("embedding_dim", "integer"),
        ("last_refreshed_at", "character varying"),
    ] {
        let (actual_type, actual_nullable): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_name = 'note_associations' AND column_name = $1",
        )
        .bind(column)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("inspect note_associations.{column}: {e}"));
        assert_eq!(
            actual_type.as_deref(),
            Some(data_type),
            "note_associations.{column} should be {data_type}, got {actual_type:?}"
        );
        assert_eq!(
            actual_nullable.as_deref(),
            Some("YES"),
            "note_associations.{column} nullability should be YES, got {actual_nullable:?}"
        );
    }

    // ── widened kind CHECK constraint accepts authored + embedding_related ──
    // Inserting an 'authored' or 'embedding_related' row must succeed; the
    // CHECK constraint no longer rejects them.
    sqlx::query(
        "INSERT INTO note_associations \
         (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source) \
         VALUES ('check-authored-a', 'check-authored-b', 0.1, 0, '2026-01-01T00:00:00.000Z', 'authored', 'llm_enrichment')",
    )
    .execute(pool)
    .await
    .expect("CHECK constraint must accept kind='authored'");
    sqlx::query(
        "INSERT INTO note_associations \
         (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source, confidence, embedding_model) \
         VALUES ('check-emb-a', 'check-emb-b', 0.2, 0, '2026-01-01T00:00:00.000Z', 'embedding_related', 'embedding_similarity', 0.9, 'test')",
    )
    .execute(pool)
    .await
    .expect("CHECK constraint must accept kind='embedding_related'");

    // ── four-column primary key ────────────────────────────────────────────
    // A duplicate (note_a_id, note_b_id, kind, source) must violate the PK,
    // but a different (kind, source) for the same pair must coexist.
    sqlx::query(
        "INSERT INTO note_associations \
         (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source) \
         VALUES ('pk-a', 'pk-b', 0.1, 1, '2026-01-01T00:00:00.000Z', 'co_access', 'session_co_access')",
    )
    .execute(pool)
    .await
    .expect("seed co_access row for PK test");
    // Same quadruple -> must fail.
    let dup = sqlx::query(
        "INSERT INTO note_associations \
         (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source) \
         VALUES ('pk-a', 'pk-b', 0.5, 1, '2026-01-01T00:00:00.000Z', 'co_access', 'session_co_access')",
    )
    .execute(pool)
    .await;
    assert!(
        dup.is_err(),
        "duplicate (note_a_id, note_b_id, kind, source) must violate the primary key"
    );
    // Different source for the same pair -> must coexist.
    sqlx::query(
        "INSERT INTO note_associations \
         (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source) \
         VALUES ('pk-a', 'pk-b', 0.7, 0, '2026-01-01T00:00:00.000Z', 'embedding_related', 'embedding_similarity')",
    )
    .execute(pool)
    .await
    .expect("different (kind, source) for the same pair must coexist");

    let pair_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM note_associations \
         WHERE note_a_id = 'pk-a' AND note_b_id = 'pk-b'",
    )
    .fetch_one(pool)
    .await
    .expect("count PK-test rows");
    assert_eq!(
        pair_rows, 2,
        "expected 2 coexistent rows for the same canonical pair, got {pair_rows}"
    );

    // ── source index exists ────────────────────────────────────────────────
    let source_index: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes \
         WHERE tablename = 'note_associations' AND indexname = 'idx_note_associations_source'",
    )
    .fetch_one(pool)
    .await
    .expect("inspect idx_note_associations_source");
    assert_eq!(
        source_index, 1,
        "idx_note_associations_source index must exist"
    );

    // Clean up the assertion probe rows so they don't affect the
    // backfill-readability assertions run after this helper. The project is
    // left in place (idempotent) — only the probe associations/notes are
    // removed.
    sqlx::query(
        "DELETE FROM note_associations \
         WHERE note_a_id LIKE 'check-%' OR note_a_id = 'pk-a' OR note_a_id LIKE 'pk-%'",
    )
    .execute(pool)
    .await
    .expect("clean up provenance-schema probe rows");
}

/// Seed legacy `note_associations` rows against the PRE-migration-97 schema.
///
/// The rows are written with only the columns that existed before migration 97
/// (`source` and the embedding-provenance columns are deliberately omitted),
/// so the backfill DEFAULT on `source` is what populates them when migration 97
/// runs. Two rows are seeded: a Hebbian co_access edge and a typed
/// `derived_from` edge, both under the old `(note_a_id, note_b_id)` primary
/// key.
async fn seed_legacy_association_rows(pool: &sqlx::PgPool) {
    // A project + two notes are required because `note_associations` has FK
    // constraints to `notes(id)` with ON DELETE CASCADE.
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) \
         VALUES ('proj-legacy', 'proj-legacy', 'djinnos', 'djinn-legacy')",
    )
    .execute(pool)
    .await
    .expect("seed legacy project");

    // `notes` rows: minimal column set (tags/scope_paths/content carry
    // DEFAULTs after migration 4).
    sqlx::query(
        "INSERT INTO notes (id, project_id, permalink, title, file_path) \
         VALUES ('note-alpha', 'proj-legacy', 'reference/alpha', 'Alpha', 'alpha.md')",
    )
    .execute(pool)
    .await
    .expect("seed legacy note alpha");
    sqlx::query(
        "INSERT INTO notes (id, project_id, permalink, title, file_path) \
         VALUES ('note-beta', 'proj-legacy', 'reference/beta', 'Beta', 'beta.md')",
    )
    .execute(pool)
    .await
    .expect("seed legacy note beta");

    // Legacy co_access edge (old PK: note_a_id, note_b_id). The `kind` column
    // (added by migration 35) carries its DEFAULT 'co_access'; `source` does
    // not exist yet at this point.
    sqlx::query(
        "INSERT INTO note_associations \
         (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind) \
         VALUES ('note-alpha', 'note-beta', 0.42, 7, '2026-01-02T00:00:00.000Z', 'co_access')",
    )
    .execute(pool)
    .await
    .expect("seed legacy co_access association");
}

/// Assert the legacy rows seeded by `seed_legacy_association_rows` survived
/// migration 97 and carry the expected post-migration defaults.
async fn assert_legacy_rows_backfilled(pool: &sqlx::PgPool) {
    // The single legacy co_access row must now read back with
    // source='session_co_access' (the DEFAULT backfill) and the provenance
    // columns NULL (populated only by the provenance-rich upsert path).
    let source: String = sqlx::query_scalar(
        "SELECT source FROM note_associations \
         WHERE note_a_id = 'note-alpha' AND note_b_id = 'note-beta'",
    )
    .fetch_one(pool)
    .await
    .expect("load backfilled legacy association source");
    assert_eq!(
        source, "session_co_access",
        "legacy row must be backfilled with source='session_co_access'"
    );

    // No provenance column may be populated for a legacy row. Counting the
    // non-NULL provenance columns in one query keeps the assertion simple.
    let non_null_provenance: i32 = sqlx::query_scalar(
        "SELECT (confidence IS NOT NULL)::int \
              + (algorithm_version IS NOT NULL)::int \
              + (embedding_model IS NOT NULL)::int \
              + (embedding_dim IS NOT NULL)::int \
              + (last_refreshed_at IS NOT NULL)::int \
         FROM note_associations \
         WHERE note_a_id = 'note-alpha' AND note_b_id = 'note-beta'",
    )
    .fetch_one(pool)
    .await
    .expect("count non-NULL provenance columns on legacy row");
    assert_eq!(
        non_null_provenance, 0,
        "legacy row must have all provenance columns NULL"
    );

    // The pre-existing weight/count must be preserved (migration is additive;
    // it does not rewrite application data).
    let (weight, count): (f64, i64) = sqlx::query_as(
        "SELECT weight, co_access_count FROM note_associations \
         WHERE note_a_id = 'note-alpha' AND note_b_id = 'note-beta'",
    )
    .fetch_one(pool)
    .await
    .expect("load backfilled legacy association weight/count");
    assert!(
        (weight - 0.42).abs() < 1e-12,
        "legacy weight must be preserved, got {weight}"
    );
    assert_eq!(count, 7, "legacy co_access_count must be preserved");
}

#[tokio::test]
async fn migration_97_applies_on_fresh_database() {
    with_temp_database("fresh_provenance", |db_url| async move {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect fresh migration database");
        sqlx::migrate!("./migrations_postgres")
            .run(&pool)
            .await
            .expect("apply all migrations to fresh database");

        assert_provenance_schema(&pool).await;

        pool.close().await;
    })
    .await;
}

#[tokio::test]
async fn migration_97_backfills_legacy_rows_after_prior_migrations() {
    with_temp_database("prior_provenance", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("connect prior migration database");
        apply_prior_migrations(&mut conn).await;
        drop(conn);

        // Seed legacy rows BEFORE migration 97 applies so we can prove the
        // `source` DEFAULT backfills pre-existing rows.
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect prior migration database (pool)");
        seed_legacy_association_rows(&pool).await;
        pool.close().await;

        let mut conn = PgConnection::connect(&db_url)
            .await
            .expect("reconnect prior migration database");
        apply_migration_97(&mut conn).await;
        drop(conn);

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&db_url)
            .await
            .expect("connect migrated database");

        assert_provenance_schema(&pool).await;
        assert_legacy_rows_backfilled(&pool).await;

        pool.close().await;
    })
    .await;
}
