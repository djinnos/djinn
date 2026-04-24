use super::*;
use crate::note_hash::note_content_hash;
use crate::retry::is_serialization_failure;

/// Returns `true` for note types whose db rows are produced by the
/// consolidation pipeline (see `consolidation.rs`).
///
/// Pre-cut-over the helper distinguished consolidation-owned types from
/// types that should be auto-promoted to file storage on edit. With the
/// db-only knowledge-base cut-over there is no file storage anymore, but
/// the list is kept around as an alias for any future caller that needs
/// to scope queries to consolidation-eligible types.
#[allow(dead_code)]
pub(super) fn db_only_consolidation_type(note_type: &str) -> bool {
    matches!(note_type, "case" | "pattern" | "pitfall")
}

impl NoteRepository {
    pub async fn upsert_db_note_by_permalink(
        &self,
        project_id: &str,
        permalink: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
    ) -> Result<Note> {
        if let Some(existing) = self.get_by_permalink(project_id, permalink).await? {
            return self.update(&existing.id, title, content, tags).await;
        }

        self.create_db_note_with_permalink(project_id, permalink, title, content, note_type, tags)
            .await
    }

    pub async fn create_db_note(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
    ) -> Result<Note> {
        self.create_internal(
            project_id, None, title, content, note_type, None, tags, "[]",
        )
        .await
    }

    pub async fn create_db_note_with_scope(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
        scope_paths: &str,
    ) -> Result<Note> {
        self.create_internal(
            project_id,
            None,
            title,
            content,
            note_type,
            None,
            tags,
            scope_paths,
        )
        .await
    }

    pub async fn update_scope_paths(&self, id: &str, scope_paths: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = id.to_owned();
        let scope_paths = scope_paths.to_owned();

        sqlx::query!(
            "UPDATE notes SET
                scope_paths = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            scope_paths,
            id
        )
        .execute(self.db.pool())
        .await?;

        let note = note_select_where_id!(id).fetch_one(self.db.pool()).await?;

        self.events.send(djinn_memory::events::note_updated(&note));
        Ok(note)
    }

    pub async fn update_tags(&self, id: &str, tags: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = id.to_owned();
        let tags = tags.to_owned();

        sqlx::query!(
            "UPDATE notes SET
                tags = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            tags,
            id
        )
        .execute(self.db.pool())
        .await?;

        let note = note_select_where_id!(id).fetch_one(self.db.pool()).await?;

        self.events.send(djinn_memory::events::note_updated(&note));
        Ok(note)
    }

    pub async fn create_db_note_with_permalink(
        &self,
        project_id: &str,
        permalink: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
    ) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = uuid::Uuid::now_v7().to_string();
        let project_id = project_id.to_owned();
        let permalink = permalink.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let note_type = note_type.to_owned();
        let folder = folder_for_type(&note_type).to_owned();
        let tags = tags.to_owned();

        let content_hash = note_content_hash(&content);
        let empty_scope = "[]".to_string();

        // Retry on Dolt 1213 — same rationale as `create_internal`.
        let note: Note = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || async {
                let mut tx = self.db.pool().begin().await?;

                // `storage` and `file_path` are vestigial columns from the
                // file-on-disk era; we still write them for back-compat with
                // pre-cut-over rows but they are no longer read by new code.
                sqlx::query!(
                    "INSERT INTO notes
                        (id, project_id, permalink, title, file_path,
                         storage, note_type, folder, tags, content, content_hash, scope_paths)
                     VALUES (?, ?, ?, ?, '', 'db', ?, ?, ?, ?, ?, ?)",
                    id,
                    project_id,
                    permalink,
                    title,
                    note_type,
                    folder,
                    tags,
                    content,
                    content_hash,
                    empty_scope
                )
                .execute(&mut *tx)
                .await?;

                index_links_for_note(&mut tx, &id, &project_id, &content).await?;
                resolve_links_for_note(&mut tx, &id, &title, &permalink, &project_id).await?;

                let note = note_select_where_id!(id).fetch_one(&mut *tx).await?;

                tx.commit().await?;
                Ok::<_, crate::Error>(note)
            },
        )
        .await?;
        self.spawn_note_embedding_sync(&note);
        self.events.send(djinn_memory::events::note_created(&note));
        Ok(note)
    }

    /// Create a new note in Dolt. This is the single entry point for note
    /// creation. `tags` and `scope_paths` must be JSON array strings, e.g.
    /// `'["rust","db"]'`. Singleton types (`brief`, `roadmap`) use a fixed
    /// permalink (the type name) — the caller is expected to use
    /// `get_by_permalink` + `update` to reconcile when a row already exists.
    pub async fn create(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
    ) -> Result<Note> {
        self.create_internal(
            project_id, None, title, content, note_type, None, tags, "[]",
        )
        .await
    }

    pub async fn create_with_status(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        status: Option<&str>,
        tags: &str,
    ) -> Result<Note> {
        self.create_internal(project_id, None, title, content, note_type, status, tags, "[]")
            .await
    }

    /// Create with explicit `scope_paths`.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_scope(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        status: Option<&str>,
        tags: &str,
        scope_paths: &str,
    ) -> Result<Note> {
        self.create_internal(
            project_id,
            None,
            title,
            content,
            note_type,
            status,
            tags,
            scope_paths,
        )
        .await
    }

    /// Single source of truth for INSERTing a note row.
    ///
    /// Performs the INSERT + wikilink indexing + broken-link resolution in
    /// one Dolt-retried transaction. Embedding generation is scheduled on a
    /// background tokio task so the caller's MCP response is not blocked
    /// behind the (potentially slow) provider round-trip.
    ///
    /// `_unused_project_path` is retained as an unused argument for
    /// signature compatibility with the (deleted) file-storage path; its
    /// value is ignored. New callers should pass `None`.
    #[allow(clippy::too_many_arguments)]
    async fn create_internal(
        &self,
        project_id: &str,
        _unused_project_path: Option<&std::path::Path>,
        title: &str,
        content: &str,
        note_type: &str,
        status: Option<&str>,
        tags: &str,
        scope_paths: &str,
    ) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = uuid::Uuid::now_v7().to_string();
        // Permalink scheme (including the proposed-ADR `decisions/proposed/...`
        // path) is preserved verbatim from the legacy file-on-disk era. The
        // permalink is a pure identifier now — no longer tied to a real
        // filesystem path.
        let permalink = permalink_for_with_status(note_type, title, status);

        let project_id = project_id.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let note_type = note_type.to_owned();
        let folder = folder_for_type_with_status(&note_type, status).to_owned();
        let tags = tags.to_owned();
        let scope_paths = scope_paths.to_owned();

        let content_hash = note_content_hash(&content);

        // Retry the INSERT + link-indexing transaction on Dolt 1213
        // serialization failures. Notes + note_links are hot tables during
        // concurrent test runs and the conflict is benign — the committed
        // peer has already persisted, the retry reopens a fresh tx and
        // succeeds.
        let note: Note = crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || async {
                let mut tx = self.db.pool().begin().await?;

                // `storage` is now always 'db'; `file_path` is the empty
                // string. Both columns are vestiges of the file-on-disk
                // era, kept on the schema to avoid a migration in the same
                // PR that does the cut-over. Drop them in a follow-up.
                sqlx::query!(
                    "INSERT INTO notes
                        (id, project_id, permalink, title, file_path,
                         storage, note_type, folder, tags, content, content_hash, scope_paths)
                     VALUES (?, ?, ?, ?, '', 'db', ?, ?, ?, ?, ?, ?)",
                    id,
                    project_id,
                    permalink,
                    title,
                    note_type,
                    folder,
                    tags,
                    content,
                    content_hash,
                    scope_paths
                )
                .execute(&mut *tx)
                .await?;

                index_links_for_note(&mut tx, &id, &project_id, &content).await?;
                resolve_links_for_note(&mut tx, &id, &title, &permalink, &project_id).await?;

                let note = note_select_where_id!(id).fetch_one(&mut *tx).await?;

                tx.commit().await?;
                Ok::<_, crate::Error>(note)
            },
        )
        .await?;

        self.spawn_note_embedding_sync(&note);
        self.events.send(djinn_memory::events::note_created(&note));
        Ok(note)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Note,
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, tags, content,
                      CAST(created_at AS CHAR) as "created_at!: String",
                      CAST(updated_at AS CHAR) as "updated_at!: String",
                      CAST(last_accessed AS CHAR) as "last_accessed!: String",
                      access_count, confidence, abstract as abstract_, overview,
                      scope_paths
               FROM notes WHERE id = ?"#,
            id,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_permalink(
        &self,
        project_id: &str,
        permalink: &str,
    ) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Note,
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, tags, content,
                      CAST(created_at AS CHAR) as "created_at!: String",
                      CAST(updated_at AS CHAR) as "updated_at!: String",
                      CAST(last_accessed AS CHAR) as "last_accessed!: String",
                      access_count, confidence, abstract as abstract_, overview,
                      scope_paths
               FROM notes WHERE project_id = ? AND permalink = ?"#,
            project_id,
            permalink,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn find_by_content_hash(
        &self,
        project_id: &str,
        content_hash: &str,
    ) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Note,
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, tags, content,
                      CAST(created_at AS CHAR) as "created_at!: String",
                      CAST(updated_at AS CHAR) as "updated_at!: String",
                      CAST(last_accessed AS CHAR) as "last_accessed!: String",
                      access_count, confidence, abstract as abstract_, overview,
                      scope_paths
               FROM notes
               WHERE project_id = ? AND content_hash = ?
               ORDER BY created_at ASC
               LIMIT 1"#,
            project_id,
            content_hash,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_summary_state(&self, id: &str) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            Note,
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, tags, content,
                      CAST(created_at AS CHAR) as "created_at!: String",
                      CAST(updated_at AS CHAR) as "updated_at!: String",
                      CAST(last_accessed AS CHAR) as "last_accessed!: String",
                      access_count, confidence, abstract as abstract_, overview,
                      scope_paths
               FROM notes WHERE id = ?"#,
            id,
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a note by permalink (primary) or title search (fallback).
    ///
    /// This is the canonical way to look up a note when the caller has a
    /// human-supplied identifier that could be either a permalink slug or a
    /// (partial) title.
    pub async fn resolve(&self, project_id: &str, identifier: &str) -> Result<Option<Note>> {
        let trimmed = identifier.trim();
        if !trimmed.is_empty() {
            let without_scheme = trimmed.strip_prefix("memory://").unwrap_or(trimmed);
            let normalized = normalize_virtual_note_path(without_scheme);
            if !normalized.is_empty() {
                if let Some(n) = self.get_by_permalink(project_id, &normalized).await? {
                    return Ok(Some(n));
                }
                if let Some(permalink) = permalink_from_virtual_note_path(&normalized)
                    && permalink != normalized
                    && let Some(n) = self.get_by_permalink(project_id, &permalink).await?
                {
                    return Ok(Some(n));
                }
            }
        }
        let results = self
            .search(NoteSearchParams {
                project_id,
                query: identifier,
                task_id: None,
                folder: None,
                note_type: None,
                limit: 1,
                semantic_scores: None,
            })
            .await?;
        if let Some(r) = results.into_iter().next() {
            return self.get(&r.id).await;
        }
        Ok(None)
    }

    pub async fn list(&self, project_id: &str, folder: Option<&str>) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;
        if let Some(folder) = folder {
            Ok(sqlx::query_as!(
                Note,
                r#"SELECT id, project_id, permalink, title, file_path,
                          storage, note_type, folder, tags, content,
                          CAST(created_at AS CHAR) as "created_at!: String",
                          CAST(updated_at AS CHAR) as "updated_at!: String",
                          CAST(last_accessed AS CHAR) as "last_accessed!: String",
                          access_count, confidence, abstract as abstract_, overview,
                          scope_paths
                   FROM notes WHERE project_id = ? AND folder = ?
                   ORDER BY folder, title"#,
                project_id,
                folder,
            )
            .fetch_all(self.db.pool())
            .await?)
        } else {
            Ok(sqlx::query_as!(
                Note,
                r#"SELECT id, project_id, permalink, title, file_path,
                          storage, note_type, folder, tags, content,
                          CAST(created_at AS CHAR) as "created_at!: String",
                          CAST(updated_at AS CHAR) as "updated_at!: String",
                          CAST(last_accessed AS CHAR) as "last_accessed!: String",
                          access_count, confidence, abstract as abstract_, overview,
                          scope_paths
                   FROM notes WHERE project_id = ?
                   ORDER BY folder, title"#,
                project_id,
            )
            .fetch_all(self.db.pool())
            .await?)
        }
    }

    pub async fn update(&self, id: &str, title: &str, content: &str, tags: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("note not found: {id}")))?;

        let id = id.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let tags = tags.to_owned();
        let permalink = current.permalink.clone();

        // See `move_note` for the retry rationale: Dolt surfaces 1213
        // serialization-failures when this tx races background note/link
        // writers.
        const MAX_TX_RETRIES: usize = 3;
        let mut attempt: usize = 0;
        let note: Note = loop {
            let mut tx = self.db.pool().begin().await?;
            let content_hash = note_content_hash(&content);

            let stage = async {
                sqlx::query!(
                    "UPDATE notes SET
                        title   = ?,
                        content = ?,
                        tags    = ?,
                        content_hash = ?,
                        updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                     WHERE id = ?",
                    title,
                    content,
                    tags,
                    content_hash,
                    id
                )
                .execute(&mut *tx)
                .await?;

                index_links_for_note(&mut tx, &id, &current.project_id, &content).await?;
                resolve_links_for_note(&mut tx, &id, &title, &permalink, &current.project_id)
                    .await?;

                let note: Note = note_select_where_id!(id).fetch_one(&mut *tx).await?;
                tx.commit().await?;
                Ok::<_, crate::Error>(note)
            };

            match stage.await {
                Ok(note) => break note,
                Err(err) if attempt + 1 < MAX_TX_RETRIES && is_serialization_failure(&err) => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        10u64.saturating_mul(1u64 << attempt),
                    ))
                    .await;
                    continue;
                }
                Err(err) => return Err(err),
            }
        };

        self.spawn_note_embedding_sync(&note);
        self.events.send(djinn_memory::events::note_updated(&note));
        Ok(note)
    }

    pub async fn update_summaries(
        &self,
        id: &str,
        abstract_summary: Option<&str>,
        overview: Option<&str>,
    ) -> Result<Note> {
        self.db.ensure_initialized().await?;
        let id = id.to_owned();
        let mut tx = self.db.pool().begin().await?;

        sqlx::query!(
            "UPDATE notes SET
                `abstract` = ?,
                overview = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
            abstract_summary,
            overview,
            id
        )
        .execute(&mut *tx)
        .await?;

        let note: Note = note_select_where_id!(id).fetch_one(&mut *tx).await?;

        tx.commit().await?;
        self.events.send(djinn_memory::events::note_updated(&note));
        Ok(note)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;

        // Confirm the note exists; emit `note_deleted` only if it did so the
        // delete remains idempotent without firing duplicate events.
        let _ = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("note not found: {id}")))?;

        let id_owned = id.to_owned();
        let id_for_event = id.to_owned();

        // Dolt's commit machinery surfaces 1213 on the single-statement
        // autocommit DELETE when another writer commits to an overlapping
        // branch at the same moment. Retry the DELETE (idempotent) before
        // giving up.
        crate::retry::retry_on_serialization_failure(
            crate::retry::DEFAULT_MAX_TX_RETRIES,
            || {
                let id_owned = id_owned.clone();
                async move {
                    sqlx::query!("DELETE FROM notes WHERE id = ?", id_owned)
                        .execute(self.db.pool())
                        .await?;
                    Ok::<_, crate::Error>(())
                }
            },
        )
        .await?;

        if let Err(error) = self.delete_embedding(&id_owned).await {
            tracing::warn!(note_id = %id_owned, %error, "failed to delete note embedding during note removal");
        }

        self.events
            .send(djinn_memory::events::note_deleted(&id_for_event));
        Ok(())
    }

    pub async fn touch_accessed(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let note = self
            .get_summary_state(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("note not found: {id}")))?;

        sqlx::query!(
            "UPDATE notes SET
                last_accessed = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'),
                access_count = access_count + 1
             WHERE id = ?",
            id
        )
        .execute(self.db.pool())
        .await?;

        if note.abstract_.is_none() || note.overview.is_none() {
            self.events
                .send(djinn_memory::events::note_missing_summary(&note));
        }

        Ok(())
    }

    pub async fn move_note(
        &self,
        id: &str,
        _project_path: &std::path::Path,
        new_title: &str,
        new_note_type: &str,
    ) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("note not found: {id}")))?;

        let new_permalink = permalink_for(new_note_type, new_title);
        let new_folder = folder_for_type(new_note_type).to_owned();

        // The move_note transaction touches `notes` + `note_links` which
        // other writers (indexers, link resolvers kicked off by events) may
        // also be modifying. On Dolt we observe occasional 1213
        // serialization-failures (40001) when those windows overlap; retry
        // the transaction a few times before surfacing the error.
        const MAX_TX_RETRIES: usize = 3;
        let mut attempt: usize = 0;
        let note: Note = loop {
            let mut tx = self.db.pool().begin().await?;

            let stage = async {
                // file_path stays empty (no on-disk mirror anymore).
                sqlx::query!(
                    "UPDATE notes SET
                        title      = ?,
                        file_path  = '',
                        note_type  = ?,
                        folder     = ?,
                        permalink  = ?,
                        updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                     WHERE id = ?",
                    new_title,
                    new_note_type,
                    new_folder,
                    new_permalink,
                    id
                )
                .execute(&mut *tx)
                .await?;

                index_links_for_note(&mut tx, id, &current.project_id, &current.content).await?;
                resolve_links_for_note(&mut tx, id, new_title, &new_permalink, &current.project_id)
                    .await?;

                let note: Note = note_select_where_id!(id).fetch_one(&mut *tx).await?;
                tx.commit().await?;
                Ok::<_, crate::Error>(note)
            };

            match stage.await {
                Ok(note) => break note,
                Err(err) if attempt + 1 < MAX_TX_RETRIES && is_serialization_failure(&err) => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        10u64.saturating_mul(1u64 << attempt),
                    ))
                    .await;
                    continue;
                }
                Err(err) => return Err(err),
            }
        };

        self.spawn_note_embedding_sync(&note);
        self.events.send(djinn_memory::events::note_updated(&note));
        Ok(note)
    }

    /// Schedule embedding generation on a background tokio task.
    ///
    /// The MCP write path used to await `sync_note_embedding` inline,
    /// blocking the response on the embedding-provider round-trip
    /// (sometimes seconds). Move to a background task — embeddings catching
    /// up async is fine; lexical search still works without them.
    fn spawn_note_embedding_sync(&self, note: &Note) {
        if self.embedding_provider().is_none() {
            return;
        }
        let repo = self.clone();
        let note_id = note.id.clone();
        let title = note.title.clone();
        let note_type = note.note_type.clone();
        let tags = note.tags.clone();
        let content = note.content.clone();
        tokio::spawn(async move {
            repo.sync_note_embedding(&note_id, &title, &note_type, &tags, &content)
                .await;
        });
    }
}
