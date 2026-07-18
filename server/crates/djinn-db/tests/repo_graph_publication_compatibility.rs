//! Live-Postgres compatibility evidence for the graph-generation expand migration.
//!
//! These deliberately use the SQL surface that existed before migration 125;
//! do not replace these calls with repository APIs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor, Row};

const MIGRATION_VERSION: u64 = 127;
const PROJECT: &str = "publication-compat-project";

// Kept byte-for-byte semantically identical to RepoGraphCacheRepository::upsert
// before the generation expand work.  In particular, callers still supply no
// generation and the source timestamp is deliberately ignored by the trigger.
const LEGACY_UPSERT: &str = r#"INSERT INTO repo_graph_cache
             (project_id, commit_sha, graph_blob, built_at)
             VALUES ($1, $2, $3, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (project_id, commit_sha) DO UPDATE SET
                graph_blob = EXCLUDED.graph_blob,
                built_at = EXCLUDED.built_at"#;

// This is the old reader, intentionally not routed through a new selector.
const OLD_LATEST: &str = "SELECT project_id, commit_sha, graph_blob, built_at, generation_id::text AS generation_id \
                          FROM repo_graph_cache WHERE project_id = $1 ORDER BY built_at DESC LIMIT 1";

fn base_url() -> String {
    std::env::var("TEST_POSTGRES_URL")
        .or_else(|_| std::env::var("DJINN_TEST_DATABASE_URL"))
        .expect("live PostgreSQL URL")
}

async fn assert_strict_history_order(conn: &mut PgConnection) {
    let times: Vec<String> = sqlx::query_scalar(
        "SELECT built_at FROM repo_graph_generation WHERE project_id=$1 ORDER BY publish_seq",
    )
    .bind(PROJECT)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "committed legacy timestamps must be unique and increasing: {times:?}"
    );
}

fn prefix(url: &str) -> String {
    url.rsplit_once('/').expect("database in URL").0.to_owned()
}

fn migrations() -> Vec<(u64, PathBuf)> {
    let mut files: Vec<_> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations_postgres"))
            .expect("read migrations")
            .map(|entry| {
                let path = entry.expect("migration entry").path();
                let version = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.split_once('_'))
                    .and_then(|(n, _)| n.parse().ok())
                    .unwrap_or_default();
                (version, path)
            })
            .filter(|(_, p)| p.extension().and_then(|s| s.to_str()) == Some("sql"))
            .collect();
    files.sort_by_key(|(version, _)| *version);
    files
}

async fn migrate(conn: &mut PgConnection) {
    for (version, path) in migrations() {
        if version <= MIGRATION_VERSION && version != 0 {
            let sql = std::fs::read_to_string(&path).expect("read migration");
            conn.execute(sql.as_str())
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
    }
}

async fn legacy_publish(conn: &mut PgConnection, commit: &str, blob: &[u8]) {
    sqlx::query(LEGACY_UPSERT)
        .bind(PROJECT)
        .bind(commit)
        .bind(blob)
        .execute(conn)
        .await
        .expect("legacy upsert");
}

async fn old_and_current_agree(conn: &mut PgConnection) {
    let old = sqlx::query(OLD_LATEST)
        .bind(PROJECT)
        .fetch_one(&mut *conn)
        .await
        .expect("old latest row");
    let current = sqlx::query(
        "SELECT g.project_id, g.commit_sha, g.graph_blob, g.built_at, g.generation_id::text AS generation_id \
         FROM repo_graph_current c JOIN repo_graph_generation g \
           ON (g.project_id, g.generation_id) = (c.project_id, c.generation_id) \
         WHERE c.project_id = $1",
    ).bind(PROJECT).fetch_one(&mut *conn).await.expect("current generation");
    for column in ["project_id", "commit_sha", "built_at", "generation_id"] {
        assert_eq!(
            old.get::<String, _>(column),
            current.get::<String, _>(column),
            "{column}"
        );
    }
    assert_eq!(
        old.get::<Vec<u8>, _>("graph_blob"),
        current.get::<Vec<u8>, _>("graph_blob")
    );
}

async fn state(conn: &mut PgConnection) -> String {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
           'cache', coalesce((SELECT jsonb_agg(jsonb_build_object(
             'commit', commit_sha, 'blob', encode(graph_blob, 'hex'),
             'built_at', built_at, 'generation', generation_id::text) ORDER BY commit_sha)
             FROM repo_graph_cache WHERE project_id = $1), '[]'::jsonb),
           'history', coalesce((SELECT jsonb_agg(jsonb_build_object(
             'seq', publish_seq, 'commit', commit_sha, 'blob', encode(graph_blob, 'hex'),
             'built_at', built_at, 'generation', generation_id::text,
             'artifact_required', artifact_required) ORDER BY publish_seq)
             FROM repo_graph_generation WHERE project_id = $1), '[]'::jsonb),
           'current', coalesce((SELECT generation_id::text FROM repo_graph_current WHERE project_id = $1), ''),
           'clock', coalesce((SELECT last_built_at::text FROM repo_graph_publish_clock WHERE project_id = $1), ''))::text",
    ).bind(PROJECT).fetch_one(&mut *conn).await.expect("publication state")
}

async fn begin_marked(conn: &mut PgConnection, generation: uuid::Uuid) {
    conn.execute("BEGIN")
        .await
        .expect("begin marked transaction");
    sqlx::query("SELECT repo_graph_reserve_generation($1, $2::text::uuid)")
        .bind(PROJECT)
        .bind(generation.to_string())
        .execute(&mut *conn)
        .await
        .expect("reserve UUIDv7 generation");
}

async fn marked_cache(conn: &mut PgConnection, generation: uuid::Uuid, commit: &str) {
    sqlx::query("INSERT INTO repo_graph_cache (project_id, commit_sha, graph_blob, built_at, generation_id) \
                 VALUES ($1, $2, decode('a1', 'hex'), 'caller-supplied-time', $3::text::uuid)")
        .bind(PROJECT).bind(commit).bind(generation.to_string()).execute(&mut *conn).await.expect("marked cache row");
}

async fn insert_valid_artifact(
    conn: &mut PgConnection,
    generation: uuid::Uuid,
    artifact: uuid::Uuid,
) {
    sqlx::query("INSERT INTO repo_graph_galaxy_artifact \
        (artifact_id, generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
        VALUES ($1::text::uuid, $2::text::uuid, 'content-hash', '8a8950f7623663222542c9469c73be3c4c81bbdf019e2c577590a61f2ce9a157', 1, 1, '[\"chunk-hash\"]')")
        .bind(artifact.to_string()).bind(generation.to_string()).execute(&mut *conn).await.expect("artifact");
    sqlx::query(
        "INSERT INTO repo_graph_galaxy_chunk \
        (generation_id, artifact_id, chunk_index, byte_count, sha256, bytes) \
        VALUES ($1::text::uuid, $2::text::uuid, 0, 1, 'chunk-hash', decode('a1', 'hex'))",
    )
    .bind(generation.to_string())
    .bind(artifact.to_string())
    .execute(&mut *conn)
    .await
    .expect("artifact chunk");
}

async fn rejected_marked_commit(conn: &mut PgConnection, generation: uuid::Uuid, setup: &str) {
    let before = state(conn).await;
    begin_marked(conn, generation).await;
    marked_cache(conn, generation, "rejected").await;
    if !setup.is_empty() {
        conn.execute(setup).await.expect("invalid manifest setup");
    }
    assert!(
        conn.execute("COMMIT").await.is_err(),
        "invalid marked publication must fail"
    );
    assert_eq!(
        state(conn).await,
        before,
        "failed publication escaped its transaction"
    );
}

#[tokio::test]
async fn exact_legacy_sql_rotates_generations_orders_commits_and_preserves_rollbacks() {
    let admin_url = format!("{}/postgres", prefix(&base_url()));
    let name = format!("djinn_graph_compat_{}", uuid::Uuid::now_v7().simple());
    let mut admin = PgConnection::connect(&admin_url).await.expect("admin");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create db");
    drop(admin);
    let url = format!("{}/{}", prefix(&base_url()), name);
    let mut first = PgConnection::connect(&url).await.expect("first connection");
    migrate(&mut first).await;
    first.execute("INSERT INTO projects(id, name, github_owner, github_repo) VALUES ('publication-compat-project', 'publication compatibility', 'compat-owner', 'compat-repo')").await.expect("project");

    legacy_publish(&mut first, "same", b"one").await;
    old_and_current_agree(&mut first).await;
    let one: String = sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_cache WHERE project_id=$1 AND commit_sha='same'").bind(PROJECT).fetch_one(&mut first).await.unwrap();
    legacy_publish(&mut first, "same", b"two").await;
    old_and_current_agree(&mut first).await;
    let two: String = sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_cache WHERE project_id=$1 AND commit_sha='same'").bind(PROJECT).fetch_one(&mut first).await.unwrap();
    assert_ne!(one, two, "conflict updates must rotate generation identity");

    // Bind the actual preceding stored value: equal/stale/future source values
    // are all overwritten by the trigger clock.
    let preceding_built_at: String = sqlx::query_scalar(
        "SELECT built_at FROM repo_graph_cache WHERE project_id=$1 AND commit_sha='same'",
    )
    .bind(PROJECT)
    .fetch_one(&mut first)
    .await
    .unwrap();
    for (commit, source) in [
        ("equal", preceding_built_at.as_str()),
        ("stale", ""),
        ("future", "9999-12-31T00:00:00Z"),
    ] {
        sqlx::query("INSERT INTO repo_graph_cache (project_id, commit_sha, graph_blob, built_at) VALUES ($1, $2, decode('01','hex'), $3)")
            .bind(PROJECT).bind(commit).bind(source).execute(&mut first).await.unwrap();
        old_and_current_agree(&mut first).await;
    }
    assert_strict_history_order(&mut first).await;
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM repo_graph_generation WHERE project_id=$1"
        )
        .bind(PROJECT)
        .fetch_one(&mut first)
        .await
        .unwrap(),
        5
    );

    let before = state(&mut first).await;
    first.execute("BEGIN").await.unwrap();
    legacy_publish(&mut first, "rolled-back", b"no").await;
    first.execute("ROLLBACK").await.unwrap();
    assert_eq!(
        state(&mut first).await,
        before,
        "cache, history, clock and pointer roll back together"
    );

    // Both transactions overlap; the second statement blocks on the first's lock.
    let mut second = PgConnection::connect(&url)
        .await
        .expect("second connection");
    first.execute("BEGIN").await.unwrap();
    second.execute("BEGIN").await.unwrap();
    legacy_publish(&mut first, "first-commit", b"first").await;
    let blocked = tokio::spawn(async move {
        legacy_publish(&mut second, "later-commit", b"later").await;
        second
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !blocked.is_finished(),
        "second publication must wait for project lock"
    );
    first.execute("COMMIT").await.unwrap();
    old_and_current_agree(&mut first).await;
    let first_observation: (String, String) = sqlx::query_as("SELECT generation_id::text, built_at FROM repo_graph_cache WHERE project_id=$1 AND commit_sha='first-commit'").bind(PROJECT).fetch_one(&mut first).await.unwrap();
    let mut second = tokio::time::timeout(Duration::from_secs(2), blocked)
        .await
        .expect("second publisher must unblock without deadlock")
        .unwrap();
    second.execute("COMMIT").await.unwrap();
    let latest = sqlx::query(OLD_LATEST)
        .bind(PROJECT)
        .fetch_one(&mut first)
        .await
        .unwrap();
    assert_eq!(latest.get::<String, _>("commit_sha"), "later-commit");
    old_and_current_agree(&mut first).await;
    let later_observation: (String, String) = sqlx::query_as("SELECT generation_id::text, built_at FROM repo_graph_cache WHERE project_id=$1 AND commit_sha='later-commit'").bind(PROJECT).fetch_one(&mut first).await.unwrap();
    assert_ne!(first_observation.0, later_observation.0);
    assert!(first_observation.1 < later_observation.1);
    assert_strict_history_order(&mut first).await;

    drop(second);
    drop(first);
    let mut admin = PgConnection::connect(&admin_url).await.unwrap();
    admin
        .execute(format!(r#"DROP DATABASE "{name}""#).as_str())
        .await
        .unwrap();
}

#[tokio::test]
async fn marked_publications_require_a_reserved_v7_complete_manifest_and_legacy_clears_artifacts() {
    let admin_url = format!("{}/postgres", prefix(&base_url()));
    let name = format!("djinn_graph_marked_{}", uuid::Uuid::now_v7().simple());
    let mut admin = PgConnection::connect(&admin_url).await.unwrap();
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .unwrap();
    drop(admin);
    let url = format!("{}/{}", prefix(&base_url()), name);
    let mut conn = PgConnection::connect(&url).await.unwrap();
    migrate(&mut conn).await;
    conn.execute("INSERT INTO projects(id, name, github_owner, github_repo) VALUES ('publication-compat-project', 'marked compatibility', 'marked-owner', 'marked-repo')").await.unwrap();
    legacy_publish(&mut conn, "baseline", b"base").await;
    old_and_current_agree(&mut conn).await;

    let generation = uuid::Uuid::now_v7();
    let artifact = uuid::Uuid::now_v7();
    begin_marked(&mut conn, generation).await;
    marked_cache(&mut conn, generation, "artifact").await;
    insert_valid_artifact(&mut conn, generation, artifact).await;
    conn.execute("COMMIT")
        .await
        .expect("valid marked publication");
    old_and_current_agree(&mut conn).await;
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT artifact_required FROM repo_graph_generation WHERE generation_id=$1::text::uuid"
        )
        .bind(generation.to_string())
        .fetch_one(&mut conn)
        .await
        .unwrap()
    );

    // An explicit identity is never accepted without a transaction-owned marker.
    let before = state(&mut conn).await;
    conn.execute("BEGIN").await.unwrap();
    assert!(sqlx::query("INSERT INTO repo_graph_cache (project_id,commit_sha,graph_blob,built_at,generation_id) VALUES ($1,'no-marker',decode('a1','hex'),'caller',$2::text::uuid)")
        .bind(PROJECT).bind(uuid::Uuid::now_v7().to_string()).execute(&mut conn).await.is_err());
    conn.execute("ROLLBACK").await.unwrap();
    assert_eq!(state(&mut conn).await, before, "unmarked identity escaped");

    // The reservation's UUID must equal the cache row's explicit UUID.
    let before = state(&mut conn).await;
    begin_marked(&mut conn, uuid::Uuid::now_v7()).await;
    assert!(sqlx::query("INSERT INTO repo_graph_cache (project_id,commit_sha,graph_blob,built_at,generation_id) VALUES ($1,'marker-mismatch',decode('a1','hex'),'caller',$2::text::uuid)")
        .bind(PROJECT).bind(uuid::Uuid::now_v7().to_string()).execute(&mut conn).await.is_err());
    conn.execute("ROLLBACK").await.unwrap();
    assert_eq!(state(&mut conn).await, before, "marker mismatch escaped");

    let before = state(&mut conn).await;
    conn.execute("BEGIN").await.unwrap();
    let non_v7 = uuid::Uuid::new_v4();
    assert!(
        sqlx::query("SELECT repo_graph_reserve_generation($1, $2::text::uuid)")
            .bind(PROJECT)
            .bind(non_v7.to_string())
            .execute(&mut conn)
            .await
            .is_err()
    );
    conn.execute("ROLLBACK").await.unwrap();
    assert_eq!(state(&mut conn).await, before, "non-v7 marker escaped");

    // This is an immutable-only collision, independently of compatibility.
    let immutable_collision = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO repo_graph_generation (generation_id,project_id,commit_sha,graph_blob,built_at) VALUES ($1::text::uuid,$2,'immutable-collision',decode('a1','hex'),'0000')")
        .bind(immutable_collision.to_string()).bind(PROJECT).execute(&mut conn).await.unwrap();
    let before = state(&mut conn).await;
    conn.execute("BEGIN").await.unwrap();
    assert!(
        sqlx::query("SELECT repo_graph_reserve_generation($1, $2::text::uuid)")
            .bind(PROJECT)
            .bind(immutable_collision.to_string())
            .execute(&mut conn)
            .await
            .is_err()
    );
    conn.execute("ROLLBACK").await.unwrap();
    assert_eq!(
        state(&mut conn).await,
        before,
        "immutable-only collision advanced state"
    );

    // Construct a compatibility-only collision outside publication triggers.
    let cache_collision = uuid::Uuid::now_v7();
    conn.execute("ALTER TABLE repo_graph_cache DISABLE TRIGGER ALL")
        .await
        .unwrap();
    sqlx::query("INSERT INTO repo_graph_cache (project_id,commit_sha,graph_blob,built_at,generation_id) VALUES ($1,'cache-collision',decode('a1','hex'),'0000',$2::text::uuid)")
        .bind(PROJECT).bind(cache_collision.to_string()).execute(&mut conn).await.unwrap();
    conn.execute("ALTER TABLE repo_graph_cache ENABLE TRIGGER ALL")
        .await
        .unwrap();
    let before = state(&mut conn).await;
    conn.execute("BEGIN").await.unwrap();
    assert!(
        sqlx::query("SELECT repo_graph_reserve_generation($1,$2::text::uuid)")
            .bind(PROJECT)
            .bind(cache_collision.to_string())
            .execute(&mut conn)
            .await
            .is_err()
    );
    conn.execute("ROLLBACK").await.unwrap();
    assert_eq!(
        state(&mut conn).await,
        before,
        "compatibility-only collision overwrote state"
    );

    // Artifact absent and artifact with a missing chunk are distinct incomplete manifests.
    rejected_marked_commit(&mut conn, uuid::Uuid::now_v7(), "").await;
    let missing_chunk_generation = uuid::Uuid::now_v7();
    let missing_chunk_artifact = uuid::Uuid::now_v7();
    let missing_chunk = format!(
        "INSERT INTO repo_graph_galaxy_artifact (artifact_id,generation_id,graph_content_hash,transport_sha256,chunk_count,byte_count,chunk_hashes) VALUES ('{missing_chunk_artifact}','{missing_chunk_generation}','a','b',1,1,'[\"x\"]')"
    );
    rejected_marked_commit(&mut conn, missing_chunk_generation, &missing_chunk).await;
    let gap_generation = uuid::Uuid::now_v7();
    let gap_artifact = uuid::Uuid::now_v7();
    let gap = format!(
        "INSERT INTO repo_graph_galaxy_artifact (artifact_id,generation_id,graph_content_hash,transport_sha256,chunk_count,byte_count,chunk_hashes) VALUES ('{gap_artifact}','{gap_generation}','a','b',2,1,'[\"x\",\"y\"]'); INSERT INTO repo_graph_galaxy_chunk (generation_id,artifact_id,chunk_index,byte_count,sha256,bytes) VALUES ('{gap_generation}','{gap_artifact}',1,1,'y',decode('01','hex'))"
    );
    rejected_marked_commit(&mut conn, gap_generation, &gap).await;
    let hash_generation = uuid::Uuid::now_v7();
    let hash_artifact = uuid::Uuid::now_v7();
    let wrong_hash = format!(
        "INSERT INTO repo_graph_galaxy_artifact (artifact_id,generation_id,graph_content_hash,transport_sha256,chunk_count,byte_count,chunk_hashes) VALUES ('{hash_artifact}','{hash_generation}','a','b',1,1,'[\"expected\"]'); INSERT INTO repo_graph_galaxy_chunk (generation_id,artifact_id,chunk_index,byte_count,sha256,bytes) VALUES ('{hash_generation}','{hash_artifact}',0,1,'wrong',decode('01','hex'))"
    );
    rejected_marked_commit(&mut conn, hash_generation, &wrong_hash).await;

    let bytes_generation = uuid::Uuid::now_v7();
    let bytes_artifact = uuid::Uuid::now_v7();
    let wrong_bytes = format!(
        "INSERT INTO repo_graph_galaxy_artifact (artifact_id,generation_id,graph_content_hash,transport_sha256,chunk_count,byte_count,chunk_hashes) VALUES ('{bytes_artifact}','{bytes_generation}','a','8a8950f7623663222542c9469c73be3c4c81bbdf019e2c577590a61f2ce9a157',1,2,'[\"x\"]'); INSERT INTO repo_graph_galaxy_chunk (generation_id,artifact_id,chunk_index,byte_count,sha256,bytes) VALUES ('{bytes_generation}','{bytes_artifact}',0,1,'x',decode('a1','hex'))"
    );
    rejected_marked_commit(&mut conn, bytes_generation, &wrong_bytes).await;

    let transport_generation = uuid::Uuid::now_v7();
    let transport_artifact = uuid::Uuid::now_v7();
    let wrong_transport = format!(
        "INSERT INTO repo_graph_galaxy_artifact (artifact_id,generation_id,graph_content_hash,transport_sha256,chunk_count,byte_count,chunk_hashes) VALUES ('{transport_artifact}','{transport_generation}','a','wrong-aggregate',1,1,'[\"x\"]'); INSERT INTO repo_graph_galaxy_chunk (generation_id,artifact_id,chunk_index,byte_count,sha256,bytes) VALUES ('{transport_generation}','{transport_artifact}',0,1,'x',decode('a1','hex'))"
    );
    rejected_marked_commit(&mut conn, transport_generation, &wrong_transport).await;

    // A complete valid manifest stays tentative until its caller commits.
    let before = state(&mut conn).await;
    let rollback_generation = uuid::Uuid::now_v7();
    begin_marked(&mut conn, rollback_generation).await;
    marked_cache(&mut conn, rollback_generation, "forced-rollback").await;
    insert_valid_artifact(&mut conn, rollback_generation, uuid::Uuid::now_v7()).await;
    conn.execute("ROLLBACK").await.unwrap();
    assert_eq!(
        state(&mut conn).await,
        before,
        "forced rollback escaped valid publication"
    );

    // A later old writer is unmarked and therefore advances to an artifactless generation.
    legacy_publish(&mut conn, "legacy-after-artifact", b"legacy").await;
    old_and_current_agree(&mut conn).await;
    let current_required: bool = sqlx::query_scalar("SELECT g.artifact_required FROM repo_graph_current c JOIN repo_graph_generation g ON g.generation_id=c.generation_id WHERE c.project_id=$1").bind(PROJECT).fetch_one(&mut conn).await.unwrap();
    assert!(
        !current_required,
        "legacy publication must not implicitly reuse old artifact"
    );

    drop(conn);
    let mut admin = PgConnection::connect(&admin_url).await.unwrap();
    admin
        .execute(format!(r#"DROP DATABASE "{name}""#).as_str())
        .await
        .unwrap();
}
