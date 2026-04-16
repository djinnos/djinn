use super::*;

#[cfg(feature = "qdrant")]
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedNote {
    pub values: Vec<f32>,
    pub model_version: String,
}

#[async_trait::async_trait]
pub trait NoteEmbeddingProvider: Send + Sync {
    fn model_version(&self) -> String;
    async fn embed_note(&self, text: &str) -> std::result::Result<EmbeddedNote, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct UpsertNoteEmbedding<'a> {
    pub note_id: &'a str,
    pub content_hash: &'a str,
    pub model_version: &'a str,
    pub embedding: &'a [f32],
    pub branch: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct NoteEmbeddingRecord {
    pub note_id: String,
    pub content_hash: String,
    pub model_version: String,
    pub embedding_dim: i64,
    pub embedded_at: String,
    pub updated_at: String,
    pub extension_state: String,
    pub branch: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NoteEmbeddingMatch {
    pub note_id: String,
    pub distance: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EmbeddingQueryContext<'a> {
    pub branch: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoteVectorBackend {
    SqliteVec,
    Qdrant,
    Noop,
}

#[async_trait::async_trait]
pub trait NoteVectorStore: Send + Sync {
    fn backend(&self) -> NoteVectorBackend;

    async fn can_index(&self, repo: &NoteRepository) -> Result<bool>;

    async fn upsert_embedding(
        &self,
        repo: &NoteRepository,
        input: UpsertNoteEmbedding<'_>,
    ) -> Result<NoteEmbeddingRecord>;

    async fn delete_embedding(&self, repo: &NoteRepository, note_id: &str) -> Result<()>;

    async fn query_similar_embeddings(
        &self,
        repo: &NoteRepository,
        query_embedding: &[f32],
        query: EmbeddingQueryContext<'_>,
        limit: usize,
    ) -> Result<Vec<NoteEmbeddingMatch>>;

    async fn promote_branch(
        &self,
        _repo: &NoteRepository,
        _from_branch: &str,
        _to_branch: &str,
    ) -> Result<u64> {
        Ok(0)
    }

    async fn delete_branch(&self, repo: &NoteRepository, branch: &str) -> Result<u64> {
        let canonical = canonical_embedding_branch(branch);
        let note_ids = sqlx::query_scalar!(
            "SELECT note_id FROM note_embedding_meta WHERE branch = ?",
            canonical
        )
        .fetch_all(repo.db.pool())
        .await?;
        let deleted = note_ids.len() as u64;
        for note_id in note_ids {
            self.delete_embedding(repo, &note_id).await?;
        }
        Ok(deleted)
    }
}

#[derive(Debug, Default)]
pub struct SqliteVecNoteVectorStore;

#[derive(Debug, Default)]
pub struct NoopNoteVectorStore;

#[derive(Clone, Debug)]
pub struct QdrantConfig {
    pub url: String,
    pub api_key: Option<String>,
    pub collection: String,
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:6334".to_owned(),
            api_key: None,
            collection: "notes".to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct QdrantNoteVectorStore {
    config: QdrantConfig,
}

impl QdrantNoteVectorStore {
    pub fn new(config: QdrantConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &QdrantConfig {
        &self.config
    }
}

pub fn infer_embedding_branch_from_worktree(worktree_root: &std::path::Path) -> Option<String> {
    let short_id = worktree_root.file_name()?.to_str()?;
    if short_id.is_empty() || short_id == "_index" {
        return None;
    }
    Some(task_branch_name(short_id))
}

#[cfg(feature = "qdrant")]
impl QdrantNoteVectorStore {
    pub fn client(&self) -> std::result::Result<qdrant_client::Qdrant, String> {
        let builder = qdrant_client::Qdrant::from_url(&self.config.url);
        let builder = match &self.config.api_key {
            Some(api_key) => builder.api_key(api_key.clone()),
            None => builder,
        };
        builder.build().map_err(|error| error.to_string())
    }
}

#[cfg(not(feature = "qdrant"))]
impl QdrantNoteVectorStore {
    pub fn client(&self) -> std::result::Result<(), String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }
}

type EmbeddingRepairRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub fn task_branch_name(task_short_id: &str) -> String {
    format!("task/{task_short_id}")
}

fn canonical_embedding_branch(branch: &str) -> String {
    let trimmed = branch.trim();
    if trimmed.is_empty() {
        return "main".to_string();
    }
    if let Some(short_id) = trimmed.strip_prefix("task_") {
        return task_branch_name(short_id);
    }
    trimmed.to_string()
}

fn embedding_query_branches(branch: Option<&str>) -> Vec<String> {
    let mut branches = vec!["main".to_string()];
    if let Some(branch) = branch {
        let branch = canonical_embedding_branch(branch);
        if branch != "main" {
            branches.insert(0, branch);
        }
    }
    branches
}

pub(super) fn embedding_branch_filter_sql(branch: Option<&str>) -> (String, Vec<String>) {
    let branches = embedding_query_branches(branch);
    let placeholders = std::iter::repeat_n("?", branches.len())
        .collect::<Vec<_>>()
        .join(", ");
    (format!("m.branch IN ({placeholders})"), branches)
}

#[cfg(feature = "qdrant")]
fn qdrant_value_from_str(value: &str) -> qdrant_client::qdrant::Value {
    use qdrant_client::qdrant::value::Kind;

    qdrant_client::qdrant::Value {
        kind: Some(Kind::StringValue(value.to_string())),
    }
}

#[cfg(feature = "qdrant")]
fn qdrant_keyword_condition(key: &str, value: &str) -> qdrant_client::qdrant::Condition {
    qdrant_client::qdrant::Condition::matches(key, value.to_string())
}

#[cfg(feature = "qdrant")]
fn qdrant_note_point_id(note_id: &str) -> qdrant_client::qdrant::PointId {
    note_id.to_string().into()
}

#[cfg(feature = "qdrant")]
fn qdrant_payload(
    input: &UpsertNoteEmbedding<'_>,
) -> HashMap<String, qdrant_client::qdrant::Value> {
    HashMap::from([
        ("note_id".to_string(), qdrant_value_from_str(input.note_id)),
        (
            "content_hash".to_string(),
            qdrant_value_from_str(input.content_hash),
        ),
        (
            "model_version".to_string(),
            qdrant_value_from_str(input.model_version),
        ),
        (
            "branch".to_string(),
            qdrant_value_from_str(&canonical_embedding_branch(input.branch)),
        ),
    ])
}

#[cfg(feature = "qdrant")]
fn qdrant_branch_filter(branch: Option<&str>) -> qdrant_client::qdrant::Filter {
    let branches = embedding_query_branches(branch);
    qdrant_client::qdrant::Filter::should(
        branches
            .into_iter()
            .map(|branch| qdrant_keyword_condition("branch", &branch)),
    )
}

#[cfg(feature = "qdrant")]
fn qdrant_note_ids_list(note_ids: &[String]) -> qdrant_client::qdrant::PointsIdsList {
    qdrant_client::qdrant::PointsIdsList {
        ids: note_ids
            .iter()
            .map(|note_id| qdrant_note_point_id(note_id))
            .collect(),
    }
}

fn embedding_to_blob(embedding: &[f32]) -> Vec<u8> {
    embedding
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

pub(super) fn embedding_document_text(
    title: &str,
    note_type: &str,
    tags: &str,
    content: &str,
) -> String {
    format!("title: {title}\ntype: {note_type}\ntags: {tags}\n\n{content}")
}

pub(super) fn embedding_content_hash(
    title: &str,
    note_type: &str,
    tags: &str,
    content: &str,
) -> String {
    crate::note_hash::note_content_hash(&embedding_document_text(title, note_type, tags, content))
}

async fn upsert_embedding_metadata(
    repo: &NoteRepository,
    input: UpsertNoteEmbedding<'_>,
    extension_state: &str,
) -> Result<NoteEmbeddingRecord> {
    let embedding_dim = i64::try_from(input.embedding.len())
        .map_err(|_| Error::InvalidData("embedding dimension exceeds i64".to_owned()))?;
    let embedding_blob = embedding_to_blob(input.embedding);

    let mut tx = repo.db.pool().begin().await?;

    sqlx::query!(
        "INSERT INTO note_embeddings (note_id, embedding, embedding_dim, updated_at)
         VALUES (?, ?, ?, NOW(3))
         ON DUPLICATE KEY UPDATE
             embedding = VALUES(embedding),
             embedding_dim = VALUES(embedding_dim),
             updated_at = NOW(3)",
        input.note_id,
        embedding_blob,
        embedding_dim
    )
    .execute(&mut *tx)
    .await?;

    let canonical_branch = canonical_embedding_branch(input.branch);
    sqlx::query!(
        "INSERT INTO note_embedding_meta (
            note_id, content_hash, embedded_at, model_version, embedding_dim, extension_state, branch
         ) VALUES (
            ?, ?, NOW(3), ?, ?, ?, ?
         )
         ON DUPLICATE KEY UPDATE
            content_hash = VALUES(content_hash),
            embedded_at = NOW(3),
            model_version = VALUES(model_version),
            embedding_dim = VALUES(embedding_dim),
            extension_state = VALUES(extension_state),
            branch = VALUES(branch)",
        input.note_id,
        input.content_hash,
        input.model_version,
        embedding_dim,
        extension_state,
        canonical_branch
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    repo.get_embedding(input.note_id)
        .await?
        .ok_or_else(|| Error::Internal("embedding row missing after upsert".to_owned()))
}

async fn delete_embedding_metadata(repo: &NoteRepository, note_id: &str) -> Result<()> {
    let mut tx = repo.db.pool().begin().await?;
    sqlx::query!("DELETE FROM note_embeddings WHERE note_id = ?", note_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM note_embedding_meta WHERE note_id = ?",
        note_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[async_trait::async_trait]
impl NoteVectorStore for SqliteVecNoteVectorStore {
    fn backend(&self) -> NoteVectorBackend {
        NoteVectorBackend::SqliteVec
    }

    async fn can_index(&self, repo: &NoteRepository) -> Result<bool> {
        Ok(repo.db.sqlite_vec_status().await?.available)
    }

    async fn upsert_embedding(
        &self,
        repo: &NoteRepository,
        input: UpsertNoteEmbedding<'_>,
    ) -> Result<NoteEmbeddingRecord> {
        let status = repo.db.sqlite_vec_status().await?;
        let record = upsert_embedding_metadata(
            repo,
            UpsertNoteEmbedding {
                note_id: input.note_id,
                content_hash: input.content_hash,
                model_version: input.model_version,
                embedding: input.embedding,
                branch: input.branch,
            },
            if status.available { "ready" } else { "pending" },
        )
        .await?;

        if status.available {
            let mut tx = repo.db.pool().begin().await?;
            // NOTE: SQLite-only vss table (note_embeddings_vec) — compile-time check not possible against MySQL
            sqlx::query("DELETE FROM note_embeddings_vec WHERE note_id = ?1")
                .bind(input.note_id)
                .execute(&mut *tx)
                .await?;
            // NOTE: SQLite-only vss table (note_embeddings_vec) — compile-time check not possible against MySQL
            sqlx::query("INSERT INTO note_embeddings_vec (note_id, embedding) VALUES (?1, ?2)")
                .bind(input.note_id)
                .bind(embedding_to_blob(input.embedding))
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
        }

        Ok(record)
    }

    async fn delete_embedding(&self, repo: &NoteRepository, note_id: &str) -> Result<()> {
        let status = repo.db.sqlite_vec_status().await?;
        if status.available {
            // NOTE: SQLite-only vss table (note_embeddings_vec) — compile-time check not possible against MySQL
            sqlx::query("DELETE FROM note_embeddings_vec WHERE note_id = ?1")
                .bind(note_id)
                .execute(repo.db.pool())
                .await?;
        }
        delete_embedding_metadata(repo, note_id).await
    }

    async fn query_similar_embeddings(
        &self,
        repo: &NoteRepository,
        query_embedding: &[f32],
        query: EmbeddingQueryContext<'_>,
        limit: usize,
    ) -> Result<Vec<NoteEmbeddingMatch>> {
        let status = repo.db.sqlite_vec_status().await?;
        if !status.available {
            return Ok(vec![]);
        }

        let limit = i64::try_from(limit)
            .map_err(|_| Error::InvalidData("embedding query limit exceeds i64".to_owned()))?;
        let branches = embedding_query_branches(query.branch);
        let placeholders = std::iter::repeat_n("?", branches.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT v.note_id, v.distance
             FROM note_embeddings_vec v
             JOIN note_embedding_meta m ON m.note_id = v.note_id
             WHERE v.embedding MATCH ?1 AND v.k = ?2
               AND m.branch IN ({placeholders})",
        );
        let mut rows_query = sqlx::query_as::<_, (String, f64)>(&sql)
            .bind(embedding_to_blob(query_embedding))
            .bind(limit);
        for branch in &branches {
            rows_query = rows_query.bind(branch);
        }
        let rows = rows_query.fetch_all(repo.db.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|(note_id, distance)| NoteEmbeddingMatch { note_id, distance })
            .collect())
    }
}

#[async_trait::async_trait]
impl NoteVectorStore for NoopNoteVectorStore {
    fn backend(&self) -> NoteVectorBackend {
        NoteVectorBackend::Noop
    }

    async fn can_index(&self, _repo: &NoteRepository) -> Result<bool> {
        Ok(false)
    }

    async fn upsert_embedding(
        &self,
        repo: &NoteRepository,
        input: UpsertNoteEmbedding<'_>,
    ) -> Result<NoteEmbeddingRecord> {
        upsert_embedding_metadata(repo, input, "pending").await
    }

    async fn delete_embedding(&self, repo: &NoteRepository, note_id: &str) -> Result<()> {
        delete_embedding_metadata(repo, note_id).await
    }

    async fn query_similar_embeddings(
        &self,
        _repo: &NoteRepository,
        _query_embedding: &[f32],
        _query: EmbeddingQueryContext<'_>,
        _limit: usize,
    ) -> Result<Vec<NoteEmbeddingMatch>> {
        Ok(vec![])
    }
}

#[cfg(feature = "qdrant")]
impl QdrantNoteVectorStore {
    async fn ensure_branch_index(&self, client: &qdrant_client::Qdrant) {
        use qdrant_client::qdrant::{CreateFieldIndexCollectionBuilder, FieldType};

        if let Err(error) = client
            .create_field_index(
                CreateFieldIndexCollectionBuilder::new(
                    &self.config.collection,
                    "branch",
                    FieldType::Keyword,
                )
                .wait(true),
            )
            .await
        {
            tracing::debug!(
                %error,
                collection = %self.config.collection,
                "failed to ensure qdrant branch payload index"
            );
        }
    }

    async fn qdrant_upsert(
        &self,
        input: &UpsertNoteEmbedding<'_>,
    ) -> std::result::Result<(), String> {
        use qdrant_client::qdrant::{PointStruct, UpsertPointsBuilder};

        let client = self.client()?;
        self.ensure_branch_index(&client).await;
        client
            .upsert_points(
                UpsertPointsBuilder::new(
                    &self.config.collection,
                    vec![PointStruct::new(
                        qdrant_note_point_id(input.note_id),
                        input.embedding.to_vec(),
                        qdrant_payload(input),
                    )],
                )
                .wait(true),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn qdrant_delete_note(&self, note_id: &str) -> std::result::Result<(), String> {
        use qdrant_client::qdrant::DeletePointsBuilder;

        let client = self.client()?;
        client
            .delete_points(
                DeletePointsBuilder::new(&self.config.collection)
                    .wait(true)
                    .points(vec![qdrant_note_point_id(note_id)]),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn qdrant_set_branch(
        &self,
        note_ids: &[String],
        branch: &str,
    ) -> std::result::Result<(), String> {
        use qdrant_client::qdrant::SetPayloadPointsBuilder;

        if note_ids.is_empty() {
            return Ok(());
        }

        let client = self.client()?;
        client
            .set_payload(
                SetPayloadPointsBuilder::new(
                    &self.config.collection,
                    HashMap::from([(
                        "branch".to_string(),
                        qdrant_value_from_str(&canonical_embedding_branch(branch)),
                    )]),
                )
                .wait(true)
                .points_selector(qdrant_note_ids_list(note_ids)),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn qdrant_delete_branch(&self, branch: &str) -> std::result::Result<(), String> {
        use qdrant_client::qdrant::DeletePointsBuilder;

        let client = self.client()?;
        client
            .delete_points(
                DeletePointsBuilder::new(&self.config.collection)
                    .wait(true)
                    .points(qdrant_branch_filter(Some(branch))),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[cfg(not(feature = "qdrant"))]
impl QdrantNoteVectorStore {
    async fn qdrant_upsert(
        &self,
        _input: &UpsertNoteEmbedding<'_>,
    ) -> std::result::Result<(), String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }

    async fn qdrant_delete_note(&self, _note_id: &str) -> std::result::Result<(), String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }

    async fn qdrant_set_branch(
        &self,
        _note_ids: &[String],
        _branch: &str,
    ) -> std::result::Result<(), String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }

    async fn qdrant_delete_branch(&self, _branch: &str) -> std::result::Result<(), String> {
        Err("qdrant support not compiled in; enable the 'qdrant' feature".to_owned())
    }
}

#[async_trait::async_trait]
impl NoteVectorStore for QdrantNoteVectorStore {
    fn backend(&self) -> NoteVectorBackend {
        NoteVectorBackend::Qdrant
    }

    async fn can_index(&self, _repo: &NoteRepository) -> Result<bool> {
        match self.client() {
            Ok(_) => Ok(true),
            Err(error) => {
                tracing::debug!(
                    %error,
                    collection = %self.config.collection,
                    "qdrant vector store unavailable; falling back to metadata-only embedding persistence"
                );
                Ok(false)
            }
        }
    }

    async fn upsert_embedding(
        &self,
        repo: &NoteRepository,
        input: UpsertNoteEmbedding<'_>,
    ) -> Result<NoteEmbeddingRecord> {
        match self.qdrant_upsert(&input).await {
            Ok(()) => upsert_embedding_metadata(repo, input, "ready").await,
            Err(error) => {
                tracing::debug!(
                    %error,
                    note_id = input.note_id,
                    collection = %self.config.collection,
                    "qdrant upsert unavailable; keeping local metadata pending"
                );
                upsert_embedding_metadata(repo, input, "pending").await
            }
        }
    }

    async fn delete_embedding(&self, repo: &NoteRepository, note_id: &str) -> Result<()> {
        if let Err(error) = self.qdrant_delete_note(note_id).await {
            tracing::debug!(
                %error,
                note_id,
                collection = %self.config.collection,
                "qdrant delete unavailable; deleting local metadata only"
            );
        }
        delete_embedding_metadata(repo, note_id).await
    }

    async fn query_similar_embeddings(
        &self,
        _repo: &NoteRepository,
        query_embedding: &[f32],
        query: EmbeddingQueryContext<'_>,
        limit: usize,
    ) -> Result<Vec<NoteEmbeddingMatch>> {
        #[cfg(feature = "qdrant")]
        {
            use qdrant_client::qdrant::SearchPointsBuilder;

            let limit = u64::try_from(limit)
                .map_err(|_| Error::InvalidData("embedding query limit exceeds u64".to_owned()))?;
            let client = match self.client() {
                Ok(client) => client,
                Err(error) => {
                    tracing::debug!(
                        %error,
                        collection = %self.config.collection,
                        "qdrant query unavailable; returning empty semantic matches"
                    );
                    return Ok(vec![]);
                }
            };

            let response = match client
                .search_points(
                    SearchPointsBuilder::new(
                        &self.config.collection,
                        query_embedding.to_vec(),
                        limit,
                    )
                    .filter(qdrant_branch_filter(query.branch))
                    .with_payload(true),
                )
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    tracing::debug!(
                        %error,
                        collection = %self.config.collection,
                        "qdrant search failed; returning empty semantic matches"
                    );
                    return Ok(vec![]);
                }
            };

            return Ok(response
                .result
                .into_iter()
                .filter_map(|point| {
                    let note_id =
                        point
                            .payload
                            .get("note_id")
                            .and_then(|value| match &value.kind {
                                Some(qdrant_client::qdrant::value::Kind::StringValue(value)) => {
                                    Some(value.clone())
                                }
                                _ => None,
                            })?;
                    Some(NoteEmbeddingMatch {
                        note_id,
                        distance: -(point.score as f64),
                    })
                })
                .collect());
        }

        #[cfg(not(feature = "qdrant"))]
        {
            let _ = (query_embedding, query, limit);
            Ok(vec![])
        }
    }

    async fn promote_branch(
        &self,
        repo: &NoteRepository,
        from_branch: &str,
        to_branch: &str,
    ) -> Result<u64> {
        let from_branch = canonical_embedding_branch(from_branch);
        let to_branch = canonical_embedding_branch(to_branch);
        let note_ids = sqlx::query_scalar!(
            "SELECT note_id FROM note_embedding_meta WHERE branch = ?",
            from_branch
        )
        .fetch_all(repo.db.pool())
        .await?;
        if let Err(error) = self.qdrant_set_branch(&note_ids, &to_branch).await {
            tracing::debug!(
                %error,
                from_branch,
                to_branch,
                collection = %self.config.collection,
                "qdrant branch promotion unavailable; updating local metadata only"
            );
        }
        Ok(note_ids.len() as u64)
    }

    async fn delete_branch(&self, repo: &NoteRepository, branch: &str) -> Result<u64> {
        let branch = canonical_embedding_branch(branch);
        let note_ids = sqlx::query_scalar!(
            "SELECT note_id FROM note_embedding_meta WHERE branch = ?",
            branch
        )
        .fetch_all(repo.db.pool())
        .await?;
        if let Err(error) = self.qdrant_delete_branch(&branch).await {
            tracing::debug!(
                %error,
                branch,
                collection = %self.config.collection,
                "qdrant branch delete unavailable; deleting local metadata only"
            );
        }
        for note_id in &note_ids {
            delete_embedding_metadata(repo, note_id).await?;
        }
        Ok(note_ids.len() as u64)
    }
}

impl NoteRepository {
    pub(super) async fn sync_note_embedding(
        &self,
        note_id: &str,
        title: &str,
        note_type: &str,
        tags: &str,
        content: &str,
    ) {
        let Some(provider) = self.embedding_provider() else {
            return;
        };

        let semantic_text = embedding_document_text(title, note_type, tags, content);
        let content_hash = embedding_content_hash(title, note_type, tags, content);

        match provider.embed_note(&semantic_text).await {
            Ok(embedded) => {
                if let Err(error) = self
                    .upsert_embedding(UpsertNoteEmbedding {
                        note_id,
                        content_hash: &content_hash,
                        model_version: &embedded.model_version,
                        embedding: &embedded.values,
                        branch: self.embedding_branch(),
                    })
                    .await
                {
                    tracing::warn!(note_id, %error, "failed to upsert note embedding");
                }
            }
            Err(reason) => {
                tracing::debug!(note_id, %reason, "semantic embedding unavailable; continuing with lexical indexing only");
            }
        }
    }

    pub(super) async fn purge_orphan_embeddings(&self) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let orphan_ids = sqlx::query_scalar!(
            "SELECT m.note_id
             FROM note_embedding_meta m
             LEFT JOIN notes n ON n.id = m.note_id
             WHERE n.id IS NULL"
        )
        .fetch_all(self.db.pool())
        .await?;

        let mut deleted = 0u64;
        for note_id in orphan_ids {
            self.delete_embedding(&note_id).await?;
            deleted += 1;
        }

        Ok(deleted)
    }

    pub(super) async fn repair_project_embeddings(&self, project_id: &str) -> Result<u64> {
        let Some(provider) = self.embedding_provider() else {
            return Ok(0);
        };

        self.db.ensure_initialized().await?;
        self.purge_orphan_embeddings().await?;

        let current_model_version = provider.model_version();
        let rows: Vec<EmbeddingRepairRow> = sqlx::query!(
            r#"SELECT n.id, n.title, n.note_type, n.tags, n.content,
                        m.content_hash AS "content_hash: Option<String>",
                        m.model_version AS "model_version: Option<String>",
                        m.branch AS "branch: Option<String>"
                 FROM notes n
                 LEFT JOIN note_embedding_meta m ON m.note_id = n.id
                 WHERE n.project_id = ?"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?
        .into_iter()
        .map(|r| {
            (
                r.id,
                r.title,
                r.note_type,
                r.tags,
                r.content,
                r.content_hash,
                r.model_version,
                r.branch,
            )
        })
        .collect();

        let total = rows.len();
        let start = std::time::Instant::now();
        let mut repaired = 0u64;
        for (
            note_id,
            title,
            note_type,
            tags,
            content,
            embedded_hash,
            embedded_model_version,
            embedded_branch,
        ) in rows
        {
            tokio::task::yield_now().await;
            let expected_hash = embedding_content_hash(&title, &note_type, &tags, &content);
            let needs_refresh = embedded_hash.as_deref() != Some(expected_hash.as_str())
                || embedded_model_version.as_deref() != Some(current_model_version.as_str())
                || embedded_branch.as_deref() != Some(self.embedding_branch());
            if needs_refresh {
                repaired += 1;
                self.sync_note_embedding(&note_id, &title, &note_type, &tags, &content)
                    .await;
            }
        }

        if repaired > 0 {
            tracing::info!(
                project_id,
                total,
                repaired,
                elapsed_secs = start.elapsed().as_secs_f32(),
                "embedding repair completed"
            );
        }

        Ok(repaired)
    }

    pub async fn upsert_embedding(
        &self,
        input: UpsertNoteEmbedding<'_>,
    ) -> Result<NoteEmbeddingRecord> {
        self.db.ensure_initialized().await?;
        self.vector_store().upsert_embedding(self, input).await
    }

    pub async fn get_embedding(&self, note_id: &str) -> Result<Option<NoteEmbeddingRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            NoteEmbeddingRecord,
            "SELECT m.note_id, m.content_hash, m.model_version, m.embedding_dim, m.embedded_at,
                    e.updated_at, m.extension_state, m.branch
             FROM note_embedding_meta m
             JOIN note_embeddings e ON e.note_id = m.note_id
             WHERE m.note_id = ?",
            note_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn delete_embedding(&self, note_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        self.vector_store().delete_embedding(self, note_id).await
    }

    pub async fn query_similar_embeddings(
        &self,
        query_embedding: &[f32],
        query: EmbeddingQueryContext<'_>,
        limit: usize,
    ) -> Result<Vec<NoteEmbeddingMatch>> {
        self.db.ensure_initialized().await?;
        self.vector_store()
            .query_similar_embeddings(self, query_embedding, query, limit)
            .await
    }

    pub async fn promote_branch_embeddings(
        &self,
        from_branch: &str,
        to_branch: &str,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;
        let promoted = self
            .vector_store()
            .promote_branch(self, from_branch, to_branch)
            .await?;
        let to_branch_c = canonical_embedding_branch(to_branch);
        let from_branch_c = canonical_embedding_branch(from_branch);
        let result = sqlx::query!(
            "UPDATE note_embedding_meta
             SET branch = ?
             WHERE branch = ?",
            to_branch_c,
            from_branch_c
        )
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected().max(promoted))
    }

    pub async fn delete_embeddings_for_branch(&self, branch: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;
        self.vector_store().delete_branch(self, branch).await
    }

    pub async fn embedding_branch_counts(&self) -> Result<Vec<(String, i64)>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query!(
            r#"SELECT branch, COUNT(*) AS "count!: i64"
                 FROM note_embedding_meta
                 GROUP BY branch
                 ORDER BY branch"#
        )
        .fetch_all(self.db.pool())
        .await?
        .into_iter()
        .map(|r| (r.branch, r.count))
        .collect())
    }

    pub async fn embedding_branch_for_note(&self, note_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar!(
            "SELECT branch FROM note_embedding_meta WHERE note_id = ?",
            note_id
        )
        .fetch_optional(self.db.pool())
        .await?)
    }
}
