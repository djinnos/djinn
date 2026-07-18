//! Immutable repo-graph generation and galaxy artifact persistence.
//!
//! This module is deliberately separate from `repo_graph_cache`: that module
//! keeps the historical `(project_id, commit_sha)` cache surface intact while
//! this one exposes the additive publication model introduced by migration 125.

use crate::database::Database;
use crate::repositories::repo_graph_cache::CachedRepoGraph;
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use sqlx::{Acquire, Postgres, Transaction, pool::PoolConnection};

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
    /// Version of the persisted galaxy wire artifact.
    pub artifact_version: i32,
    /// Content encoding of the persisted galaxy wire artifact.
    pub encoding: String,
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

/// The largest chunk the stream reader will ever materialize at once.
pub const MAX_GALAXY_CHUNK_BYTES: usize = 256 * 1024;
/// Bound the manifest retained by a pinned reader before response headers form.
pub const MAX_GALAXY_ARTIFACT_CHUNKS: usize = 4_096;
/// Bound metadata-advertised stream size before response headers form.
pub const MAX_GALAXY_ARTIFACT_BYTES: i64 = 1_073_741_824;
/// The only artifact wire version understood by this schema generation.
pub const SUPPORTED_GALAXY_ARTIFACT_VERSION: u32 = 1;
/// The canonical transport encoding produced by the galaxy publisher.
pub const SUPPORTED_GALAXY_ARTIFACT_ENCODING: &str = "gzip";

/// Namespace for the two-int PostgreSQL advisory key used to pin a generation.
/// Both readers and retention must use [`generation_stream_pin_key`].
pub const GENERATION_STREAM_PIN_LOCK_CLASS: i32 = 0x4741_4c58;

/// Canonical PostgreSQL advisory key for a generation stream pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenerationStreamPinKey {
    pub class_id: i32,
    pub object_id: i32,
}

/// Derive the one shared/exclusive advisory-lock identity for a generation.
pub fn generation_stream_pin_key(generation_id: &str) -> Result<GenerationStreamPinKey> {
    let generation = uuid::Uuid::parse_str(generation_id)
        .map_err(|_| Error::InvalidData("invalid galaxy generation UUID".to_owned()))?;
    if generation.to_string() != generation_id {
        return Err(Error::InvalidData(
            "galaxy generation UUID is not canonical".to_owned(),
        ));
    }
    let mut hasher = Sha256::new();
    hasher.update(b"djinn:galaxy-stream-pin:v1\0");
    hasher.update(generation.as_bytes());
    let digest = hasher.finalize();
    Ok(GenerationStreamPinKey {
        class_id: GENERATION_STREAM_PIN_LOCK_CLASS,
        object_id: i32::from_be_bytes(digest[..4].try_into().expect("SHA-256 prefix")),
    })
}

/// Acquire the reader side of the canonical generation stream-pin protocol.
pub async fn acquire_generation_stream_pin_shared(
    conn: &mut sqlx::postgres::PgConnection,
    key: GenerationStreamPinKey,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_lock_shared($1, $2)")
        .bind(key.class_id)
        .bind(key.object_id)
        .execute(conn)
        .await
        .map(|_| ())
}

/// Try the retention side of the canonical generation stream-pin protocol.
pub async fn try_acquire_generation_stream_pin_exclusive(
    conn: &mut sqlx::postgres::PgConnection,
    key: GenerationStreamPinKey,
) -> std::result::Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
        .bind(key.class_id)
        .bind(key.object_id)
        .fetch_one(conn)
        .await
}

/// Release the reader side of the canonical generation stream-pin protocol.
///
/// PostgreSQL tracks shared and exclusive advisory locks separately. Callers
/// must treat `false` as a protocol error: it means this session did not hold
/// the shared lock being released.
pub async fn release_generation_stream_pin_shared(
    conn: &mut sqlx::postgres::PgConnection,
    key: GenerationStreamPinKey,
) -> std::result::Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_advisory_unlock_shared($1, $2)")
        .bind(key.class_id)
        .bind(key.object_id)
        .fetch_one(conn)
        .await
}

/// Release the retention side of the canonical generation stream-pin protocol.
pub async fn release_generation_stream_pin_exclusive(
    conn: &mut sqlx::postgres::PgConnection,
    key: GenerationStreamPinKey,
) -> std::result::Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT pg_advisory_unlock($1, $2)")
        .bind(key.class_id)
        .bind(key.object_id)
        .fetch_one(conn)
        .await
}

/// Stage selector used by integration tests to prove partial publications roll
/// back through the same transaction body as production publication.
/// Keeping the transaction body shared ensures rollback assertions exercise the
/// same production write ordering.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservedPublicationFailureStage {
    CompatibilityUpsert,
    ArtifactInsert,
    FirstChunkInsert,
    Commit,
}

// Kept private and always compiled so the production transaction body does not
// expose a cfg-gated test type in its signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationFailureStage {
    CompatibilityUpsert,
    ArtifactInsert,
    FirstChunkInsert,
    Commit,
}

#[cfg(any(test, feature = "test-support"))]
impl From<ReservedPublicationFailureStage> for PublicationFailureStage {
    fn from(value: ReservedPublicationFailureStage) -> Self {
        match value {
            ReservedPublicationFailureStage::CompatibilityUpsert => Self::CompatibilityUpsert,
            ReservedPublicationFailureStage::ArtifactInsert => Self::ArtifactInsert,
            ReservedPublicationFailureStage::FirstChunkInsert => Self::FirstChunkInsert,
            ReservedPublicationFailureStage::Commit => Self::Commit,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicationTestSnapshot {
    pub cache: i64,
    pub generations: i64,
    pub current: Option<String>,
    pub artifacts: i64,
    pub chunks: i64,
    pub clock: Option<String>,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct LegacyLatestGraph {
    pub project_id: String,
    pub commit_sha: String,
    pub graph_blob: Vec<u8>,
    pub built_at: String,
    pub generation_id: String,
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

/// Header-safe, bounded artifact metadata retained while a stream is pinned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedGalaxyArtifactMetadata {
    pub project_id: String,
    pub generation_id: String,
    pub commit_sha: String,
    pub artifact_id: String,
    pub graph_content_hash: String,
    pub transport_sha256: String,
    pub artifact_version: u32,
    pub encoding: String,
    pub chunk_count: i32,
    pub byte_count: i64,
}

/// Selection outcome before a route has formed response headers.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum PinnedGalaxyArtifactSelection {
    NoCurrentGeneration,
    ArtifactUnavailable,
    UnsupportedVersion { version: u32, encoding: Box<str> },
    CorruptMetadata { reason: String },
    Pinned(PinnedGalaxyArtifact),
}

/// A session-pinned artifact reader. It deliberately retains no chunk payloads.
pub struct PinnedGalaxyArtifact {
    metadata: PinnedGalaxyArtifactMetadata,
    chunk_hashes: Vec<String>,
    pin_key: GenerationStreamPinKey,
    connection: Option<PoolConnection<Postgres>>,
}

impl std::fmt::Debug for PinnedGalaxyArtifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedGalaxyArtifact")
            .field("metadata", &self.metadata)
            .field("manifest_entries", &self.chunk_hashes.len())
            .finish_non_exhaustive()
    }
}

impl PinnedGalaxyArtifact {
    pub fn metadata(&self) -> &PinnedGalaxyArtifactMetadata {
        &self.metadata
    }

    /// Fetch and verify exactly one expected chunk using the pinned session.
    pub async fn read_chunk(&mut self, chunk_index: i32) -> Result<RepoGraphGalaxyChunk> {
        if chunk_index < 0 || chunk_index >= self.metadata.chunk_count {
            return Err(Error::InvalidData(
                "galaxy chunk index is out of range".to_owned(),
            ));
        }
        let conn = self.connection.as_mut().ok_or_else(|| {
            Error::InvalidData("galaxy artifact reader has already finished".to_owned())
        })?;
        let row = sqlx::query_as::<_, RepoGraphGalaxyChunk>(
            "SELECT generation_id::text AS generation_id, artifact_id::text AS artifact_id, \
                    chunk_index, byte_count, sha256, bytes \
             FROM repo_graph_galaxy_chunk \
             WHERE generation_id = $1::uuid AND artifact_id = $2::uuid AND chunk_index = $3",
        )
        .bind(&self.metadata.generation_id)
        .bind(&self.metadata.artifact_id)
        .bind(chunk_index)
        .fetch_optional(&mut **conn)
        .await;
        let chunk = match row {
            Ok(Some(chunk)) => chunk,
            Ok(None) => {
                return Err(Error::InvalidData(
                    "expected galaxy chunk is missing".to_owned(),
                ));
            }
            Err(error) => {
                conn.close_on_drop();
                return Err(error.into());
            }
        };
        let expected_hash = &self.chunk_hashes[chunk_index as usize];
        if chunk.generation_id != self.metadata.generation_id
            || chunk.artifact_id != self.metadata.artifact_id
            || chunk.chunk_index != chunk_index
            || chunk.byte_count < 0
            || chunk.byte_count as usize != chunk.bytes.len()
            || chunk.bytes.len() > MAX_GALAXY_CHUNK_BYTES
            || chunk.sha256 != *expected_hash
            || sha256_hex(&chunk.bytes) != chunk.sha256
        {
            conn.close_on_drop();
            return Err(Error::InvalidData(
                "corrupt galaxy chunk metadata or bytes".to_owned(),
            ));
        }
        Ok(chunk)
    }

    /// Explicitly release the session advisory pin before returning its connection.
    pub async fn finish(mut self) -> Result<()> {
        let mut conn = self
            .connection
            .take()
            .expect("finished reader has no connection");
        match release_generation_stream_pin_shared(&mut conn, self.pin_key).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                let _ = conn.close().await;
                Err(Error::InvalidData(
                    "galaxy shared stream pin was not held by its session".to_owned(),
                ))
            }
            Err(error) => {
                let _ = conn.close().await;
                Err(error.into())
            }
        }
    }
}

impl Drop for PinnedGalaxyArtifact {
    fn drop(&mut self) {
        // Drop cannot await pg_advisory_unlock. Never return a potentially
        // pinned session to the pool: SQLx closes it and PostgreSQL releases
        // all session advisory locks with the connection.
        if let Some(conn) = self.connection.as_mut() {
            conn.close_on_drop();
        }
    }
}

fn header_safe(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn sha256_hex_value(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_pinned_metadata(
    project_id: &str,
    generation: &RepoGraphGeneration,
    artifact: &RepoGraphGalaxyArtifact,
) -> std::result::Result<Vec<String>, String> {
    if !header_safe(project_id, 36) || !header_safe(&generation.commit_sha, 64) {
        return Err("project or commit is not header-safe".to_owned());
    }
    for value in [
        &generation.generation_id,
        &artifact.generation_id,
        &artifact.artifact_id,
    ] {
        if uuid::Uuid::parse_str(value).ok().map(|id| id.to_string()) != Some(value.clone()) {
            return Err("generation or artifact UUID is invalid".to_owned());
        }
    }
    if generation.project_id != project_id || artifact.generation_id != generation.generation_id {
        return Err("current pointer identities disagree".to_owned());
    }
    if artifact.chunk_count < 0
        || artifact.chunk_count as usize > MAX_GALAXY_ARTIFACT_CHUNKS
        || artifact.byte_count < 0
        || artifact.byte_count > MAX_GALAXY_ARTIFACT_BYTES
        || !sha256_hex_value(&artifact.graph_content_hash)
        || !sha256_hex_value(&artifact.transport_sha256)
    {
        return Err("artifact counts or hashes are invalid".to_owned());
    }
    let hashes: Vec<String> = serde_json::from_str(&artifact.chunk_hashes)
        .map_err(|_| "artifact manifest is not a JSON string array".to_owned())?;
    if hashes.len() != artifact.chunk_count as usize
        || hashes.iter().any(|hash| !sha256_hex_value(hash))
    {
        return Err("artifact manifest length or hash is invalid".to_owned());
    }
    Ok(hashes)
}

#[derive(sqlx::FromRow)]
struct PinnedSelectorGeneration {
    generation_id: String,
    project_id: String,
    commit_sha: String,
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
        self.publish_reserved_generation_inner(publication, None)
            .await
    }

    /// Test-only failure seam for verifying every partial write rolls back as
    /// one transaction. Production callers use `publish_reserved_generation`.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn publish_reserved_generation_with_failure(
        &self,
        publication: ReservedGraphPublication,
        failure_stage: ReservedPublicationFailureStage,
    ) -> Result<()> {
        self.publish_reserved_generation_inner(publication, Some(failure_stage.into()))
            .await
    }

    async fn publish_reserved_generation_inner(
        &self,
        publication: ReservedGraphPublication,
        failure_stage: Option<PublicationFailureStage>,
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

        if failure_stage == Some(PublicationFailureStage::CompatibilityUpsert) {
            return Err(invalid_publication(
                "injected failure after compatibility upsert",
            ));
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
        if failure_stage == Some(PublicationFailureStage::ArtifactInsert) {
            return Err(invalid_publication(
                "injected failure after artifact insertion",
            ));
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
                && failure_stage == Some(PublicationFailureStage::FirstChunkInsert)
            {
                return Err(invalid_publication(
                    "injected failure after partial chunk insertion",
                ));
            }
        }
        if failure_stage == Some(PublicationFailureStage::Commit) {
            return Err(invalid_publication("injected failure before commit"));
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
                    graph_content_hash, transport_sha256, artifact_version, encoding, chunk_count, byte_count, chunk_hashes::text AS chunk_hashes \
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

    /// Select metadata under `FOR SHARE`, then retain a shared session pin on
    /// the same checked-out connection for bounded one-chunk-at-a-time reads.
    pub async fn pin_current_galaxy_artifact(
        &self,
        project_id: &str,
    ) -> Result<PinnedGalaxyArtifactSelection> {
        self.db.ensure_initialized().await?;
        let mut conn = self.db.pool().acquire().await?;
        let mut tx = conn.begin().await?;
        let generation = sqlx::query_as::<_, PinnedSelectorGeneration>(
            "SELECT g.generation_id::text AS generation_id, g.project_id, g.commit_sha \
             FROM repo_graph_current c JOIN repo_graph_generation g ON g.generation_id = c.generation_id \
             WHERE c.project_id = $1 FOR SHARE OF c, g",
        ).bind(project_id).fetch_optional(&mut *tx).await?;
        let Some(generation) = generation else {
            tx.commit().await?;
            return Ok(PinnedGalaxyArtifactSelection::NoCurrentGeneration);
        };
        let artifact = sqlx::query_as::<_, RepoGraphGalaxyArtifact>(
            "SELECT artifact_id::text AS artifact_id, generation_id::text AS generation_id, \
                    graph_content_hash, transport_sha256, artifact_version, encoding, chunk_count, byte_count, chunk_hashes::text AS chunk_hashes \
             FROM repo_graph_galaxy_artifact WHERE generation_id = $1::uuid FOR SHARE",
        ).bind(&generation.generation_id).fetch_optional(&mut *tx).await?;
        let Some(artifact) = artifact else {
            tx.commit().await?;
            return Ok(PinnedGalaxyArtifactSelection::ArtifactUnavailable);
        };
        let version = u32::try_from(artifact.artifact_version).map_err(|_| {
            Error::InvalidData("galaxy artifact version does not fit u32".to_owned())
        })?;
        if version != SUPPORTED_GALAXY_ARTIFACT_VERSION
            || artifact.encoding != SUPPORTED_GALAXY_ARTIFACT_ENCODING
        {
            tx.commit().await?;
            return Ok(PinnedGalaxyArtifactSelection::UnsupportedVersion {
                version,
                encoding: artifact.encoding.into(),
            });
        }
        let synthetic = RepoGraphGeneration {
            generation_id: generation.generation_id.clone(),
            project_id: generation.project_id.clone(),
            commit_sha: generation.commit_sha.clone(),
            graph_blob: Vec::new(),
            built_at: String::new(),
            publish_seq: 0,
            artifact_required: true,
        };
        let hashes = match validate_pinned_metadata(project_id, &synthetic, &artifact) {
            Ok(hashes) => hashes,
            Err(reason) => {
                tx.commit().await?;
                return Ok(PinnedGalaxyArtifactSelection::CorruptMetadata { reason });
            }
        };
        let pin_key = generation_stream_pin_key(&generation.generation_id)?;
        acquire_generation_stream_pin_shared(&mut tx, pin_key).await?;
        if let Err(error) = tx.commit().await {
            conn.close_on_drop();
            return Err(error.into());
        }
        Ok(PinnedGalaxyArtifactSelection::Pinned(
            PinnedGalaxyArtifact {
                metadata: PinnedGalaxyArtifactMetadata {
                    project_id: generation.project_id,
                    generation_id: generation.generation_id,
                    commit_sha: generation.commit_sha,
                    artifact_id: artifact.artifact_id,
                    graph_content_hash: artifact.graph_content_hash,
                    transport_sha256: artifact.transport_sha256,
                    artifact_version: version,
                    encoding: artifact.encoding,
                    chunk_count: artifact.chunk_count,
                    byte_count: artifact.byte_count,
                },
                chunk_hashes: hashes,
                pin_key,
                connection: Some(conn),
            },
        ))
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

    #[cfg(any(test, feature = "test-support"))]
    pub async fn prepare_publication_test_project(&self, project_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(project_id)
            .execute(self.db.pool())
            .await?;
        sqlx::query("INSERT INTO projects(id, name, github_owner, github_repo) VALUES ($1, 'full warm publication regression', 'test-owner', 'test-repo')")
            .bind(project_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn compatibility_generation_id(
        &self,
        project_id: &str,
        commit_sha: &str,
    ) -> Result<String> {
        Ok(sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_cache WHERE project_id = $1 AND commit_sha = $2")
            .bind(project_id).bind(commit_sha).fetch_one(self.db.pool()).await?)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn galaxy_chunks_for_test(
        &self,
        generation_id: &str,
    ) -> Result<Vec<RepoGraphGalaxyChunk>> {
        Ok(sqlx::query_as("SELECT generation_id::text AS generation_id, artifact_id::text AS artifact_id, chunk_index, byte_count, sha256, bytes FROM repo_graph_galaxy_chunk WHERE generation_id = $1::uuid ORDER BY chunk_index")
            .bind(generation_id).fetch_all(self.db.pool()).await?)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn publication_snapshot_for_test(
        &self,
        project_id: &str,
    ) -> Result<PublicationTestSnapshot> {
        Ok(PublicationTestSnapshot {
            cache: sqlx::query_scalar("SELECT count(*) FROM repo_graph_cache WHERE project_id=$1").bind(project_id).fetch_one(self.db.pool()).await?,
            generations: sqlx::query_scalar("SELECT count(*) FROM repo_graph_generation WHERE project_id=$1").bind(project_id).fetch_one(self.db.pool()).await?,
            current: sqlx::query_scalar("SELECT generation_id::text FROM repo_graph_current WHERE project_id=$1").bind(project_id).fetch_optional(self.db.pool()).await?,
            artifacts: sqlx::query_scalar("SELECT count(*) FROM repo_graph_galaxy_artifact a JOIN repo_graph_generation g ON g.generation_id=a.generation_id WHERE g.project_id=$1").bind(project_id).fetch_one(self.db.pool()).await?,
            chunks: sqlx::query_scalar("SELECT count(*) FROM repo_graph_galaxy_chunk c JOIN repo_graph_generation g ON g.generation_id=c.generation_id WHERE g.project_id=$1").bind(project_id).fetch_one(self.db.pool()).await?,
            clock: sqlx::query_scalar("SELECT last_built_at::text FROM repo_graph_publish_clock WHERE project_id=$1").bind(project_id).fetch_optional(self.db.pool()).await?,
        })
    }

    /// Execute the exact unmarked SQL shipped by the legacy warmer.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn legacy_upsert_for_publication_test(
        &self,
        project_id: &str,
        commit_sha: &str,
        graph_blob: &[u8],
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO repo_graph_cache
             (project_id, commit_sha, graph_blob, built_at)
             VALUES ($1, $2, $3, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
             ON CONFLICT (project_id, commit_sha) DO UPDATE SET
                graph_blob = EXCLUDED.graph_blob,
                built_at = EXCLUDED.built_at"#,
        )
        .bind(project_id)
        .bind(commit_sha)
        .bind(graph_blob)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Execute the exact old-server latest-row reader unchanged.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn legacy_latest_for_publication_test(
        &self,
        project_id: &str,
    ) -> Result<LegacyLatestGraph> {
        Ok(sqlx::query_as("SELECT project_id, commit_sha, graph_blob, built_at, generation_id::text AS generation_id FROM repo_graph_cache WHERE project_id = $1 ORDER BY built_at DESC LIMIT 1")
            .bind(project_id).fetch_one(self.db.pool()).await?)
    }
}

#[cfg(test)]
#[cfg(test)]
#[path = "repo_graph_generation_tests.rs"]
mod tests;
