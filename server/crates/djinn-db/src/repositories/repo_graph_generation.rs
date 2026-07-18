//! Immutable repo-graph generation and galaxy artifact persistence.
//!
//! This module is deliberately separate from `repo_graph_cache`: that module
//! keeps the historical `(project_id, commit_sha)` cache surface intact while
//! this one exposes the additive publication model introduced by migration 125.

use crate::Result;
use crate::database::Database;
use crate::repositories::repo_graph_cache::CachedRepoGraph;
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
