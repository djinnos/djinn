
use super::*;
use crate::database::Database;
use crate::repositories::repo_graph_cache::{RepoGraphCacheInsert, RepoGraphCacheRepository};

async fn fresh() -> (Database, RepoGraphGenerationRepository) {
    let db = Database::open_in_memory().expect("in-memory db");
    db.ensure_initialized().await.expect("initialize database");
    let repo = RepoGraphGenerationRepository::new(db.clone());
    (db, repo)
}

async fn insert_project(db: &Database, project_id: &str) {
    sqlx::query(
        "INSERT INTO projects(id, name, github_owner, github_repo) \
             VALUES ($1, $2, 'test-owner', 'test-repo')",
    )
    .bind(project_id)
    .bind(format!("test project {project_id}"))
    .execute(db.pool())
    .await
    .expect("insert project");
}

/// Publish via the legacy unmarked cache upsert.  The migration triggers
/// mint a fresh generation (artifact_required = false) and advance
/// `repo_graph_current`.
async fn legacy_publish(db: &Database, project_id: &str, commit_sha: &str, blob: &[u8]) {
    let cache_repo = RepoGraphCacheRepository::new(db.clone());
    cache_repo
        .upsert(RepoGraphCacheInsert {
            project_id,
            commit_sha,
            graph_blob: blob,
        })
        .await
        .expect("legacy upsert");
}

/// Publish a marked (reserved) generation with a valid single-chunk galaxy
/// artifact so the deferred validation trigger accepts the commit.
async fn reserved_publish_with_artifact(
    repo: &RepoGraphGenerationRepository,
    project_id: &str,
    commit_sha: &str,
    blob: &[u8],
) -> (String, String) {
    let generation_id = uuid::Uuid::now_v7();
    let artifact_id = uuid::Uuid::now_v7();
    let gen_str = generation_id.to_string();
    let art_str = artifact_id.to_string();

    let chunk_hash = sha256_hex(blob);
    repo.publish_reserved_generation(ReservedGraphPublication {
        project_id: project_id.to_owned(),
        commit_sha: commit_sha.to_owned(),
        generation_id: gen_str.clone(),
        graph_blob: blob.to_vec(),
        artifact: ReservedGalaxyArtifactManifest {
            artifact_id: art_str.clone(),
            generation_id: gen_str.clone(),
            // The pinned-reader selector validates both hash domains
            // before it can acquire its session pin. Keep this fixture
            // representative so its lock assertions reach that protocol.
            graph_content_hash: sha256_hex(b"semantic graph domain"),
            transport_sha256: chunk_hash.clone(),
            chunk_count: 1,
            byte_count: blob.len() as i64,
            chunk_hashes: vec![chunk_hash.clone()],
        },
        chunks: vec![ReservedGalaxyArtifactChunk {
            generation_id: gen_str.clone(),
            artifact_id: art_str.clone(),
            chunk_index: 0,
            sha256: chunk_hash,
            bytes: blob.to_vec(),
        }],
    })
    .await
    .expect("publish reserved artifact");
    (gen_str, art_str)
}

fn valid_publication() -> ReservedGraphPublication {
    let bytes = b"firstsecond";
    let first_hash = sha256_hex(b"first");
    let second_hash = sha256_hex(b"second");
    ReservedGraphPublication {
        project_id: "project".to_owned(),
        commit_sha: "commit".to_owned(),
        generation_id: "generation".to_owned(),
        graph_blob: b"graph".to_vec(),
        artifact: ReservedGalaxyArtifactManifest {
            artifact_id: "artifact".to_owned(),
            generation_id: "generation".to_owned(),
            graph_content_hash: "graph-hash".to_owned(),
            transport_sha256: sha256_hex(bytes),
            chunk_count: 2,
            byte_count: bytes.len() as i64,
            chunk_hashes: vec![first_hash.clone(), second_hash.clone()],
        },
        chunks: vec![
            ReservedGalaxyArtifactChunk {
                generation_id: "generation".to_owned(),
                artifact_id: "artifact".to_owned(),
                chunk_index: 0,
                sha256: first_hash,
                bytes: b"first".to_vec(),
            },
            ReservedGalaxyArtifactChunk {
                generation_id: "generation".to_owned(),
                artifact_id: "artifact".to_owned(),
                chunk_index: 1,
                sha256: second_hash,
                bytes: b"second".to_vec(),
            },
        ],
    }
}

#[test]
fn reserved_publication_validation_rejects_manifest_and_transport_mismatches() {
    assert!(validate_reserved_publication(&valid_publication()).is_ok());

    let mut identity = valid_publication();
    identity.chunks[0].artifact_id = "other".to_owned();
    assert!(validate_reserved_publication(&identity).is_err());

    let mut gap = valid_publication();
    gap.chunks[1].chunk_index = 2;
    assert!(validate_reserved_publication(&gap).is_err());

    let mut oversized = valid_publication();
    oversized.chunks[0].bytes = vec![0; MAX_GALAXY_CHUNK_BYTES + 1];
    assert!(validate_reserved_publication(&oversized).is_err());

    let mut wrong_count = valid_publication();
    wrong_count.artifact.chunk_count = 1;
    assert!(validate_reserved_publication(&wrong_count).is_err());

    let mut wrong_bytes = valid_publication();
    wrong_bytes.artifact.byte_count += 1;
    assert!(validate_reserved_publication(&wrong_bytes).is_err());

    let mut wrong_manifest = valid_publication();
    wrong_manifest.artifact.chunk_hashes.swap(0, 1);
    assert!(validate_reserved_publication(&wrong_manifest).is_err());

    let mut wrong_transport = valid_publication();
    wrong_transport.artifact.transport_sha256 = "wrong".to_owned();
    assert!(validate_reserved_publication(&wrong_transport).is_err());
}

#[tokio::test]
async fn select_current_follows_repo_graph_current_pointer() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-current").await;
    legacy_publish(&db, "p-current", "commit-1", b"graph-blob-1").await;

    let selected = repo
        .select_project_current_graph("p-current")
        .await
        .expect("select");
    match selected {
        ProjectCurrentGraph::Current(generation) => {
            assert_eq!(generation.project_id, "p-current");
            assert_eq!(generation.commit_sha, "commit-1");
            assert_eq!(generation.graph_blob, b"graph-blob-1");
        }
        other => panic!("expected Current, got {other:?}"),
    }

    // The pointer-based read agrees with the explicit current lookup.
    let by_current = repo
        .current_generation_for_project("p-current")
        .await
        .expect("current")
        .expect("generation exists");
    assert_eq!(by_current.commit_sha, "commit-1");
}

#[tokio::test]
async fn two_same_commit_generations_choose_greatest_publish_seq() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-seq").await;
    legacy_publish(&db, "p-seq", "same", b"v1").await;
    legacy_publish(&db, "p-seq", "same", b"v2").await;

    let latest = repo
        .latest_for_project_commit("p-seq", "same")
        .await
        .expect("latest")
        .expect("generation exists");
    // The second publication has the greater publish_seq and overwrote
    // the compatibility graph_blob, so the selector must return v2.
    assert_eq!(latest.graph_blob, b"v2");

    let greatest = repo
        .greatest_publish_seq_for_project_commit("p-seq", "same")
        .await
        .expect("greatest seq");
    assert_eq!(greatest, Some(latest.publish_seq));
}

#[tokio::test]
async fn legacy_fallback_only_when_pointer_is_absent() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-fallback").await;
    legacy_publish(&db, "p-fallback", "fb-commit", b"fb-blob").await;

    // A pointer exists, so the selector must never fall back.
    let with_pointer = repo
        .select_project_current_graph("p-fallback")
        .await
        .expect("select with pointer");
    assert!(
        matches!(with_pointer, ProjectCurrentGraph::Current(_)),
        "pointer exists: expected Current, got {with_pointer:?}"
    );

    // Simulate the pre-backfill state by removing the pointer while the
    // compatibility row remains.
    sqlx::query("DELETE FROM repo_graph_current WHERE project_id = 'p-fallback'")
        .execute(db.pool())
        .await
        .expect("delete current pointer");

    let without_pointer = repo
        .select_project_current_graph("p-fallback")
        .await
        .expect("select without pointer");
    match without_pointer {
        ProjectCurrentGraph::LegacyFallback(graph) => {
            assert_eq!(graph.project_id, "p-fallback");
            assert_eq!(graph.commit_sha, "fb-commit");
            assert_eq!(graph.graph_blob, b"fb-blob");
        }
        other => panic!("no pointer: expected LegacyFallback, got {other:?}"),
    }

    // A project with neither pointer nor cache row is Unavailable.
    let empty = repo
        .select_project_current_graph("p-fallback-nonexistent")
        .await
        .expect("select empty");
    assert_eq!(empty, ProjectCurrentGraph::Unavailable);
}

#[tokio::test]
async fn artifactless_current_is_distinct_from_no_pointer() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-artifactless").await;
    legacy_publish(&db, "p-artifactless", "c1", b"blob").await;

    // A current generation exists but carries no galaxy artifact.
    let with_gen = repo
        .current_galaxy_artifact_for_project("p-artifactless")
        .await
        .expect("artifact status");
    match with_gen {
        CurrentGalaxyArtifact::ArtifactUnavailable { generation } => {
            assert_eq!(generation.commit_sha, "c1");
        }
        other => panic!("expected ArtifactUnavailable, got {other:?}"),
    }

    // Removing the pointer yields NoCurrentGeneration — distinct from
    // having a current generation without an artifact.
    sqlx::query("DELETE FROM repo_graph_current WHERE project_id = 'p-artifactless'")
        .execute(db.pool())
        .await
        .expect("delete current pointer");
    let no_gen = repo
        .current_galaxy_artifact_for_project("p-artifactless")
        .await
        .expect("artifact status");
    assert_eq!(no_gen, CurrentGalaxyArtifact::NoCurrentGeneration);
}

#[tokio::test]
async fn current_artifact_metadata_has_distinct_hash_domains() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-meta").await;
    reserved_publish_with_artifact(&repo, "p-meta", "meta-commit", b"meta-blob").await;

    let result = repo
        .current_galaxy_artifact_for_project("p-meta")
        .await
        .expect("artifact");
    match result {
        CurrentGalaxyArtifact::Available {
            generation,
            artifact,
        } => {
            assert_eq!(generation.commit_sha, "meta-commit");
            assert_eq!(artifact.chunk_count, 1);
            assert_eq!(artifact.byte_count, b"meta-blob".len() as i64);
            assert_ne!(
                artifact.graph_content_hash, artifact.transport_sha256,
                "graph_content_hash and transport_sha256 must be distinct domains"
            );
        }
        other => panic!("expected Available, got {other:?}"),
    }
}

#[tokio::test]
async fn pinned_reader_shared_pin_survives_commit_and_releases() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-pin").await;
    let (generation_id, _) =
        reserved_publish_with_artifact(&repo, "p-pin", "pin-commit", b"pin-blob").await;
    let key = generation_stream_pin_key(&generation_id).expect("pin key");

    let reader = match repo
        .pin_current_galaxy_artifact("p-pin")
        .await
        .expect("select pinned artifact")
    {
        PinnedGalaxyArtifactSelection::Pinned(reader) => reader,
        other => panic!("expected pinned artifact, got {other:?}"),
    };
    let mut contender = db.pool().acquire().await.expect("contender connection");
    assert!(
        !try_acquire_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .expect("try exclusive while pinned"),
        "the shared pin must survive selector commit"
    );
    reader.finish().await.expect("finish reader");
    assert!(
        try_acquire_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .expect("try exclusive after finish"),
        "finish must release the shared pin"
    );
    assert!(
        release_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .expect("release exclusive")
    );

    let reader = match repo
        .pin_current_galaxy_artifact("p-pin")
        .await
        .expect("select second pinned artifact")
    {
        PinnedGalaxyArtifactSelection::Pinned(reader) => reader,
        other => panic!("expected pinned artifact, got {other:?}"),
    };
    drop(reader);
    assert!(
        try_acquire_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .expect("try exclusive after drop"),
        "drop must discard the pinned connection rather than leak its lock"
    );
    assert!(
        release_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .expect("release after drop")
    );
}

#[tokio::test]
async fn pinned_reader_read_error_discards_its_pinned_connection() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-pin-read-error").await;
    let (generation_id, artifact_id) = reserved_publish_with_artifact(
        &repo,
        "p-pin-read-error",
        "pin-read-error-commit",
        b"pin-read-error-blob",
    )
    .await;
    let key = generation_stream_pin_key(&generation_id).expect("pin key");

    // Deliberately make the row disagree with its immutable manifest. The
    // reader must reject it and discard, rather than return, its session.
    sqlx::query(
        "UPDATE repo_graph_galaxy_chunk SET sha256 = $1 \
             WHERE generation_id = $2::uuid AND artifact_id = $3::uuid AND chunk_index = 0",
    )
    .bind(sha256_hex(b"different chunk hash"))
    .bind(&generation_id)
    .bind(&artifact_id)
    .execute(db.pool())
    .await
    .expect("corrupt chunk hash for reader test");

    let mut reader = match repo
        .pin_current_galaxy_artifact("p-pin-read-error")
        .await
        .expect("select pinned artifact")
    {
        PinnedGalaxyArtifactSelection::Pinned(reader) => reader,
        other => panic!("expected pinned artifact, got {other:?}"),
    };
    assert!(
        reader.read_chunk(0).await.is_err(),
        "corrupt chunk must fail"
    );
    drop(reader);

    let mut contender = db.pool().acquire().await.expect("contender connection");
    assert!(
        try_acquire_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .expect("try exclusive after read error"),
        "a read error must not return a session holding the shared pin"
    );
    assert!(
        release_generation_stream_pin_exclusive(&mut contender, key)
            .await
            .expect("release exclusive")
    );
}

#[tokio::test]
async fn pinned_reader_reports_persisted_unsupported_wire_format() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-unsupported-wire").await;
    let (generation_id, _) = reserved_publish_with_artifact(
        &repo,
        "p-unsupported-wire",
        "unsupported-wire-commit",
        b"unsupported-wire-blob",
    )
    .await;
    sqlx::query(
        "UPDATE repo_graph_galaxy_artifact SET artifact_version = $1 \
             WHERE generation_id = $2::uuid",
    )
    .bind((SUPPORTED_GALAXY_ARTIFACT_VERSION + 1) as i32)
    .bind(&generation_id)
    .execute(db.pool())
    .await
    .expect("set unsupported artifact version");

    match repo
        .pin_current_galaxy_artifact("p-unsupported-wire")
        .await
        .expect("select unsupported artifact")
    {
        PinnedGalaxyArtifactSelection::UnsupportedVersion { version, encoding } => {
            assert_eq!(version, SUPPORTED_GALAXY_ARTIFACT_VERSION + 1);
            assert_eq!(&*encoding, SUPPORTED_GALAXY_ARTIFACT_ENCODING);
        }
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[tokio::test]
async fn galaxy_chunk_returns_exactly_one_by_index() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-chunk").await;
    let (gen_id, art_id) =
        reserved_publish_with_artifact(&repo, "p-chunk", "chunk-commit", b"chunk-blob").await;

    // Requested chunk index exists and carries the published bytes.
    let chunk = repo
        .galaxy_chunk(&gen_id, &art_id, 0)
        .await
        .expect("read chunk 0")
        .expect("chunk 0 exists");
    assert_eq!(chunk.chunk_index, 0);
    assert_eq!(chunk.bytes, b"chunk-blob");
    assert_eq!(chunk.byte_count, b"chunk-blob".len() as i32);

    // An out-of-range index returns None — no aggregation of all chunks.
    let miss = repo
        .galaxy_chunk(&gen_id, &art_id, 1)
        .await
        .expect("read chunk 1");
    assert!(miss.is_none(), "chunk index 1 should not exist");
}

#[tokio::test]
async fn generation_by_id_round_trips() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-byid").await;
    legacy_publish(&db, "p-byid", "byid-commit", b"byid-blob").await;

    let current = repo
        .current_generation_for_project("p-byid")
        .await
        .expect("current")
        .expect("generation exists");

    let by_id = repo
        .generation_by_id(&current.generation_id)
        .await
        .expect("by id")
        .expect("generation exists");
    assert_eq!(by_id.generation_id, current.generation_id);
    assert_eq!(by_id.commit_sha, "byid-commit");
    assert_eq!(by_id.graph_blob, b"byid-blob");
}

fn reserved_two_chunk_publication(project_id: &str, commit_sha: &str) -> ReservedGraphPublication {
    let generation_id = uuid::Uuid::now_v7().to_string();
    let artifact_id = uuid::Uuid::now_v7().to_string();
    let first = b"first".to_vec();
    let second = b"second".to_vec();
    let first_hash = sha256_hex(&first);
    let second_hash = sha256_hex(&second);
    ReservedGraphPublication {
        project_id: project_id.to_owned(),
        commit_sha: commit_sha.to_owned(),
        generation_id: generation_id.clone(),
        graph_blob: b"complete graph".to_vec(),
        artifact: ReservedGalaxyArtifactManifest {
            artifact_id: artifact_id.clone(),
            generation_id: generation_id.clone(),
            graph_content_hash: sha256_hex(b"semantic graph domain"),
            transport_sha256: sha256_hex(&[first.clone(), second.clone()].concat()),
            chunk_count: 2,
            byte_count: (first.len() + second.len()) as i64,
            chunk_hashes: vec![first_hash.clone(), second_hash.clone()],
        },
        chunks: vec![
            ReservedGalaxyArtifactChunk {
                generation_id: generation_id.clone(),
                artifact_id: artifact_id.clone(),
                chunk_index: 0,
                sha256: first_hash,
                bytes: first,
            },
            ReservedGalaxyArtifactChunk {
                generation_id,
                artifact_id,
                chunk_index: 1,
                sha256: second_hash,
                bytes: second,
            },
        ],
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PublicationSnapshot {
    cache: i64,
    generations: i64,
    current: Option<String>,
    artifacts: i64,
    chunks: i64,
    clock: Option<String>,
}

async fn publication_snapshot(db: &Database, project_id: &str) -> PublicationSnapshot {
    PublicationSnapshot {
            cache: sqlx::query_scalar("SELECT count(*) FROM repo_graph_cache WHERE project_id = $1").bind(project_id).fetch_one(db.pool()).await.expect("cache"),
            generations: sqlx::query_scalar("SELECT count(*) FROM repo_graph_generation WHERE project_id = $1").bind(project_id).fetch_one(db.pool()).await.expect("generations"),
            current: sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_current WHERE project_id = $1").bind(project_id).fetch_optional(db.pool()).await.expect("current"),
            artifacts: sqlx::query_scalar("SELECT count(*) FROM repo_graph_galaxy_artifact a JOIN repo_graph_generation g ON g.generation_id = a.generation_id WHERE g.project_id = $1").bind(project_id).fetch_one(db.pool()).await.expect("artifacts"),
            chunks: sqlx::query_scalar("SELECT count(*) FROM repo_graph_galaxy_chunk c JOIN repo_graph_generation g ON g.generation_id = c.generation_id WHERE g.project_id = $1").bind(project_id).fetch_one(db.pool()).await.expect("chunks"),
            clock: sqlx::query_scalar("SELECT last_built_at::text FROM repo_graph_publish_clock WHERE project_id = $1").bind(project_id).fetch_optional(db.pool()).await.expect("clock"),
        }
}

#[tokio::test]
async fn reserved_publication_persists_complete_write_set_under_reserved_identity() {
    let (db, repo) = fresh().await;
    insert_project(&db, "p-complete").await;
    let publication = reserved_two_chunk_publication("p-complete", "complete-commit");
    let generation_id = publication.generation_id.clone();
    let artifact_id = publication.artifact.artifact_id.clone();
    repo.publish_reserved_generation(publication)
        .await
        .expect("publish");
    let cache_id: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_cache WHERE project_id = $1",
    )
    .bind("p-complete")
    .fetch_one(db.pool())
    .await
    .expect("cache");
    let current_id: String = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_current WHERE project_id = $1",
    )
    .bind("p-complete")
    .fetch_one(db.pool())
    .await
    .expect("current");
    let generation = repo
        .generation_by_id(&generation_id)
        .await
        .expect("generation")
        .expect("immutable generation");
    let artifact: RepoGraphGalaxyArtifact = sqlx::query_as("SELECT artifact_id::text AS artifact_id, generation_id::text AS generation_id, graph_content_hash, transport_sha256, artifact_version, encoding, chunk_count, byte_count, chunk_hashes::text AS chunk_hashes FROM repo_graph_galaxy_artifact WHERE artifact_id = $1::uuid").bind(&artifact_id).fetch_one(db.pool()).await.expect("artifact");
    let chunks: Vec<RepoGraphGalaxyChunk> = sqlx::query_as("SELECT generation_id::text AS generation_id, artifact_id::text AS artifact_id, chunk_index, byte_count, sha256, bytes FROM repo_graph_galaxy_chunk WHERE generation_id = $1::uuid ORDER BY chunk_index").bind(&generation_id).fetch_all(db.pool()).await.expect("chunks");
    assert_eq!(cache_id, generation_id);
    assert_eq!(generation.generation_id, generation_id);
    assert_eq!(current_id, generation_id);
    assert_eq!(artifact.generation_id, generation_id);
    assert_eq!(artifact.artifact_id, artifact_id);
    assert_eq!(chunks.len(), 2);
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.generation_id, generation_id);
        assert_eq!(chunk.artifact_id, artifact_id);
        assert_eq!(chunk.chunk_index, index as i32);
    }
}

#[tokio::test]
async fn injected_reserved_publication_failures_rollback_every_write_stage() {
    for stage in [
        ReservedPublicationFailureStage::CompatibilityUpsert,
        ReservedPublicationFailureStage::ArtifactInsert,
        ReservedPublicationFailureStage::FirstChunkInsert,
    ] {
        let (db, repo) = fresh().await;
        insert_project(&db, "p-rollback").await;
        legacy_publish(&db, "p-rollback", "old", b"old graph").await;
        let before = publication_snapshot(&db, "p-rollback").await;
        assert!(
            repo.publish_reserved_generation_with_failure(
                reserved_two_chunk_publication("p-rollback", "new"),
                stage
            )
            .await
            .is_err()
        );
        assert_eq!(
            publication_snapshot(&db, "p-rollback").await,
            before,
            "stage {stage:?} leaked a transaction write"
        );
    }
}
