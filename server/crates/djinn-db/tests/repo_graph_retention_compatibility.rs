//! Live-Postgres regression matrix for bounded graph-generation retention.
//!
//! This deliberately executes the byte-for-byte legacy cache upsert and latest
//! reader while using `RepoGraphRetentionRepository` for every sweep.  It is a
//! compatibility test, not a replacement implementation of the old SQL.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use djinn_db::{
    Database, DatabaseConnectConfig, PostgresDatabaseConfig, RepoGraphGenerationRepository,
    RepoGraphRetentionRepository, RetentionMode, RetentionSweepRequest,
    acquire_generation_stream_pin_shared, generation_stream_pin_key,
    release_generation_stream_pin_shared,
};
use sqlx::postgres::PgConnection;
use sqlx::{Acquire, Connection, Executor, Row};
use tokio::sync::Barrier;

const PROJECT: &str = "retention-compat-project";

// Kept semantically identical to RepoGraphCacheRepository::upsert before the
// generation expand work. Do not replace this with a repository call.
const LEGACY_UPSERT: &str = r#"INSERT INTO repo_graph_cache
             (project_id, commit_sha, graph_blob, built_at)
             VALUES ($1, $2, $3, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (project_id, commit_sha) DO UPDATE SET
                graph_blob = EXCLUDED.graph_blob,
                built_at = EXCLUDED.built_at"#;

// The pre-change reader must remain an actual old `built_at` reader.
const OLD_LATEST: &str = "SELECT project_id, commit_sha, graph_blob, built_at, generation_id::text AS generation_id \
                          FROM repo_graph_cache WHERE project_id = $1 ORDER BY built_at DESC LIMIT 1";

async fn fresh() -> (Database, RepoGraphRetentionRepository) {
    let base = std::env::var("TEST_POSTGRES_URL").expect("live PostgreSQL URL");
    let prefix = base.rsplit_once('/').expect("database URL").0;
    let name = format!("djinn_retention_{}", uuid::Uuid::now_v7().simple());
    let mut admin = PgConnection::connect(&format!("{prefix}/postgres"))
        .await
        .expect("admin connection");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create isolated database");
    drop(admin);
    let url = format!("{prefix}/{name}");
    let mut migration = PgConnection::connect(&url)
        .await
        .expect("migration connection");
    let migration_files = migrations();
    for (_, path) in &migration_files {
        let sql = std::fs::read_to_string(&path).expect("read migration");
        migration
            .execute(sql.as_str())
            .await
            .unwrap_or_else(|error| panic!("apply {}: {error}", path.display()));
    }
    migration
        .execute(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, success BOOLEAN NOT NULL)",
        )
        .await
        .expect("migration ledger");
    for (version, _) in migration_files {
        sqlx::query("INSERT INTO _sqlx_migrations(version, success) VALUES ($1, TRUE)")
            .bind(version as i64)
            .execute(&mut migration)
            .await
            .expect("record applied migration");
    }
    drop(migration);
    let db = Database::open_with_config(DatabaseConnectConfig::Postgres(PostgresDatabaseConfig {
        url,
    }))
    .expect("production database handle");
    db.verify_and_mark_initialized()
        .await
        .expect("mark migrated database initialized");
    sqlx::query(
        "INSERT INTO projects(id, name, github_owner, github_repo) \
         VALUES ($1, 'retention compatibility', 'compat-owner', 'compat-repo')",
    )
    .bind(PROJECT)
    .execute(db.pool())
    .await
    .expect("insert project");
    let retention = RepoGraphRetentionRepository::new(db.clone());
    (db, retention)
}

fn migrations() -> Vec<(u64, PathBuf)> {
    let mut files: Vec<_> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres"))
            .expect("read migrations")
            .map(|entry| {
                let path = entry.expect("migration entry").path();
                let version = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.split_once('_'))
                    .and_then(|(version, _)| version.parse().ok())
                    .unwrap_or_default();
                (version, path)
            })
            .filter(|(_, path)| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
            .collect();
    files.sort_by_key(|(version, _)| *version);
    files
}

async fn legacy_publish(db: &Database, commit: &str, blob: &[u8]) {
    sqlx::query(LEGACY_UPSERT)
        .bind(PROJECT)
        .bind(commit)
        .bind(blob)
        .execute(db.pool())
        .await
        .expect("actual legacy upsert");
}

async fn count(db: &Database, table: &str) -> i64 {
    let sql = match table {
        "repo_graph_cache" => "SELECT count(*) FROM repo_graph_cache WHERE project_id = $1",
        "repo_graph_generation" => {
            "SELECT count(*) FROM repo_graph_generation WHERE project_id = $1"
        }
        "artifacts" => {
            "SELECT count(*) FROM repo_graph_galaxy_artifact a \
                        JOIN repo_graph_generation g ON g.generation_id = a.generation_id \
                        WHERE g.project_id = $1"
        }
        "chunks" => {
            "SELECT count(*) FROM repo_graph_galaxy_chunk c \
                     JOIN repo_graph_generation g ON g.generation_id = c.generation_id \
                     WHERE g.project_id = $1"
        }
        _ => panic!("unknown count table"),
    };
    sqlx::query_scalar(sql)
        .bind(PROJECT)
        .fetch_one(db.pool())
        .await
        .expect("count rows")
}

async fn assert_old_reader_agrees_with_current(db: &Database) {
    let old = sqlx::query(OLD_LATEST)
        .bind(PROJECT)
        .fetch_one(db.pool())
        .await
        .expect("old latest reader row");
    let current = sqlx::query(
        "SELECT g.project_id, g.commit_sha, g.graph_blob, g.built_at, \
                g.generation_id::text AS generation_id \
         FROM repo_graph_current c JOIN repo_graph_generation g \
           ON (g.project_id, g.generation_id) = (c.project_id, c.generation_id) \
         WHERE c.project_id = $1",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .expect("current generation");
    for column in ["project_id", "commit_sha", "built_at", "generation_id"] {
        assert_eq!(
            old.get::<String, _>(column),
            current.get::<String, _>(column),
            "old reader {column}"
        );
    }
    assert_eq!(
        old.get::<Vec<u8>, _>("graph_blob"),
        current.get::<Vec<u8>, _>("graph_blob"),
        "old reader blob"
    );
}

async fn sweep(retention: &RepoGraphRetentionRepository, mode: RetentionMode, n: usize) {
    retention
        .sweep(RetentionSweepRequest {
            project_id: PROJECT,
            mode,
            history_n: n,
        })
        .await
        .expect("production retention sweep");
}

async fn assert_bounded_full_blobs(db: &Database, n: i64) {
    let cache = count(db, "repo_graph_cache").await;
    let generations = count(db, "repo_graph_generation").await;
    assert!(cache <= n + 1, "cache full blobs {cache} exceed N+1");
    assert!(
        generations <= n + 1,
        "generation full blobs {generations} exceed N+1"
    );
    assert!(
        cache + generations <= 2 * (n + 1),
        "combined full blobs {} exceed 2(N+1)",
        cache + generations
    );
}

#[tokio::test]
async fn retention_matrix_uses_production_api_and_actual_legacy_sql() {
    let (db, retention) = fresh().await;

    // Equal, stale, and future source timestamps are intentionally ignored by
    // the trigger clock. The old reader must still agree after each publication.
    for (commit, source) in [
        ("equal", ""),
        ("stale", ""),
        ("future", "9999-12-31T00:00:00Z"),
    ] {
        sqlx::query(
            "INSERT INTO repo_graph_cache (project_id, commit_sha, graph_blob, built_at) \
                     VALUES ($1, $2, decode('01', 'hex'), $3)",
        )
        .bind(PROJECT)
        .bind(commit)
        .bind(source)
        .execute(db.pool())
        .await
        .expect("direct legacy compatibility row");
        assert_old_reader_agrees_with_current(&db).await;
    }

    // Same-key rotations and more than one 25-row delete batch.
    for i in 0..31 {
        legacy_publish(&db, &format!("commit-{i}"), format!("blob-{i}").as_bytes()).await;
    }
    for i in 0..4 {
        legacy_publish(&db, "rotated", format!("rotation-{i}").as_bytes()).await;
    }
    let rotated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_generation WHERE project_id=$1 AND commit_sha='rotated'",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .expect("count rotations");
    assert_eq!(rotated, 4, "same commit must keep immutable rotations");

    // Dry-run selects but cannot remove rows.
    let before_dry = count(&db, "repo_graph_generation").await;
    sweep(&retention, RetentionMode::DryRun, 2).await;
    assert_eq!(count(&db, "repo_graph_generation").await, before_dry);

    // A rollback of the actual old statement leaves no generation or cache row.
    let mut rollback = db.pool().acquire().await.expect("rollback connection");
    let mut tx = rollback.begin().await.expect("begin rollback publication");
    sqlx::query(LEGACY_UPSERT)
        .bind(PROJECT)
        .bind("rolled-back")
        .bind(b"nope".as_slice())
        .execute(&mut *tx)
        .await
        .expect("legacy upsert inside rollback");
    tx.rollback().await.expect("rollback publication");
    let rolled_back: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM repo_graph_generation WHERE project_id=$1 AND commit_sha='rolled-back'",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .expect("rolled back generation absent");
    assert_eq!(rolled_back, 0);

    // Each call is bounded to 25; repeat until the independently bounded cache
    // and generation stores have converged.
    loop {
        let outcome = retention
            .sweep(RetentionSweepRequest {
                project_id: PROJECT,
                mode: RetentionMode::Delete,
                history_n: 2,
            })
            .await
            .expect("bounded delete sweep");
        assert!(outcome.deleted <= 25);
        if outcome.deleted == 0 {
            break;
        }
    }
    assert_bounded_full_blobs(&db, 2).await;
    assert_old_reader_agrees_with_current(&db).await;

    // Recreate a commit after its prior generation was pruned; its fresh
    // generation becomes current and must survive the following sweep.
    legacy_publish(&db, "commit-0", b"recreated").await;
    let recreated: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_current WHERE project_id=$1",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .expect("recreated current");
    sweep(&retention, RetentionMode::Delete, 2).await;
    let still_current: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_current WHERE project_id=$1",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .expect("current after recreation sweep");
    assert_eq!(still_current, recreated);
    assert_old_reader_agrees_with_current(&db).await;
}

#[tokio::test]
async fn active_pin_refills_and_pruned_artifacts_cascade() {
    let (db, retention) = fresh().await;
    let generations = RepoGraphGenerationRepository::new(db.clone());

    // Publish an artifact-bearing generation through the production publisher,
    // then make it old with actual legacy rows. Existing repository coverage
    // validates the manifest; this test validates retention's cascade path.
    let artifact_generation = uuid::Uuid::now_v7().to_string();
    let artifact = uuid::Uuid::now_v7().to_string();
    let bytes = b"artifact".to_vec();
    let digest = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(&bytes))
    };
    generations
        .publish_reserved_generation(djinn_db::ReservedGraphPublication {
            project_id: PROJECT.to_owned(),
            commit_sha: "artifact-old".to_owned(),
            generation_id: artifact_generation.clone(),
            graph_blob: bytes.clone(),
            artifact: djinn_db::ReservedGalaxyArtifactManifest {
                artifact_id: artifact.clone(),
                generation_id: artifact_generation.clone(),
                graph_content_hash: "semantic-domain-hash".to_owned(),
                transport_sha256: digest.clone(),
                chunk_count: 1,
                byte_count: bytes.len() as i64,
                chunk_hashes: vec![digest.clone()],
            },
            chunks: vec![djinn_db::ReservedGalaxyArtifactChunk {
                generation_id: artifact_generation.clone(),
                artifact_id: artifact,
                chunk_index: 0,
                sha256: digest,
                bytes,
            }],
        })
        .await
        .expect("publish artifact generation");
    for i in 0..5 {
        legacy_publish(&db, &format!("later-{i}"), b"later").await;
    }
    assert_eq!(count(&db, "artifacts").await, 1);
    assert_eq!(count(&db, "chunks").await, 1);

    // Pin the artifact generation. Retention must skip it without waiting and
    // refill from later candidates rather than stopping at the active stream.
    let key = generation_stream_pin_key(&artifact_generation).expect("stream key");
    let mut holder = db.pool().acquire().await.expect("pin holder");
    acquire_generation_stream_pin_shared(&mut holder, key)
        .await
        .expect("shared pin");
    let outcome = retention
        .sweep(RetentionSweepRequest {
            project_id: PROJECT,
            mode: RetentionMode::Delete,
            history_n: 2,
        })
        .await
        .expect("pinned sweep");
    assert_eq!(outcome.skipped_active_pin, 1);
    assert!(outcome.deleted >= 3, "unpinned candidates refill the batch");
    assert_eq!(count(&db, "artifacts").await, 1, "pinned artifact survives");
    release_generation_stream_pin_shared(&mut holder, key)
        .await
        .expect("release shared pin");
    drop(holder);

    sweep(&retention, RetentionMode::Delete, 2).await;
    assert_eq!(count(&db, "artifacts").await, 0, "artifact cascaded");
    assert_eq!(count(&db, "chunks").await, 0, "chunks cascaded");
    assert_bounded_full_blobs(&db, 2).await;
    assert_old_reader_agrees_with_current(&db).await;
}

#[tokio::test]
async fn paused_actual_same_key_legacy_upsert_blocks_retention_then_recomputes() {
    let (db, _retention) = fresh().await;
    for i in 0..4 {
        legacy_publish(&db, &format!("old-{i}"), b"old").await;
    }
    legacy_publish(&db, "same-key", b"before").await;
    let before: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_current WHERE project_id=$1",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .expect("baseline current");

    let statement_owned = Arc::new(Barrier::new(2));
    let allow_commit = Arc::new(Barrier::new(2));
    let publisher_db = db.clone();
    let publisher_owned = statement_owned.clone();
    let publisher_commit = allow_commit.clone();
    let publisher = tokio::spawn(async move {
        let mut conn = publisher_db
            .pool()
            .acquire()
            .await
            .expect("publisher connection");
        let mut tx = conn.begin().await.expect("begin old publisher");
        sqlx::query(LEGACY_UPSERT)
            .bind(PROJECT)
            .bind("same-key")
            .bind(b"republished".as_slice())
            .execute(&mut *tx)
            .await
            .expect("actual same-key legacy upsert");
        // The old statement now owns both its project advisory and conflict-row
        // locks. Do not commit until retention has demonstrably waited.
        publisher_owned.wait().await;
        publisher_commit.wait().await;
        tx.commit().await.expect("commit old publisher");
    });
    statement_owned.wait().await;

    let retention_db = db.clone();
    let mut retention = tokio::spawn(async move {
        RepoGraphRetentionRepository::new(retention_db)
            .sweep(RetentionSweepRequest {
                project_id: PROJECT,
                mode: RetentionMode::Delete,
                history_n: 1,
            })
            .await
    });
    // A bounded timeout (not a sleep) proves retention cannot get past the
    // publisher's project lock to acquire/re-read the current pointer early.
    assert!(
        tokio::time::timeout(Duration::from_millis(250), &mut retention)
            .await
            .is_err()
    );
    let while_paused: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_current WHERE project_id=$1",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .expect("uncommitted pointer hidden");
    assert_eq!(
        while_paused, before,
        "uncommitted legacy pointer is invisible"
    );

    allow_commit.wait().await;
    tokio::time::timeout(Duration::from_secs(3), publisher)
        .await
        .expect("publisher unblocks")
        .expect("publisher task");
    let outcome = tokio::time::timeout(Duration::from_secs(3), retention)
        .await
        .expect("retention unblocks")
        .expect("retention task")
        .expect("retention result");
    assert_eq!(
        outcome.retries, 0,
        "consistent order needs no deadlock victim"
    );
    let republished: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_current WHERE project_id=$1",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .expect("republished current");
    assert_ne!(
        republished, before,
        "same-key upsert rotated current generation"
    );
    assert_old_reader_agrees_with_current(&db).await;
    assert_bounded_full_blobs(&db, 1).await;
}
