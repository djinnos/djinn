//! Live-Postgres migration coverage for the expand migration (125):
//! `repo_graph_generation_expand`.
//!
//! Covers:
//!   1. A seeded version-124 database upgraded by migration 125: deterministic
//!      backfill, one immutable generation per compatibility row, initialized
//!      clocks/current pointers, globally ordered `publish_seq`, preservation
//!      of the `(project_id, commit_sha)` conflict target and old readable
//!      columns.
//!   2. A fresh database through all migrations plus catalog assertions for
//!      all new keys, indexes, checks, and cascades.
//!   3. Artifact schema invariants: parent FKs/cascades, metadata count/byte
//!      constraints, distinct hash columns, 256 KiB accepted / 256 KiB+1
//!      rejected, and marked publication commit rejection for invalid
//!      manifests.
//!   4. Rollback-safe additive coverage: a forced failed expand transaction
//!      leaves pre-change rows and the actual old upsert / latest-row reader
//!      usable after rollback.

use std::path::{Path, PathBuf};

use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor, Row};

const EXPAND_VERSION: u64 = 125;

// ── connection / database helpers ────────────────────────────────────────────

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
        .map(|entry| {
            let path = entry.expect("migration entry").path();
            let version = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.split_once('_'))
                .and_then(|(prefix, _)| prefix.parse::<u64>().ok())
                .unwrap_or(0);
            (version, path)
        })
        .filter(|(_, path)| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
        .collect();
    entries.sort_by_key(|(version, _)| *version);
    entries
}

/// Apply migrations with version <= `max`.
async fn apply_through(conn: &mut PgConnection, max: u64) {
    for (version, path) in migration_entries(&migrations_dir()) {
        if version == 0 || version > max {
            continue;
        }
        let sql = std::fs::read_to_string(&path).expect("read migration");
        conn.execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("apply migration {}: {error}", path.display()));
    }
}

/// Read and return the SQL for a single migration version.
fn read_migration(version: u64) -> String {
    for (v, path) in migration_entries(&migrations_dir()) {
        if v == version {
            return std::fs::read_to_string(&path).expect("read migration");
        }
    }
    panic!("migration {version} not found");
}

struct TempDatabase {
    db_url: String,
    admin_url: String,
    db_name: String,
}

async fn create_temp_db(suffix: &str) -> TempDatabase {
    let base = base_database_url();
    let prefix = server_prefix(&base);
    let db_name = format!("djinn_expand_{suffix}_{}", uuid::Uuid::now_v7().simple());
    let admin_url = format!("{prefix}/postgres");
    let mut admin = PgConnection::connect(&admin_url)
        .await
        .expect("connect postgres admin database");
    admin
        .execute(format!(r#"CREATE DATABASE "{db_name}""#).as_str())
        .await
        .expect("create migration test database");
    drop(admin);
    TempDatabase {
        db_url: format!("{prefix}/{db_name}"),
        admin_url,
        db_name,
    }
}

async fn drop_temp_db(temp: &TempDatabase) {
    let mut admin = PgConnection::connect(&temp.admin_url)
        .await
        .expect("reconnect admin");
    let _ = admin
        .execute(
            format!(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = '{}' AND pid <> pg_backend_pid()",
                temp.db_name
            )
            .as_str(),
        )
        .await;
    admin
        .execute(format!(r#"DROP DATABASE IF EXISTS "{}""#, temp.db_name).as_str())
        .await
        .expect("drop migration test database");
}

// ── seeding helpers (version-124 shape) ──────────────────────────────────────

async fn seed_project(conn: &mut PgConnection, id: &str) {
    conn.execute(
        format!(
            "INSERT INTO projects(id, name, github_owner, github_repo) \
             VALUES ('{id}', '{id}', 'test-owner', '{id}')"
        )
        .as_str(),
    )
    .await
    .expect("seed project");
}

/// Insert a pre-expand `repo_graph_cache` row at the version-124 shape — no
/// `generation_id` column, textual `built_at`.
async fn seed_legacy_cache_row(
    conn: &mut PgConnection,
    project_id: &str,
    commit_sha: &str,
    built_at: &str,
    blob_hex: &str,
) {
    sqlx::query(&format!(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('{project_id}', '{commit_sha}', decode('{blob_hex}', 'hex'), '{built_at}')"
    ))
    .execute(&mut *conn)
    .await
    .expect("seed legacy repo_graph_cache row");
}

// ── Test 1: seeded version-124 upgrade ───────────────────────────────────────

#[tokio::test]
async fn seeded_v124_upgrade_deterministically_backfills_generations() {
    let temp = create_temp_db("seeded").await;
    let mut conn = PgConnection::connect(&temp.db_url)
        .await
        .expect("connect seeded db");

    // Migrate only through 124 — the expand migration has NOT run.
    apply_through(&mut conn, 124).await;

    // Seed two projects.
    seed_project(&mut conn, "proj-equal").await;
    seed_project(&mut conn, "proj-skew").await;

    // Project A — equal + empty textual built_at (ambiguous order).
    seed_legacy_cache_row(&mut conn, "proj-equal", "commit-a", "", "aa").await;
    seed_legacy_cache_row(&mut conn, "proj-equal", "commit-b", "", "bb").await;
    seed_legacy_cache_row(&mut conn, "proj-equal", "commit-c", "", "cc").await;

    // Project B — stale, equal, and future-skewed timestamps.
    seed_legacy_cache_row(
        &mut conn,
        "proj-skew",
        "old-1",
        "2020-01-01T00:00:00.000Z",
        "01",
    )
    .await;
    seed_legacy_cache_row(
        &mut conn,
        "proj-skew",
        "old-2",
        "2020-01-01T00:00:00.000Z",
        "02",
    )
    .await;
    seed_legacy_cache_row(
        &mut conn,
        "proj-skew",
        "future-1",
        "2099-12-31T23:59:59.999Z",
        "03",
    )
    .await;

    // Record pre-expand row counts.
    let pre_count_a: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_graph_cache WHERE project_id = 'proj-equal'")
            .fetch_one(&mut conn)
            .await
            .expect("count pre-expand proj-equal");
    assert_eq!(pre_count_a, 3);
    let pre_count_b: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_graph_cache WHERE project_id = 'proj-skew'")
            .fetch_one(&mut conn)
            .await
            .expect("count pre-expand proj-skew");
    assert_eq!(pre_count_b, 3);

    // Now apply the expand migration.
    let expand_sql = read_migration(EXPAND_VERSION);
    conn.execute(expand_sql.as_str())
        .await
        .expect("apply expand migration");

    // ── One immutable generation per compatibility row ────────────────────
    let gen_count_a: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_generation WHERE project_id = 'proj-equal'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count generations proj-equal");
    assert_eq!(gen_count_a, 3, "one generation per compatibility row");

    let gen_count_b: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_generation WHERE project_id = 'proj-skew'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count generations proj-skew");
    assert_eq!(gen_count_b, 3, "one generation per compatibility row");

    // ── Compatibility rows now carry generation_id ────────────────────────
    let cache_gen_null: i64 =
        sqlx::query_scalar("SELECT count(*) FROM repo_graph_cache WHERE generation_id IS NULL")
            .fetch_one(&mut conn)
            .await
            .expect("count null generation_id");
    assert_eq!(
        cache_gen_null, 0,
        "all compatibility rows have generation_id"
    );

    // ── generation_id is 1:1 with cache rows ──────────────────────────────
    let distinct_cache_gens: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT generation_id) FROM repo_graph_cache")
            .fetch_one(&mut conn)
            .await
            .expect("distinct cache generation ids");
    let total_cache: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_graph_cache")
        .fetch_one(&mut conn)
        .await
        .expect("total cache rows");
    assert_eq!(distinct_cache_gens, total_cache);

    // ── Deterministic unique normalized order per project ─────────────────
    // The backfill normalizes built_at to strictly increasing trigger-assigned
    // values; the ORDER BY ... built_at DESC LIMIT 1 reader now agrees with
    // repo_graph_current.
    for (project_id, expected_commits) in [
        ("proj-equal", &["commit-a", "commit-b", "commit-c"][..]),
        ("proj-skew", &["old-1", "old-2", "future-1"][..]),
    ] {
        // publish_seq is globally unique and strictly increasing per project.
        let seqs: Vec<i64> = sqlx::query_scalar(
            "SELECT publish_seq FROM repo_graph_generation \
             WHERE project_id = $1 ORDER BY publish_seq",
        )
        .bind(project_id)
        .fetch_all(&mut conn)
        .await
        .expect("fetch publish_seq");
        assert_eq!(seqs.len(), 3);
        for w in seqs.windows(2) {
            assert!(
                w[0] < w[1],
                "publish_seq strictly increasing for {project_id}"
            );
        }

        // Backfill's tie-breaker is part of the compatibility contract: empty
        // and equal legacy timestamps sort by commit_sha, while stale and
        // future rows retain lexical timestamp order. A nondeterministic
        // backfill would otherwise satisfy the uniqueness checks above.
        let commits: Vec<String> = sqlx::query_scalar(
            "SELECT commit_sha FROM repo_graph_generation \
             WHERE project_id = $1 ORDER BY publish_seq",
        )
        .bind(project_id)
        .fetch_all(&mut conn)
        .await
        .expect("fetch deterministic backfill commit order");
        assert_eq!(
            commits, expected_commits,
            "backfill commit order is deterministic for {project_id}"
        );

        // built_at is now unique and normalized (no empty / stale / skewed text).
        let built_ats: Vec<String> = sqlx::query_scalar(
            "SELECT built_at FROM repo_graph_generation \
             WHERE project_id = $1 ORDER BY publish_seq",
        )
        .bind(project_id)
        .fetch_all(&mut conn)
        .await
        .expect("fetch normalized built_at");
        let distinct: std::collections::HashSet<&str> =
            built_ats.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            distinct.len(),
            3,
            "normalized built_at are unique for {project_id}"
        );

        // ── Greatest-row current pointer ──────────────────────────────────
        let current_gen: Option<String> = sqlx::query_scalar(
            "SELECT generation_id::text FROM repo_graph_current WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_optional(&mut conn)
        .await
        .expect("fetch current pointer");
        assert!(
            current_gen.is_some(),
            "current pointer initialized for {project_id}"
        );

        // The latest reader agrees with the current pointer.
        let reader_gen: String = sqlx::query_scalar(
            "SELECT generation_id::text FROM repo_graph_cache \
             WHERE project_id = $1 ORDER BY built_at DESC LIMIT 1",
        )
        .bind(project_id)
        .fetch_one(&mut conn)
        .await
        .expect("fetch latest reader generation");
        assert_eq!(
            current_gen.unwrap(),
            reader_gen,
            "latest reader matches current for {project_id}"
        );
    }

    // ── Initialized publication clocks ────────────────────────────────────
    let clock_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_graph_publish_clock")
        .fetch_one(&mut conn)
        .await
        .expect("count clocks");
    assert_eq!(clock_count, 2, "one clock per project");

    // ── Preservation of old readable columns & conflict target ────────────
    let row = sqlx::query(
        "SELECT project_id, commit_sha, graph_blob FROM repo_graph_cache \
         WHERE project_id = 'proj-equal' AND commit_sha = 'commit-b'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("read old readable columns");
    let pid: String = row.get("project_id");
    let csha: String = row.get("commit_sha");
    let blob: Vec<u8> = row.get("graph_blob");
    assert_eq!(pid, "proj-equal");
    assert_eq!(csha, "commit-b");
    assert_eq!(blob, vec![0xbb]);

    // ── Globally ordered publish_seq ──────────────────────────────────────
    let all_seqs: Vec<i64> =
        sqlx::query_scalar("SELECT publish_seq FROM repo_graph_generation ORDER BY publish_seq")
            .fetch_all(&mut conn)
            .await
            .expect("fetch all publish_seq");
    assert_eq!(all_seqs.len(), 6);
    for w in all_seqs.windows(2) {
        assert!(w[0] < w[1], "publish_seq globally strictly increasing");
    }

    // ── Generation immutability ───────────────────────────────────────────
    let immutability = conn
        .execute(
            "UPDATE repo_graph_generation SET built_at = 'hacked' \
             WHERE project_id = 'proj-equal'",
        )
        .await;
    assert!(immutability.is_err(), "generations must be immutable");

    drop(conn);
    drop_temp_db(&temp).await;
}

// ── Test 2: fresh database + catalog assertions ──────────────────────────────

#[tokio::test]
async fn fresh_database_preserves_old_surface_and_has_all_new_objects() {
    let temp = create_temp_db("fresh").await;
    let mut conn = PgConnection::connect(&temp.db_url)
        .await
        .expect("connect fresh db");

    // Apply all migrations (including 125) on an empty database.
    apply_through(&mut conn, EXPAND_VERSION).await;

    // ── Old cache columns still exist ─────────────────────────────────────
    for col in [
        "project_id",
        "commit_sha",
        "graph_blob",
        "built_at",
        "generation_id",
    ] {
        let exists: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_name = 'repo_graph_cache' AND column_name = $1",
        )
        .bind(col)
        .fetch_one(&mut conn)
        .await
        .expect("inspect column");
        assert_eq!(exists, 1, "repo_graph_cache.{col} must exist");
    }

    // ── (project_id, commit_sha) conflict target (PK) preserved ───────────
    let pk: String = sqlx::query_scalar(
        "SELECT string_agg(kcu.column_name, ',' ORDER BY kcu.ordinal_position) \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON tc.constraint_name = kcu.constraint_name \
         WHERE tc.table_name = 'repo_graph_cache' AND tc.constraint_type = 'PRIMARY KEY'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("inspect cache PK");
    assert_eq!(
        pk, "project_id,commit_sha",
        "PK preserved as (project_id, commit_sha)"
    );

    // ── New tables exist ──────────────────────────────────────────────────
    for table in [
        "repo_graph_publish_clock",
        "repo_graph_generation",
        "repo_graph_current",
        "repo_graph_galaxy_artifact",
        "repo_graph_galaxy_chunk",
        "repo_graph_publish_lock_token",
    ] {
        let exists: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = $1",
        )
        .bind(table)
        .fetch_one(&mut conn)
        .await
        .expect("inspect table");
        assert_eq!(exists, 1, "table {table} must exist");
    }

    // ── New table keys and UNIQUE constraints ──────────────────────────────
    assert_pk(&mut conn, "repo_graph_publish_clock", "project_id").await;
    assert_pk(&mut conn, "repo_graph_generation", "generation_id").await;
    assert_pk(&mut conn, "repo_graph_current", "project_id").await;
    assert_pk(&mut conn, "repo_graph_galaxy_artifact", "artifact_id").await;
    assert_pk(
        &mut conn,
        "repo_graph_galaxy_chunk",
        "generation_id,artifact_id,chunk_index",
    )
    .await;
    assert_pk(
        &mut conn,
        "repo_graph_publish_lock_token",
        "project_id,transaction_id,backend_pid",
    )
    .await;
    assert_unique(
        &mut conn,
        "repo_graph_generation",
        "project_id,generation_id",
    )
    .await;
    assert_unique(&mut conn, "repo_graph_generation", "publish_seq").await;
    assert_unique(&mut conn, "repo_graph_cache", "generation_id").await;
    assert_unique(&mut conn, "repo_graph_galaxy_artifact", "generation_id").await;
    assert_unique(
        &mut conn,
        "repo_graph_galaxy_artifact",
        "generation_id,artifact_id",
    )
    .await;

    // ── publish_seq is GENERATED ALWAYS AS IDENTITY (globally unique) ─────
    let seq_is_identity: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.columns \
         WHERE table_name = 'repo_graph_generation' AND column_name = 'publish_seq' \
           AND is_identity = 'YES'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("inspect publish_seq identity");
    assert_eq!(seq_is_identity, 1, "publish_seq is an identity column");

    // ── Indexes ───────────────────────────────────────────────────────────
    for idx in [
        "repo_graph_generation_project_publish_seq",
        "repo_graph_generation_project_commit_publish_seq",
        "repo_graph_cache_project_built_at",
        "repo_graph_galaxy_chunk_artifact_order",
    ] {
        let exists: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pg_indexes WHERE indexname = $1")
                .bind(idx)
                .fetch_one(&mut conn)
                .await
                .expect("inspect index");
        assert_eq!(exists, 1, "index {idx} must exist");
    }

    // ── CHECK constraints on galaxy artifact ──────────────────────────────
    for check in [
        "repo_graph_galaxy_artifact_hashes_distinct",
        "repo_graph_galaxy_artifact_counts_nonnegative",
        "repo_graph_galaxy_artifact_chunk_hashes_array",
    ] {
        assert_check_exists(&mut conn, "repo_graph_galaxy_artifact", check).await;
    }

    // ── CHECK constraints on galaxy chunk ─────────────────────────────────
    for check in [
        "repo_graph_galaxy_chunk_index_nonnegative",
        "repo_graph_galaxy_chunk_size_nonnegative",
        "repo_graph_galaxy_chunk_size_matches_bytes",
        "repo_graph_galaxy_chunk_max_bytes",
    ] {
        assert_check_exists(&mut conn, "repo_graph_galaxy_chunk", check).await;
    }

    // ── FK cascades ───────────────────────────────────────────────────────
    assert_fk_cascade(&mut conn, "fk_repo_graph_publish_clock_project").await;
    assert_fk_cascade(&mut conn, "fk_repo_graph_generation_project").await;
    assert_fk_cascade(&mut conn, "fk_repo_graph_current_generation").await;
    assert_fk_cascade(&mut conn, "fk_repo_graph_galaxy_artifact_generation").await;
    assert_fk_cascade(&mut conn, "fk_repo_graph_galaxy_chunk_artifact").await;
    assert_fk_cascade(&mut conn, "fk_repo_graph_cache_generation").await;
    assert_fk_cascade(&mut conn, "fk_repo_graph_publish_lock_token_project").await;

    // ── On-delete cascade actually fires ──────────────────────────────────
    seed_project(&mut conn, "cascade-proj").await;
    // Legacy insert goes through the trigger path.
    conn.execute(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('cascade-proj', 'c1', decode('aa', 'hex'), '')",
    )
    .await
    .expect("seed cache row on fresh db");

    let gen_before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_generation WHERE project_id = 'cascade-proj'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count gens before delete");
    assert_eq!(gen_before, 1);

    // Deleting the project cascades to all child rows.
    conn.execute("DELETE FROM projects WHERE id = 'cascade-proj'")
        .await
        .expect("delete project");

    let cache_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_cache WHERE project_id = 'cascade-proj'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count cache after delete");
    assert_eq!(cache_after, 0, "cascade deleted cache rows");

    let gen_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_generation WHERE project_id = 'cascade-proj'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count gens after delete");
    assert_eq!(gen_after, 0, "cascade deleted generation rows");

    let current_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_current WHERE project_id = 'cascade-proj'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count current after delete");
    assert_eq!(current_after, 0, "cascade deleted current pointer");

    drop(conn);
    drop_temp_db(&temp).await;
}

async fn assert_pk(conn: &mut PgConnection, table: &str, columns: &str) {
    let pk: String = sqlx::query_scalar(
        "SELECT string_agg(kcu.column_name, ',' ORDER BY kcu.ordinal_position) \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON tc.constraint_name = kcu.constraint_name \
         WHERE tc.table_name = $1 AND tc.constraint_type = 'PRIMARY KEY'",
    )
    .bind(table)
    .fetch_one(conn)
    .await
    .expect("inspect PK");
    assert_eq!(pk, columns, "PK on {table} must be ({columns})");
}

async fn assert_unique(conn: &mut PgConnection, table: &str, columns: &str) {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
            SELECT tc.constraint_name,
                   string_agg(kcu.column_name, ',' ORDER BY kcu.ordinal_position) AS cols
            FROM information_schema.table_constraints tc
            JOIN information_schema.key_column_usage kcu
              ON tc.constraint_name = kcu.constraint_name
            WHERE tc.table_name = $1 AND tc.constraint_type = 'UNIQUE'
            GROUP BY tc.constraint_name
         ) s WHERE s.cols = $2",
    )
    .bind(table)
    .bind(columns)
    .fetch_one(conn)
    .await
    .expect("inspect UNIQUE");
    assert_eq!(count, 1, "UNIQUE({columns}) must exist on {table}");
}

async fn assert_check_exists(conn: &mut PgConnection, table: &str, constraint: &str) {
    let exists: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.table_constraints \
         WHERE table_name = $1 AND constraint_name = $2 AND constraint_type = 'CHECK'",
    )
    .bind(table)
    .bind(constraint)
    .fetch_one(conn)
    .await
    .expect("inspect CHECK");
    assert_eq!(exists, 1, "CHECK {constraint} must exist on {table}");
}

async fn assert_fk_cascade(conn: &mut PgConnection, constraint: &str) {
    let delete_rule: String = sqlx::query_scalar(
        "SELECT delete_rule FROM information_schema.referential_constraints \
         WHERE constraint_name = $1",
    )
    .bind(constraint)
    .fetch_one(conn)
    .await
    .unwrap_or_else(|_| panic!("FK {constraint} must exist"));
    assert_eq!(delete_rule, "CASCADE", "FK {constraint} must CASCADE");
}

// ── Test 3: artifact schema boundary invariants ──────────────────────────────

#[tokio::test]
async fn artifact_boundary_invariants_accept_and_reject_correctly() {
    let temp = create_temp_db("boundary").await;
    let mut conn = PgConnection::connect(&temp.db_url)
        .await
        .expect("connect boundary db");

    apply_through(&mut conn, EXPAND_VERSION).await;
    seed_project(&mut conn, "boundary-proj").await;

    // ── Establish a generation via a valid legacy publication ─────────────
    conn.execute(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('boundary-proj', 'b1', decode('aa', 'hex'), '')",
    )
    .await
    .expect("seed cache row");

    let generation_id: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_cache WHERE project_id = 'boundary-proj'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("fetch generation id");

    // ── 256 KiB chunk accepted ────────────────────────────────────────────
    conn.execute("BEGIN").await.expect("begin 256k");
    sqlx::query(
        "INSERT INTO repo_graph_galaxy_artifact \
         (generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
         VALUES ($1::uuid, 'content-256k', 'transport-256k', 1, 262144, '[\"sha-256k\"]'::jsonb)",
    )
    .bind(&generation_id)
    .execute(&mut conn)
    .await
    .expect("insert 256k artifact");
    sqlx::query(&format!(
        "INSERT INTO repo_graph_galaxy_chunk \
         (generation_id, artifact_id, chunk_index, byte_count, sha256, bytes) \
         SELECT $1::uuid, artifact_id, 0, 262144, 'sha-256k', decode('{}', 'hex') \
         FROM repo_graph_galaxy_artifact WHERE generation_id = $1::uuid",
        "00".repeat(262144)
    ))
    .bind(&generation_id)
    .execute(&mut conn)
    .await
    .expect("insert 256k chunk");
    conn.execute("COMMIT").await.expect("commit 256k");

    // ── 256 KiB + 1 rejected by CHECK constraint ──────────────────────────
    let artifact_2 = uuid::Uuid::now_v7();
    // The artifact itself can be inserted (byte_count is just a number), but
    // the chunk insert will fail the max_bytes CHECK.
    let _ = conn
        .execute(format!(
            "INSERT INTO repo_graph_galaxy_artifact \
             (artifact_id, generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
             VALUES ('{artifact_2}', '{generation_id}'::uuid, 'content-plus', 'transport-plus', 1, 262145, '[\"sha-plus\"]'::jsonb)"
        ).as_str())
        .await;
    let chunk_err = conn
        .execute(format!(
            "INSERT INTO repo_graph_galaxy_chunk \
             (generation_id, artifact_id, chunk_index, byte_count, sha256, bytes) \
             VALUES ('{generation_id}'::uuid, '{artifact_2}', 0, 262145, 'sha-plus', decode('{}', 'hex'))",
            "00".repeat(262145)
        ).as_str())
        .await
        .expect_err("256 KiB + 1 chunk must be rejected");
    let msg = chunk_err.to_string();
    assert!(
        msg.contains("262144") || msg.contains("chunk_max_bytes"),
        "oversize chunk error must mention size limit: {msg}"
    );

    // Clean up the failed artifact if it was inserted.
    let _ = conn
        .execute(
            format!("DELETE FROM repo_graph_galaxy_artifact WHERE artifact_id = '{artifact_2}'")
                .as_str(),
        )
        .await;

    // ── Distinct hash columns: identical values rejected ──────────────────
    let same_hash_err = conn
        .execute(format!(
            "INSERT INTO repo_graph_galaxy_artifact \
             (generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
             VALUES ('{generation_id}'::uuid, 'same', 'same', 0, 0, '[]'::jsonb)"
        ).as_str())
        .await
        .expect_err("identical graph/transport hashes must be rejected");
    assert!(
        same_hash_err.to_string().contains("distinct"),
        "identical hash error: {same_hash_err}"
    );

    // ── Nonnegative count/byte constraints ────────────────────────────────
    let neg_count_err = conn
        .execute(format!(
            "INSERT INTO repo_graph_galaxy_artifact \
             (generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
             VALUES ('{generation_id}'::uuid, 'c-neg', 't-neg', -1, 0, '[]'::jsonb)"
        ).as_str())
        .await
        .expect_err("negative chunk_count must be rejected");
    assert!(
        neg_count_err.to_string().contains("nonnegative")
            || neg_count_err.to_string().contains("chunk_hashes_array"),
        "negative count must be rejected by a CHECK constraint: {neg_count_err}"
    );

    // Use a separate generation so its otherwise-valid artifact identity cannot
    // mask a missing byte_count check with the one-artifact-per-generation key.
    conn.execute(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('boundary-proj', 'negative-byte-count', decode('ab', 'hex'), '')",
    )
    .await
    .expect("seed cache row for negative byte_count test");
    let negative_byte_generation_id: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_cache \
         WHERE project_id = 'boundary-proj' AND commit_sha = 'negative-byte-count'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("fetch generation id for negative byte_count test");
    let neg_byte_err = conn
        .execute(
            format!(
                "INSERT INTO repo_graph_galaxy_artifact \
                 (generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
                 VALUES ('{negative_byte_generation_id}'::uuid, 'c-neg-byte', 't-neg-byte', 0, -1, '[]'::jsonb)"
            )
            .as_str(),
        )
        .await
        .expect_err("negative artifact byte_count must be rejected");
    assert!(
        neg_byte_err.to_string().contains("nonnegative"),
        "negative byte_count must be rejected by the nonnegative CHECK: {neg_byte_err}"
    );

    // ── chunk_hashes must be an array whose length equals chunk_count ──────
    let bad_array_err = conn
        .execute(format!(
            "INSERT INTO repo_graph_galaxy_artifact \
             (generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
             VALUES ('{generation_id}'::uuid, 'c-arr', 't-arr', 2, 0, '[\"a\"]'::jsonb)"
        ).as_str())
        .await
        .expect_err("chunk_hashes length mismatch must be rejected");
    let msg = bad_array_err.to_string();
    assert!(
        msg.contains("chunk_hashes_array") || msg.contains("array_length"),
        "bad chunk_hashes array: {msg}"
    );

    // ── Missing parent: artifact references nonexistent generation ────────
    let nonexistent_gen = uuid::Uuid::now_v7();
    let missing_parent_err = conn
        .execute(format!(
            "INSERT INTO repo_graph_galaxy_artifact \
             (generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
             VALUES ('{nonexistent_gen}', 'c-miss', 't-miss', 0, 0, '[]'::jsonb)"
        ).as_str())
        .await
        .expect_err("missing parent generation must be rejected");
    assert!(
        missing_parent_err.to_string().contains("foreign key")
            || missing_parent_err.to_string().contains("violates"),
        "missing parent error: {missing_parent_err}"
    );

    // ── byte_count = octet_length(bytes) enforced ─────────────────────────
    // Need a fresh generation since each generation allows only one artifact.
    conn.execute(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('boundary-proj', 'b2', decode('bb', 'hex'), '')",
    )
    .await
    .expect("seed second cache row for byte mismatch test");
    let generation_id_2: String =
        sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_cache WHERE project_id = 'boundary-proj' AND commit_sha = 'b2'")
            .fetch_one(&mut conn)
            .await
            .expect("fetch second generation id");

    let artifact_3 = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO repo_graph_galaxy_artifact \
         (artifact_id, generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
         VALUES ($1::uuid, $2::uuid, 'content-mismatch', 'transport-mismatch', 1, 1, '[\"sha-mm\"]'::jsonb)",
    )
    .bind(artifact_3.to_string())
    .bind(&generation_id_2)
    .execute(&mut conn)
    .await
    .expect("insert artifact for byte mismatch test");

    let byte_mismatch_err = conn
        .execute(format!(
            "INSERT INTO repo_graph_galaxy_chunk \
             (generation_id, artifact_id, chunk_index, byte_count, sha256, bytes) \
             VALUES ('{generation_id_2}'::uuid, '{artifact_3}', 0, 99, 'sha-mm', decode('ff', 'hex'))"
        ).as_str())
        .await
        .expect_err("byte_count != octet_length(bytes) must be rejected");
    assert!(
        byte_mismatch_err.to_string().contains("matches_bytes"),
        "byte mismatch error: {byte_mismatch_err}"
    );
    let _ = conn
        .execute(
            format!("DELETE FROM repo_graph_galaxy_artifact WHERE artifact_id = '{artifact_3}'")
                .as_str(),
        )
        .await;

    drop(conn);
    drop_temp_db(&temp).await;
}

// ── Test 4: marked publication commit rejection ──────────────────────────────

#[tokio::test]
async fn marked_publication_rejects_invalid_manifests_without_partial_state() {
    let temp = create_temp_db("marked").await;
    let mut conn = PgConnection::connect(&temp.db_url)
        .await
        .expect("connect marked db");

    apply_through(&mut conn, EXPAND_VERSION).await;
    seed_project(&mut conn, "marked-proj").await;

    // A successful compatibility publication gives failed marked publications
    // observable clock and current-pointer state that they must not change.
    conn.execute(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('marked-proj', 'baseline', decode('00', 'hex'), '')",
    )
    .await
    .expect("publish baseline compatibility row");

    async fn publication_state(
        conn: &mut PgConnection,
    ) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
        let cache = sqlx::query_scalar(
            "SELECT commit_sha || ':' || generation_id::text || ':' || built_at \
             FROM repo_graph_cache WHERE project_id = 'marked-proj' \
             ORDER BY commit_sha",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("snapshot compatibility rows");
        let generations = sqlx::query_scalar(
            "SELECT commit_sha || ':' || generation_id::text || ':' || publish_seq::text \
             FROM repo_graph_generation WHERE project_id = 'marked-proj' \
             ORDER BY publish_seq",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("snapshot immutable generations");
        let clocks = sqlx::query_scalar(
            "SELECT project_id || ':' || last_built_at::text \
             FROM repo_graph_publish_clock WHERE project_id = 'marked-proj'",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("snapshot publication clock");
        let current = sqlx::query_scalar(
            "SELECT project_id || ':' || generation_id::text \
             FROM repo_graph_current WHERE project_id = 'marked-proj'",
        )
        .fetch_all(&mut *conn)
        .await
        .expect("snapshot current pointer");
        (cache, generations, clocks, current)
    }

    let baseline_state = publication_state(&mut conn).await;

    // Helper: attempt a marked publication that should fail at COMMIT.
    // Returns true if COMMIT failed.
    async fn attempt_bad_marked(
        conn: &mut PgConnection,
        graph_blob_hex: &str,
        chunk_count: i32,
        byte_count: i64,
        chunk_hashes_json: &str,
        chunks: &[(i32, &str, &str)], // (chunk_index, sha256, blob_hex)
    ) -> bool {
        let generation = uuid::Uuid::now_v7();
        conn.execute("BEGIN").await.expect("begin marked");
        sqlx::query("SELECT repo_graph_reserve_generation('marked-proj', $1::text::uuid)")
            .bind(generation.to_string())
            .execute(&mut *conn)
            .await
            .expect("reserve generation");
        sqlx::query(
            "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at, generation_id) \
             VALUES ('marked-proj', $1, decode($2, 'hex'), '', $3::text::uuid)",
        )
        .bind(generation.to_string())
        .bind(graph_blob_hex)
        .bind(generation.to_string())
        .execute(&mut *conn)
        .await
        .expect("insert marked compatibility row");

        sqlx::query(
            "INSERT INTO repo_graph_galaxy_artifact \
             (generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
             VALUES ($1::uuid, 'gc', 'ts', $2, $3, $4::jsonb)",
        )
        .bind(generation.to_string())
        .bind(chunk_count)
        .bind(byte_count)
        .bind(chunk_hashes_json)
        .execute(&mut *conn)
        .await
        .expect("insert artifact for deferred validation");

        let artifact_id: String =
            sqlx::query_scalar("SELECT artifact_id::text FROM repo_graph_galaxy_artifact WHERE generation_id = $1::uuid")
                .bind(generation.to_string())
                .fetch_one(&mut *conn)
                .await
                .expect("fetch artifact id");

        for &(idx, sha, blob) in chunks {
            sqlx::query(
                "INSERT INTO repo_graph_galaxy_chunk \
                 (generation_id, artifact_id, chunk_index, byte_count, sha256, bytes) \
                 VALUES ($1::uuid, $2::uuid, $3, $4, $5, decode($6, 'hex'))",
            )
            .bind(generation.to_string())
            .bind(&artifact_id)
            .bind(idx)
            .bind(blob.len() as i32 / 2) // hex pairs -> bytes
            .bind(sha)
            .bind(blob)
            .execute(&mut *conn)
            .await
            .expect("insert chunk");
        }

        let commit_result = conn.execute("COMMIT").await;
        if commit_result.is_err() {
            let _ = conn.execute("ROLLBACK").await;
            true
        } else {
            false
        }
    }

    // ── Missing chunks: chunk_count=2 but only 1 chunk present ────────────
    let failed = attempt_bad_marked(
        &mut conn,
        "aa",
        2,
        2,
        r#"["sha-0", "sha-1"]"#,
        &[(0, "sha-0", "aa")],
    )
    .await;
    assert!(failed, "missing chunk must fail marked publication");
    assert_eq!(
        publication_state(&mut conn).await,
        baseline_state,
        "missing chunks leave compatibility, generation, clock, and current unchanged"
    );

    // ── Noncontiguous chunks: index 0 and 2 (skipping 1) ──────────────────
    let failed = attempt_bad_marked(
        &mut conn,
        "bb",
        2,
        2,
        r#"["sha-0", "sha-1"]"#,
        &[(0, "sha-0", "aa"), (2, "sha-1", "bb")],
    )
    .await;
    assert!(failed, "noncontiguous chunks must fail marked publication");
    assert_eq!(
        publication_state(&mut conn).await,
        baseline_state,
        "noncontiguous chunks leave compatibility, generation, clock, and current unchanged"
    );

    // ── Mismatched per-chunk hash ──────────────────────────────────────────
    let failed = attempt_bad_marked(
        &mut conn,
        "cc",
        1,
        1,
        r#"["manifest-hash"]"#,
        &[(0, "wrong-hash", "cc")],
    )
    .await;
    assert!(failed, "mismatched chunk hash must fail marked publication");
    assert_eq!(
        publication_state(&mut conn).await,
        baseline_state,
        "mismatched chunk hash leaves compatibility, generation, clock, and current unchanged"
    );

    // ── Mismatched aggregate byte count ────────────────────────────────────
    let failed = attempt_bad_marked(
        &mut conn,
        "dd",
        1,
        99,
        r#"["sha-0"]"#,
        &[(0, "sha-0", "dd")],
    )
    .await;
    assert!(
        failed,
        "mismatched aggregate byte count must fail marked publication"
    );
    assert_eq!(
        publication_state(&mut conn).await,
        baseline_state,
        "mismatched aggregate bytes leave compatibility, generation, clock, and current unchanged"
    );

    // ── Failed publication leaves no partial state ────────────────────────
    let artifact_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_graph_galaxy_artifact")
        .fetch_one(&mut conn)
        .await
        .expect("count artifacts");
    assert_eq!(artifact_count, 0, "no artifacts from failed publications");

    let chunk_count: i64 = sqlx::query_scalar("SELECT count(*) FROM repo_graph_galaxy_chunk")
        .fetch_one(&mut conn)
        .await
        .expect("count chunks");
    assert_eq!(chunk_count, 0, "no chunks from failed publications");

    // No generation should have artifact_required = true (no marked publication succeeded).
    let marked_gens: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_generation WHERE artifact_required = true",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count marked gens");
    assert_eq!(marked_gens, 0, "no successful marked publications");

    // Only the successful baseline cache row remains; every failed marked
    // transaction rolled back its compatibility publication entirely.
    let cache_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_cache WHERE project_id = 'marked-proj'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count cache rows");
    assert_eq!(
        cache_count, 1,
        "only baseline cache row remains after rollback"
    );

    drop(conn);
    drop_temp_db(&temp).await;
}

// ── Test 5: rollback-safe additive coverage ──────────────────────────────────

#[tokio::test]
async fn forced_failed_expand_leaves_pre_change_rows_and_old_reader_usable() {
    let temp = create_temp_db("rollback").await;
    let mut conn = PgConnection::connect(&temp.db_url)
        .await
        .expect("connect rollback db");

    // Migrate only through 124.
    apply_through(&mut conn, 124).await;

    // Seed a project and a legacy cache row with the ACTUAL old upsert.
    seed_project(&mut conn, "rollback-proj").await;
    conn.execute(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('rollback-proj', 'rc1', decode('aa', 'hex'), '2024-01-01T00:00:00.000Z')",
    )
    .await
    .expect("seed legacy cache row");

    // Verify the actual old latest-row reader works before expand.
    let row = sqlx::query(
        "SELECT commit_sha, graph_blob FROM repo_graph_cache \
         WHERE project_id = 'rollback-proj' \
         ORDER BY built_at DESC LIMIT 1",
    )
    .fetch_one(&mut conn)
    .await
    .expect("old reader before expand");
    let old_commit: String = row.get("commit_sha");
    let old_blob: Vec<u8> = row.get("graph_blob");
    assert_eq!(old_commit, "rc1");
    assert_eq!(old_blob, vec![0xaa]);

    // Read the expand migration SQL.
    let expand_sql = read_migration(EXPAND_VERSION);

    // ── Attempt the expand migration within a transaction and force failure ─
    conn.execute("BEGIN").await.expect("begin expand txn");

    // Apply the migration inside the transaction.
    let apply_result = conn.execute(expand_sql.as_str()).await;

    if apply_result.is_ok() {
        // The migration succeeded inside the txn. Now force a failure by
        // raising an error, which will abort the transaction.
        let force_fail = conn.execute("SELECT 1 / 0").await;
        assert!(force_fail.is_err(), "forced failure must abort transaction");
    }

    // The transaction is now aborted; ROLLBACK to undo.
    let rollback_result = conn.execute("ROLLBACK").await;
    // If ROLLBACK also fails (because the connection is in a bad state), the
    // transaction was already implicitly rolled back.
    if rollback_result.is_err() {
        // Reconnect to get a clean connection.
        drop(conn);
        conn = PgConnection::connect(&temp.db_url)
            .await
            .expect("reconnect after rollback");
    }

    // ── Pre-expand data is still intact ──────────────────────────────────
    let cache_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_cache WHERE project_id = 'rollback-proj'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count cache after rollback");
    assert_eq!(
        cache_count, 1,
        "pre-expand cache row preserved after rollback"
    );

    let row = sqlx::query(
        "SELECT commit_sha, graph_blob, built_at FROM repo_graph_cache \
         WHERE project_id = 'rollback-proj'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("read cache row after rollback");
    let commit_sha: String = row.get("commit_sha");
    let blob: Vec<u8> = row.get("graph_blob");
    let built_at: String = row.get("built_at");
    assert_eq!(commit_sha, "rc1");
    assert_eq!(blob, vec![0xaa]);
    assert_eq!(built_at, "2024-01-01T00:00:00.000Z");

    // ── The actual old latest-row reader still works ──────────────────────
    let row = sqlx::query(
        "SELECT commit_sha, graph_blob FROM repo_graph_cache \
         WHERE project_id = 'rollback-proj' \
         ORDER BY built_at DESC LIMIT 1",
    )
    .fetch_one(&mut conn)
    .await
    .expect("old reader after rollback");
    let reader_commit: String = row.get("commit_sha");
    let reader_blob: Vec<u8> = row.get("graph_blob");
    assert_eq!(reader_commit, "rc1");
    assert_eq!(reader_blob, vec![0xaa]);

    // ── The actual old upsert still works (insert a new commit) ───────────
    conn.execute(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('rollback-proj', 'rc2', decode('bb', 'hex'), '2024-06-01T00:00:00.000Z')",
    )
    .await
    .expect("old upsert after rollback");

    // The old conflict-update path works too (ON CONFLICT update).
    conn.execute(
        "INSERT INTO repo_graph_cache(project_id, commit_sha, graph_blob, built_at) \
         VALUES ('rollback-proj', 'rc1', decode('cc', 'hex'), '2024-02-01T00:00:00.000Z') \
         ON CONFLICT (project_id, commit_sha) DO UPDATE \
         SET graph_blob = EXCLUDED.graph_blob, built_at = EXCLUDED.built_at",
    )
    .await
    .expect("old conflict-update after rollback");

    // Verify the conflict-update took effect.
    let row = sqlx::query(
        "SELECT graph_blob, built_at FROM repo_graph_cache \
         WHERE project_id = 'rollback-proj' AND commit_sha = 'rc1'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("read updated row");
    let updated_blob: Vec<u8> = row.get("graph_blob");
    let updated_built_at: String = row.get("built_at");
    assert_eq!(updated_blob, vec![0xcc]);
    assert_eq!(updated_built_at, "2024-02-01T00:00:00.000Z");

    // ── No expand artifacts exist ─────────────────────────────────────────
    let gen_table_exists: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_name = 'repo_graph_generation'",
    )
    .fetch_one(&mut conn)
    .await
    .expect("check generation table");
    assert_eq!(
        gen_table_exists, 0,
        "expand tables must not exist after rollback"
    );

    drop(conn);
    drop_temp_db(&temp).await;
}
