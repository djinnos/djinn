//! Live-Postgres full-warm publication regression coverage.
//!
//! This deliberately rebuilds all validation inputs from rows read back from
//! Postgres. It never uses producer-calculated hashes as the assertion oracle.

use std::io::Read;
use std::sync::OnceLock;

use djinn_db::repositories::repo_graph_generation::ReservedPublicationFailureStage;
use djinn_db::{
    CurrentGalaxyArtifact, Database, DatabaseConnectConfig, PostgresDatabaseConfig,
    RepoGraphCacheRepository, RepoGraphGenerationRepository, ReservedGalaxyArtifactChunk,
    ReservedGalaxyArtifactManifest, ReservedGraphPublication,
};
use sha2::{Digest, Sha256};
use sqlx::{Row, postgres::PgRow};

use super::{
    ArtifactSizeCap, GalaxyArtifact, GalaxyArtifactError, GalaxyArtifactInput,
    GalaxySnapshotPayload, GenerationId, build_galaxy_artifact,
};

const PROJECT: &str = "full-warm-publication-regression";
const LEGACY_UPSERT: &str = r#"INSERT INTO repo_graph_cache
             (project_id, commit_sha, graph_blob, built_at)
             VALUES ($1, $2, $3, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (project_id, commit_sha) DO UPDATE SET
                graph_blob = EXCLUDED.graph_blob,
                built_at = EXCLUDED.built_at"#;
const OLD_LATEST: &str = "SELECT project_id, commit_sha, graph_blob, built_at, generation_id::text AS generation_id \
                          FROM repo_graph_cache WHERE project_id = $1 ORDER BY built_at DESC LIMIT 1";

fn database_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

async fn fresh() -> (Database, RepoGraphGenerationRepository) {
    let url = std::env::var("TEST_POSTGRES_URL").expect("live Postgres URL");
    let db = Database::open_with_config(DatabaseConnectConfig::Postgres(PostgresDatabaseConfig {
        url,
    }))
    .expect("open live Postgres database");
    db.ensure_initialized()
        .await
        .expect("migrate live database");
    sqlx::query("DELETE FROM projects WHERE id=$1")
        .bind(PROJECT)
        .execute(db.pool())
        .await
        .expect("clear previous test project");
    sqlx::query("INSERT INTO projects(id, name, github_owner, github_repo) VALUES ($1, 'full warm publication regression', 'test-owner', 'test-repo')")
        .bind(PROJECT)
        .execute(db.pool())
        .await
        .expect("seed project");
    let repo = RepoGraphGenerationRepository::new(db.clone());
    (db, repo)
}

fn build_artifact(id: GenerationId, cap: ArtifactSizeCap) -> GalaxyArtifact {
    let graph = crate::test_helpers::td55_equivalence_fixture_graph();
    build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: PROJECT.to_owned(),
        git_head: "full-warm-commit".to_owned(),
        generated_at: "2026-07-18T00:00:00Z".to_owned(),
        generation_id: id,
        size_cap: cap,
    })
    .expect("deterministic small full-warm artifact")
}

fn publication(artifact: &GalaxyArtifact) -> ReservedGraphPublication {
    let generation_id = artifact.generation_id.as_str();
    ReservedGraphPublication {
        project_id: PROJECT.to_owned(),
        commit_sha: "full-warm-commit".to_owned(),
        generation_id: generation_id.clone(),
        graph_blob: crate::test_helpers::td55_equivalence_fixture_artifact_blob(),
        artifact: ReservedGalaxyArtifactManifest {
            artifact_id: generation_id.clone(),
            generation_id: generation_id.clone(),
            graph_content_hash: artifact.graph_content_hash.clone(),
            transport_sha256: artifact.spool.transport_sha256.clone(),
            chunk_count: i32::try_from(artifact.spool.chunks.len()).expect("chunk count"),
            byte_count: i64::try_from(artifact.spool.total_compressed_bytes).expect("byte count"),
            chunk_hashes: artifact.spool.chunk_hashes.clone(),
        },
        chunks: artifact
            .spool
            .chunks
            .iter()
            .map(|chunk| ReservedGalaxyArtifactChunk {
                generation_id: generation_id.clone(),
                artifact_id: generation_id.clone(),
                chunk_index: i32::try_from(chunk.index).expect("chunk index"),
                sha256: chunk.sha256.clone(),
                bytes: chunk.bytes.clone(),
            })
            .collect(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    cache: i64,
    generations: i64,
    current: Option<String>,
    artifacts: i64,
    chunks: i64,
    clock: Option<String>,
}

async fn snapshot(db: &Database) -> Snapshot {
    Snapshot {
        cache: sqlx::query_scalar("SELECT count(*) FROM repo_graph_cache WHERE project_id=$1").bind(PROJECT).fetch_one(db.pool()).await.unwrap(),
        generations: sqlx::query_scalar("SELECT count(*) FROM repo_graph_generation WHERE project_id=$1").bind(PROJECT).fetch_one(db.pool()).await.unwrap(),
        current: sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_current WHERE project_id=$1").bind(PROJECT).fetch_optional(db.pool()).await.unwrap(),
        artifacts: sqlx::query_scalar("SELECT count(*) FROM repo_graph_galaxy_artifact a JOIN repo_graph_generation g ON g.generation_id=a.generation_id WHERE g.project_id=$1").bind(PROJECT).fetch_one(db.pool()).await.unwrap(),
        chunks: sqlx::query_scalar("SELECT count(*) FROM repo_graph_galaxy_chunk c JOIN repo_graph_generation g ON g.generation_id=c.generation_id WHERE g.project_id=$1").bind(PROJECT).fetch_one(db.pool()).await.unwrap(),
        clock: sqlx::query_scalar("SELECT last_built_at::text FROM repo_graph_publish_clock WHERE project_id=$1").bind(PROJECT).fetch_optional(db.pool()).await.unwrap(),
    }
}

fn row_string(row: &PgRow, name: &str) -> String {
    row.get(name)
}

#[tokio::test]
async fn full_warm_publishes_one_identity_and_stored_artifact_recomputes_independently() {
    let _serial = database_lock().lock().await;
    let (db, repo) = fresh().await;
    let id = GenerationId::new(uuid::Uuid::now_v7()).unwrap();
    let artifact = build_artifact(id, ArtifactSizeCap::default());
    let expected_id = artifact.generation_id.as_str();
    repo.publish_reserved_generation(publication(&artifact))
        .await
        .expect("publish full warm");

    let compatibility = sqlx::query("SELECT generation_id::text AS generation_id FROM repo_graph_cache WHERE project_id=$1 AND commit_sha='full-warm-commit'").bind(PROJECT).fetch_one(db.pool()).await.unwrap();
    let generation = repo
        .generation_by_id(&expected_id)
        .await
        .unwrap()
        .expect("immutable generation");
    let current: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_current WHERE project_id=$1",
    )
    .bind(PROJECT)
    .fetch_one(db.pool())
    .await
    .unwrap();
    let metadata = sqlx::query("SELECT artifact_id::text AS artifact_id, generation_id::text AS generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes::text AS chunk_hashes FROM repo_graph_galaxy_artifact WHERE generation_id=$1::uuid").bind(&expected_id).fetch_one(db.pool()).await.unwrap();
    let chunks = sqlx::query("SELECT generation_id::text AS generation_id, artifact_id::text AS artifact_id, chunk_index, sha256, bytes FROM repo_graph_galaxy_chunk WHERE generation_id=$1::uuid ORDER BY chunk_index").bind(&expected_id).fetch_all(db.pool()).await.unwrap();

    assert_eq!(row_string(&compatibility, "generation_id"), expected_id);
    assert_eq!(generation.generation_id, expected_id);
    assert!(generation.artifact_required);
    assert_eq!(current, expected_id);
    assert_eq!(row_string(&metadata, "artifact_id"), expected_id);
    assert_eq!(row_string(&metadata, "generation_id"), expected_id);
    assert_eq!(chunks.len(), artifact.spool.chunks.len());

    let mut compressed = Vec::new();
    let mut manifest_hashes = Vec::new();
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(row_string(chunk, "generation_id"), expected_id);
        assert_eq!(row_string(chunk, "artifact_id"), expected_id);
        assert_eq!(chunk.get::<i32, _>("chunk_index"), index as i32);
        let bytes: Vec<u8> = chunk.get("bytes");
        let chunk_hash = sha256(&bytes);
        assert_eq!(chunk_hash, row_string(chunk, "sha256"));
        compressed.extend_from_slice(&bytes);
        manifest_hashes.push(chunk_hash);
    }
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&row_string(&metadata, "chunk_hashes")).unwrap(),
        manifest_hashes
    );
    assert_eq!(
        sha256(&compressed),
        row_string(&metadata, "transport_sha256")
    );
    assert_eq!(
        compressed.len() as i64,
        metadata.get::<i64, _>("byte_count")
    );

    let mut payload = Vec::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .read_to_end(&mut payload)
        .unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    let object = value.as_object_mut().unwrap();
    assert_eq!(object["generation_id"], expected_id);
    assert_eq!(
        object["graph_content_hash"],
        row_string(&metadata, "graph_content_hash")
    );
    assert!(!object.contains_key("transport_sha256"));
    // Rebuild the semantic wire model from the decompressed stored JSON rather
    // than consuming the producer's retained hash-input bytes.
    let mut semantic: GalaxySnapshotPayload = serde_json::from_slice(&payload).unwrap();
    semantic.graph_content_hash = None;
    assert_eq!(
        sha256(&serde_json::to_vec(&semantic).unwrap()),
        row_string(&metadata, "graph_content_hash")
    );
}

#[tokio::test]
async fn full_warm_failures_preserve_previous_pointer_and_every_table() {
    let _serial = database_lock().lock().await;
    let (db, repo) = fresh().await;
    sqlx::query(LEGACY_UPSERT)
        .bind(PROJECT)
        .bind("old")
        .bind(b"old graph".as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    let before = snapshot(&db).await;
    let graph = crate::test_helpers::td55_equivalence_fixture_graph();
    let cap = build_galaxy_artifact(GalaxyArtifactInput {
        graph: &graph,
        project_id: PROJECT.to_owned(),
        git_head: "too-big".to_owned(),
        generated_at: "2026-07-18T00:00:00Z".to_owned(),
        generation_id: GenerationId::new(uuid::Uuid::now_v7()).unwrap(),
        size_cap: ArtifactSizeCap::compressed_bytes(1),
    });
    assert!(matches!(cap, Err(GalaxyArtifactError::Oversize { .. })));
    assert_eq!(
        snapshot(&db).await,
        before,
        "size cap must not open publication"
    );
    for stage in [
        ReservedPublicationFailureStage::CompatibilityUpsert,
        ReservedPublicationFailureStage::ArtifactInsert,
        ReservedPublicationFailureStage::FirstChunkInsert,
        ReservedPublicationFailureStage::Commit,
    ] {
        let artifact = build_artifact(
            GenerationId::new(uuid::Uuid::now_v7()).unwrap(),
            ArtifactSizeCap::default(),
        );
        assert!(
            repo.publish_reserved_generation_with_failure(publication(&artifact), stage)
                .await
                .is_err()
        );
        assert_eq!(
            snapshot(&db).await,
            before,
            "{stage:?} leaked cache/generation/metadata/chunks/current/clock"
        );
    }
}

#[tokio::test]
async fn unchanged_old_reader_sees_new_warm_and_unmarked_legacy_advances_artifactless_current() {
    let _serial = database_lock().lock().await;
    let (db, repo) = fresh().await;
    let artifact = build_artifact(
        GenerationId::new(uuid::Uuid::now_v7()).unwrap(),
        ArtifactSizeCap::default(),
    );
    let published = publication(&artifact);
    let blob = published.graph_blob.clone();
    let generation_id = published.generation_id.clone();
    repo.publish_reserved_generation(published).await.unwrap();
    let old = sqlx::query(OLD_LATEST)
        .bind(PROJECT)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(row_string(&old, "project_id"), PROJECT);
    assert_eq!(row_string(&old, "commit_sha"), "full-warm-commit");
    assert_eq!(old.get::<Vec<u8>, _>("graph_blob"), blob);
    assert_eq!(row_string(&old, "generation_id"), generation_id);

    sqlx::query(LEGACY_UPSERT)
        .bind(PROJECT)
        .bind("legacy-after-warm")
        .bind(b"legacy graph".as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    match repo
        .current_galaxy_artifact_for_project(PROJECT)
        .await
        .unwrap()
    {
        CurrentGalaxyArtifact::ArtifactUnavailable { generation } => {
            assert_eq!(generation.commit_sha, "legacy-after-warm")
        }
        other => panic!(
            "legacy upsert must advance to an artifactless current generation, got {other:?}"
        ),
    }
    let cache = RepoGraphCacheRepository::new(db);
    assert_eq!(
        cache
            .latest_for_project(PROJECT)
            .await
            .unwrap()
            .unwrap()
            .commit_sha,
        "legacy-after-warm"
    );
}
