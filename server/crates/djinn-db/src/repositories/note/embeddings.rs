use super::*;

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
}

#[derive(Clone, Debug, PartialEq)]
pub struct NoteEmbeddingMatch {
    pub note_id: String,
    pub distance: f64,
}

type EmbeddingRepairRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

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

        let orphan_ids: Vec<(String,)> = sqlx::query_as(
            "SELECT m.note_id
             FROM note_embedding_meta m
             LEFT JOIN notes n ON n.id = m.note_id
             WHERE n.id IS NULL",
        )
        .fetch_all(self.db.pool())
        .await?;

        let mut deleted = 0u64;
        for (note_id,) in orphan_ids {
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
        let rows: Vec<EmbeddingRepairRow> = sqlx::query_as(
            "SELECT n.id, n.title, n.note_type, n.tags, n.content,
                        m.content_hash, m.model_version
                 FROM notes n
                 LEFT JOIN note_embedding_meta m ON m.note_id = n.id
                 WHERE n.project_id = ?1",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let total = rows.len();
        let start = std::time::Instant::now();
        let mut repaired = 0u64;
        for (note_id, title, note_type, tags, content, embedded_hash, embedded_model_version) in
            rows
        {
            // Yield between notes so the async runtime stays responsive
            // during bulk embedding repair (can be 1000+ notes).
            tokio::task::yield_now().await;
            let expected_hash = embedding_content_hash(&title, &note_type, &tags, &content);
            let needs_refresh = embedded_hash.as_deref() != Some(expected_hash.as_str())
                || embedded_model_version.as_deref() != Some(current_model_version.as_str());
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
        let status = self.db.sqlite_vec_status().await?;

        let embedding_dim = i64::try_from(input.embedding.len())
            .map_err(|_| Error::InvalidData("embedding dimension exceeds i64".to_owned()))?;
        let embedding_blob = embedding_to_blob(input.embedding);
        let extension_state = if status.available { "ready" } else { "pending" };

        let mut tx = self.db.pool().begin().await?;

        sqlx::query(
            "INSERT INTO note_embeddings (note_id, embedding, embedding_dim, updated_at)
             VALUES (?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
             ON CONFLICT(note_id) DO UPDATE SET
                 embedding = excluded.embedding,
                 embedding_dim = excluded.embedding_dim,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        )
        .bind(input.note_id)
        .bind(embedding_blob)
        .bind(embedding_dim)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO note_embedding_meta (
                note_id, content_hash, embedded_at, model_version, embedding_dim, extension_state
             ) VALUES (
                ?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?3, ?4, ?5
             )
             ON CONFLICT(note_id) DO UPDATE SET
                content_hash = excluded.content_hash,
                embedded_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                model_version = excluded.model_version,
                embedding_dim = excluded.embedding_dim,
                extension_state = excluded.extension_state",
        )
        .bind(input.note_id)
        .bind(input.content_hash)
        .bind(input.model_version)
        .bind(embedding_dim)
        .bind(extension_state)
        .execute(&mut *tx)
        .await?;

        if status.available {
            sqlx::query("DELETE FROM note_embeddings_vec WHERE note_id = ?1")
                .bind(input.note_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO note_embeddings_vec (note_id, embedding) VALUES (?1, ?2)")
                .bind(input.note_id)
                .bind(embedding_to_blob(input.embedding))
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        self.get_embedding(input.note_id)
            .await?
            .ok_or_else(|| Error::Internal("embedding row missing after upsert".to_owned()))
    }

    pub async fn get_embedding(&self, note_id: &str) -> Result<Option<NoteEmbeddingRecord>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, NoteEmbeddingRecord>(
            "SELECT m.note_id, m.content_hash, m.model_version, m.embedding_dim, m.embedded_at,
                    e.updated_at, m.extension_state
             FROM note_embedding_meta m
             JOIN note_embeddings e ON e.note_id = m.note_id
             WHERE m.note_id = ?1",
        )
        .bind(note_id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn delete_embedding(&self, note_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let status = self.db.sqlite_vec_status().await?;

        let mut tx = self.db.pool().begin().await?;
        if status.available {
            sqlx::query("DELETE FROM note_embeddings_vec WHERE note_id = ?1")
                .bind(note_id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM note_embeddings WHERE note_id = ?1")
            .bind(note_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM note_embedding_meta WHERE note_id = ?1")
            .bind(note_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn query_similar_embeddings(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<NoteEmbeddingMatch>> {
        self.db.ensure_initialized().await?;
        let status = self.db.sqlite_vec_status().await?;
        if !status.available {
            return Ok(vec![]);
        }

        let limit = i64::try_from(limit)
            .map_err(|_| Error::InvalidData("embedding query limit exceeds i64".to_owned()))?;
        let rows = sqlx::query_as::<_, (String, f64)>(
            "SELECT note_id, distance
             FROM note_embeddings_vec
             WHERE embedding MATCH ?1 AND k = ?2",
        )
        .bind(embedding_to_blob(query_embedding))
        .bind(limit)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|(note_id, distance)| NoteEmbeddingMatch { note_id, distance })
            .collect())
    }
}
