//! Immutable repo-graph generation and galaxy artifact persistence.
//!
//! This module is deliberately separate from `repo_graph_cache`: that module
//! keeps the historical `(project_id, commit_sha)` cache surface intact while
//! this one exposes the additive publication model introduced by migration 125.

use crate::database::Database;
use crate::repositories::repo_graph_cache::CachedRepoGraph;
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoGraphGeneration {
    pub generation_id: String,
    pub project_id: String,
    pub commit_sha: String,
    pub graph_blob: Vec<u8>,
    pub built_at: String,
    pub publish_seq: i64,
    pub artifact_required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoGraphGalaxyArtifact {
    pub artifact_id: String,
    pub generation_id: String,
    /// Hash over the untransported graph content domain.
    pub graph_content_hash: String,
    /// SHA-256 over the served transport representation.
    pub transport_sha256: String,
    pub chunk_count: i32,
    pub byte_count: i64,
    /// JSON manifest of ordered chunk SHA-256 values.
    pub chunk_hashes: String,
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct RepoGraphGalaxyChunk {
    pub generation_id: String,
    pub artifact_id: String,
    pub chunk_index: i32,
    pub byte_count: i32,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoGraphGalaxyArtifactInsert<'a> {
    pub artifact_id: &'a str,
    pub generation_id: &'a str,
    pub graph_content_hash: &'a str,
    pub transport_sha256: &'a str,
    pub chunk_count: i32,
    pub byte_count: i64,
    /// A JSON array containing the SHA-256 for each chunk, in chunk order.
    pub chunk_hashes: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoGraphGalaxyChunkInsert<'a> {
    pub generation_id: &'a str,
    pub artifact_id: &'a str,
    pub chunk_index: i32,
    pub sha256: &'a str,
    pub bytes: &'a [u8],
}

/// Owned chunk data for a caller-built galaxy artifact.
///
/// Keeping identities on every chunk detects accidentally mixed spools before
/// the publication transaction is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedGalaxyArtifactChunk {
    pub generation_id: String,
    pub artifact_id: String,
    pub chunk_index: i32,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

/// Owned manifest for a caller-built galaxy artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedGalaxyArtifactManifest {
    pub artifact_id: String,
    pub generation_id: String,
    pub graph_content_hash: String,
    pub transport_sha256: String,
    pub chunk_count: i32,
    pub byte_count: i64,
    /// SHA-256 values in exactly the same order as the chunks.
    pub chunk_hashes: Vec<String>,
}

/// All data required to atomically publish one caller-reserved generation.
///
/// This DB-owned type deliberately keeps `djinn-db` independent of the graph
/// producer crate, which builds the artifact before calling this API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedGraphPublication {
    pub project_id: String,
    pub commit_sha: String,
    pub generation_id: String,
    pub graph_blob: Vec<u8>,
    pub artifact: ReservedGalaxyArtifactManifest,
    pub chunks: Vec<ReservedGalaxyArtifactChunk>,
}

const MAX_GALAXY_CHUNK_BYTES: usize = 256 * 1024;

/// Private stage selector used only by the test-only publication entry point.
/// Keeping the transaction body shared ensures rollback assertions exercise the
/// same production write ordering.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservedPublicationFailureStage {
    AfterCompatibilityUpsert,
    AfterArtifactInsert,
    AfterFirstChunkInsert,
}

fn invalid_publication(message: impl Into<String>) -> Error {
    Error::InvalidData(format!(
        "invalid reserved graph publication: {}",
        message.into()
    ))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn validate_reserved_publication(publication: &ReservedGraphPublication) -> Result<()> {
    let artifact = &publication.artifact;
    if artifact.generation_id != publication.generation_id {
        return Err(invalid_publication(
            "artifact generation_id does not match publication",
        ));
    }
    if artifact.graph_content_hash == artifact.transport_sha256 {
        return Err(invalid_publication(
            "graph and transport hashes must be distinct",
        ));
    }
    if artifact.chunk_count < 0 || artifact.byte_count < 0 {
        return Err(invalid_publication(
            "manifest count and byte count must be nonnegative",
        ));
    }
    if artifact.chunk_count as usize != publication.chunks.len()
        || artifact.chunk_hashes.len() != publication.chunks.len()
    {
        return Err(invalid_publication(
            "manifest chunk count does not match chunks",
        ));
    }

    let mut byte_count = 0_i64;
    let mut transport = Sha256::new();
    for (expected_index, chunk) in publication.chunks.iter().enumerate() {
        if chunk.generation_id != publication.generation_id
            || chunk.artifact_id != artifact.artifact_id
        {
            return Err(invalid_publication(
                "chunk identity does not match manifest",
            ));
        }
        if chunk.chunk_index != expected_index as i32 {
            return Err(invalid_publication(
                "chunk indexes must be contiguous and zero-based",
            ));
        }
        if chunk.bytes.len() > MAX_GALAXY_CHUNK_BYTES {
            return Err(invalid_publication("chunk exceeds 256 KiB"));
        }
        let expected_hash = &artifact.chunk_hashes[expected_index];
        if &chunk.sha256 != expected_hash {
            return Err(invalid_publication(
                "chunk hash does not match ordered manifest",
            ));
        }
        if sha256_hex(&chunk.bytes) != chunk.sha256 {
            return Err(invalid_publication("chunk hash does not match chunk bytes"));
        }
        byte_count = byte_count
            .checked_add(chunk.bytes.len() as i64)
            .ok_or_else(|| invalid_publication("chunk byte total overflowed"))?;
        transport.update(&chunk.bytes);
    }
    if byte_count != artifact.byte_count {
        return Err(invalid_publication(
            "manifest byte count does not match chunks",
        ));
    }
    if format!("{:x}", transport.finalize()) != artifact.transport_sha256 {
        return Err(invalid_publication(
            "transport digest does not match chunks",
        ));
    }
    Ok(())
}

/// Result of selecting a graph for a project.
///
/// `LegacyFallback` is only returned when `repo_graph_current` has no pointer;
/// an existing pointer is never allowed to fall back to an older cache row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectCurrentGraph {
    Current(RepoGraphGeneration),
    LegacyFallback(CachedRepoGraph),
    Unavailable,
}

/// Result of selecting the galaxy artifact supported by the current pointer.
///
/// In particular, `ArtifactUnavailable` means a current generation exists but
/// has no artifact. It is intentionally distinct from `NoCurrentGeneration`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CurrentGalaxyArtifact {
    Available {
        generation: RepoGraphGeneration,
        artifact: RepoGraphGalaxyArtifact,
    },
    ArtifactUnavailable {
        generation: RepoGraphGeneration,
    },
    NoCurrentGeneration,
}

pub struct RepoGraphGenerationRepository {
    db: Database,
}

impl RepoGraphGenerationRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Publish compatibility data and a complete artifact under one reserved
    /// UUIDv7 generation. No collision or marker failure is retried here.
    pub async fn publish_reserved_generation(
        &self,
        publication: ReservedGraphPublication,
    ) -> Result<()> {
        self.publish_reserved_generation_inner(publication, None).await
    }

    /// Test-only failure seam for verifying every partial write rolls back as
    /// one transaction. Production callers use `publish_reserved_generation`.
    #[cfg(test)]
    async fn publish_reserved_generation_with_failure(
        &self,
        publication: ReservedGraphPublication,
        failure_stage: ReservedPublicationFailureStage,
    ) -> Result<()> {
        self.publish_reserved_generation_inner(publication, Some(failure_stage))
            .await
    }

    async fn publish_reserved_generation_inner(
        &self,
        publication: ReservedGraphPublication,
        failure_stage: Option<ReservedPublicationFailureStage>,
    ) -> Result<()> {
        validate_reserved_publication(&publication)?;

        let mut tx = self
            .begin_reserved_publication(&publication.project_id, &publication.generation_id)
            .await?;
        Self::reserved_compatibility_upsert_in_transaction(
            &mut tx,
            &publication.project_id,
            &publication.commit_sha,
            &publication.graph_blob,
            &publication.generation_id,
        )
        .await?;

        if failure_stage == Some(ReservedPublicationFailureStage::AfterCompatibilityUpsert) {
            return Err(invalid_publication("injected failure after compatibility upsert"));
        }

        let chunk_hashes = serde_json::to_string(&publication.artifact.chunk_hashes)?;
        Self::insert_galaxy_artifact_in_transaction(
            &mut tx,
            RepoGraphGalaxyArtifactInsert {
                artifact_id: &publication.artifact.artifact_id,
                generation_id: &publication.artifact.generation_id,
                graph_content_hash: &publication.artifact.graph_content_hash,
                transport_sha256: &publication.artifact.transport_sha256,
                chunk_count: publication.artifact.chunk_count,
                byte_count: publication.artifact.byte_count,
                chunk_hashes: &chunk_hashes,
            },
        )
        .await?;
        if failure_stage == Some(ReservedPublicationFailureStage::AfterArtifactInsert) {
            return Err(invalid_publication("injected failure after artifact insertion"));
        }
        for (chunk_position, chunk) in publication.chunks.iter().enumerate() {
            Self::insert_galaxy_chunk_in_transaction(
                &mut tx,
                RepoGraphGalaxyChunkInsert {
                    generation_id: &chunk.generation_id,
                    artifact_id: &chunk.artifact_id,
                    chunk_index: chunk.chunk_index,
                    sha256: &chunk.sha256,
                    bytes: &chunk.bytes,
                },
            )
            .await?;
            if chunk_position == 0
                && failure_stage == Some(ReservedPublicationFailureStage::AfterFirstChunkInsert)
            {
                return Err(invalid_publication("injected failure after partial chunk insertion"));
            }
        }
        // Deferred migration triggers validate and advance current here.
        tx.commit().await?;
        Ok(())
    }

    /// Start a caller-owned publication transaction and reserve its UUIDv7.
    /// The transaction is returned uncommitted so metadata and chunks can be
    /// inserted before the migration's deferred validation advances current.
    pub async fn begin_reserved_publication<'a>(
        &'a self,
        project_id: &str,
        generation_id: &str,
    ) -> Result<Transaction<'a, Postgres>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        Self::reserve_generation_in_transaction(&mut tx, project_id, generation_id).await?;
        Ok(tx)
    }

    /// Set the transaction-local reservation marker enforced by the migration.
    pub async fn reserve_generation_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        project_id: &str,
        generation_id: &str,
    ) -> Result<()> {
        sqlx::query("SELECT repo_graph_reserve_generation($1, $2::uuid)")
            .bind(project_id)
            .bind(generation_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Compatibility upsert using the generation reserved in this transaction.
    /// This intentionally mirrors the legacy cache SQL's exact conflict key.
    pub async fn reserved_compatibility_upsert_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        project_id: &str,
        commit_sha: &str,
        graph_blob: &[u8],
        generation_id: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO repo_graph_cache (project_id, commit_sha, graph_blob, generation_id) \
             VALUES ($1, $2, $3, $4::uuid) \
             ON CONFLICT (project_id, commit_sha) DO UPDATE SET \
                 graph_blob = EXCLUDED.graph_blob, generation_id = EXCLUDED.generation_id",
        )
        .bind(project_id)
        .bind(commit_sha)
        .bind(graph_blob)
        .bind(generation_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn insert_galaxy_artifact_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        artifact: RepoGraphGalaxyArtifactInsert<'_>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO repo_graph_galaxy_artifact \
             (artifact_id, generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes) \
             VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, $7::jsonb)",
        )
        .bind(artifact.artifact_id)
        .bind(artifact.generation_id)
        .bind(artifact.graph_content_hash)
        .bind(artifact.transport_sha256)
        .bind(artifact.chunk_count)
        .bind(artifact.byte_count)
        .bind(artifact.chunk_hashes)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn insert_galaxy_chunk_in_transaction(
        tx: &mut Transaction<'_, Postgres>,
        chunk: RepoGraphGalaxyChunkInsert<'_>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO repo_graph_galaxy_chunk \
             (generation_id, artifact_id, chunk_index, byte_count, sha256, bytes) \
             VALUES ($1::uuid, $2::uuid, $3, octet_length($4), $5, $4)",
        )
        .bind(chunk.generation_id)
        .bind(chunk.artifact_id)
        .bind(chunk.chunk_index)
        .bind(chunk.bytes)
        .bind(chunk.sha256)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn generation_by_id(
        &self,
        generation_id: &str,
    ) -> Result<Option<RepoGraphGeneration>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, RepoGraphGeneration>(
            "SELECT generation_id::text AS generation_id, project_id, commit_sha, graph_blob, \
                    built_at, publish_seq, artifact_required \
             FROM repo_graph_generation WHERE generation_id = $1::uuid",
        )
        .bind(generation_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn latest_for_project_commit(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> Result<Option<RepoGraphGeneration>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, RepoGraphGeneration>(
            "SELECT generation_id::text AS generation_id, project_id, commit_sha, graph_blob, \
                    built_at, publish_seq, artifact_required \
             FROM repo_graph_generation \
             WHERE project_id = $1 AND commit_sha = $2 ORDER BY publish_seq DESC LIMIT 1",
        )
        .bind(project_id)
        .bind(commit_sha)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn greatest_publish_seq_for_project_commit(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> Result<Option<i64>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar("SELECT max(publish_seq) FROM repo_graph_generation WHERE project_id = $1 AND commit_sha = $2")
            .bind(project_id)
            .bind(commit_sha)
            .fetch_one(self.db.pool())
            .await?)
    }

    pub async fn current_generation_for_project(
        &self,
        project_id: &str,
    ) -> Result<Option<RepoGraphGeneration>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, RepoGraphGeneration>(
            "SELECT g.generation_id::text AS generation_id, g.project_id, g.commit_sha, g.graph_blob, \
                    g.built_at, g.publish_seq, g.artifact_required \
             FROM repo_graph_generation g \
             JOIN repo_graph_current c ON c.generation_id = g.generation_id \
             WHERE c.project_id = $1",
        )
            .bind(project_id)
            .fetch_optional(self.db.pool())
            .await?)
    }

    /// Select through `repo_graph_current`, using legacy timestamp order only
    /// when no pointer exists (pre-backfill compatibility).
    pub async fn select_project_current_graph(
        &self,
        project_id: &str,
    ) -> Result<ProjectCurrentGraph> {
        if let Some(generation) = self.current_generation_for_project(project_id).await? {
            return Ok(ProjectCurrentGraph::Current(generation));
        }
        Ok(match self.pre_backfill_legacy_fallback(project_id).await? {
            Some(graph) => ProjectCurrentGraph::LegacyFallback(graph),
            None => ProjectCurrentGraph::Unavailable,
        })
    }

    /// Explicitly named compatibility fallback; callers normally want
    /// `select_project_current_graph` instead.
    pub async fn pre_backfill_legacy_fallback(
        &self,
        project_id: &str,
    ) -> Result<Option<CachedRepoGraph>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, CachedRepoGraph>(
            "SELECT project_id, commit_sha, graph_blob, built_at FROM repo_graph_cache \
             WHERE project_id = $1 ORDER BY built_at DESC LIMIT 1",
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn current_galaxy_artifact_for_project(
        &self,
        project_id: &str,
    ) -> Result<CurrentGalaxyArtifact> {
        let Some(generation) = self.current_generation_for_project(project_id).await? else {
            return Ok(CurrentGalaxyArtifact::NoCurrentGeneration);
        };
        let artifact = sqlx::query_as::<_, RepoGraphGalaxyArtifact>(
            "SELECT artifact_id::text AS artifact_id, generation_id::text AS generation_id, \
                    graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes::text AS chunk_hashes \
             FROM repo_graph_galaxy_artifact WHERE generation_id = $1::uuid",
        )
        .bind(&generation.generation_id)
        .fetch_optional(self.db.pool())
        .await?;
        Ok(match artifact {
            Some(artifact) => CurrentGalaxyArtifact::Available {
                generation,
                artifact,
            },
            None => CurrentGalaxyArtifact::ArtifactUnavailable { generation },
        })
    }

    /// Return exactly one chunk for a known artifact and ordered chunk index.
    pub async fn galaxy_chunk(
        &self,
        generation_id: &str,
        artifact_id: &str,
        chunk_index: i32,
    ) -> Result<Option<RepoGraphGalaxyChunk>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, RepoGraphGalaxyChunk>(
            "SELECT generation_id::text AS generation_id, artifact_id::text AS artifact_id, \
                    chunk_index, byte_count, sha256, bytes \
             FROM repo_graph_galaxy_chunk \
             WHERE generation_id = $1::uuid AND artifact_id = $2::uuid AND chunk_index = $3 \
             ORDER BY chunk_index ASC LIMIT 1",
        )
        .bind(generation_id)
        .bind(artifact_id)
        .bind(chunk_index)
        .fetch_optional(self.db.pool())
        .await?)
    }
}

#[cfg(test)]
mod tests {
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
                graph_content_hash: "graph_content_hash_domain_value".to_owned(),
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
            project_id: project_id.to_owned(), commit_sha: commit_sha.to_owned(), generation_id: generation_id.clone(), graph_blob: b"complete graph".to_vec(),
            artifact: ReservedGalaxyArtifactManifest { artifact_id: artifact_id.clone(), generation_id: generation_id.clone(), graph_content_hash: "semantic-graph-hash".to_owned(), transport_sha256: sha256_hex(&[first.clone(), second.clone()].concat()), chunk_count: 2, byte_count: (first.len() + second.len()) as i64, chunk_hashes: vec![first_hash.clone(), second_hash.clone()] },
            chunks: vec![
                ReservedGalaxyArtifactChunk { generation_id: generation_id.clone(), artifact_id: artifact_id.clone(), chunk_index: 0, sha256: first_hash, bytes: first },
                ReservedGalaxyArtifactChunk { generation_id, artifact_id, chunk_index: 1, sha256: second_hash, bytes: second },
            ],
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct PublicationSnapshot { cache: i64, generations: i64, current: Option<String>, artifacts: i64, chunks: i64, clock: Option<String> }

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
        let generation_id = publication.generation_id.clone(); let artifact_id = publication.artifact.artifact_id.clone();
        repo.publish_reserved_generation(publication).await.expect("publish");
        let cache_id: String = sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_cache WHERE project_id = $1").bind("p-complete").fetch_one(db.pool()).await.expect("cache");
        let current_id: String = sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_current WHERE project_id = $1").bind("p-complete").fetch_one(db.pool()).await.expect("current");
        let generation = repo.generation_by_id(&generation_id).await.expect("generation").expect("immutable generation");
        let artifact: RepoGraphGalaxyArtifact = sqlx::query_as("SELECT artifact_id::text AS artifact_id, generation_id::text AS generation_id, graph_content_hash, transport_sha256, chunk_count, byte_count, chunk_hashes::text AS chunk_hashes FROM repo_graph_galaxy_artifact WHERE artifact_id = $1::uuid").bind(&artifact_id).fetch_one(db.pool()).await.expect("artifact");
        let chunks: Vec<RepoGraphGalaxyChunk> = sqlx::query_as("SELECT generation_id::text AS generation_id, artifact_id::text AS artifact_id, chunk_index, byte_count, sha256, bytes FROM repo_graph_galaxy_chunk WHERE generation_id = $1::uuid ORDER BY chunk_index").bind(&generation_id).fetch_all(db.pool()).await.expect("chunks");
        assert_eq!(cache_id, generation_id); assert_eq!(generation.generation_id, generation_id); assert_eq!(current_id, generation_id); assert_eq!(artifact.generation_id, generation_id); assert_eq!(artifact.artifact_id, artifact_id); assert_eq!(chunks.len(), 2);
        for (index, chunk) in chunks.iter().enumerate() { assert_eq!(chunk.generation_id, generation_id); assert_eq!(chunk.artifact_id, artifact_id); assert_eq!(chunk.chunk_index, index as i32); }
    }

    #[tokio::test]
    async fn injected_reserved_publication_failures_rollback_every_write_stage() {
        for stage in [ReservedPublicationFailureStage::AfterCompatibilityUpsert, ReservedPublicationFailureStage::AfterArtifactInsert, ReservedPublicationFailureStage::AfterFirstChunkInsert] {
            let (db, repo) = fresh().await; insert_project(&db, "p-rollback").await; legacy_publish(&db, "p-rollback", "old", b"old graph").await;
            let before = publication_snapshot(&db, "p-rollback").await;
            assert!(repo.publish_reserved_generation_with_failure(reserved_two_chunk_publication("p-rollback", "new"), stage).await.is_err());
            assert_eq!(publication_snapshot(&db, "p-rollback").await, before, "stage {stage:?} leaked a transaction write");
        }
    }
}
