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
            project_id, None, title, content, note_type, None, tags, "[]", None,
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
            None,
        )
        .await
    }

    /// `create_db_note_with_scope` plus a normalized retrieval anchor. Used by
    /// the LLM extraction path so newly written case/pattern/pitfall notes
    /// carry an objective `applies_when` sentence distinct from the body.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_db_note_with_scope_and_retrieval_anchor(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
        scope_paths: &str,
        retrieval_anchor: Option<&str>,
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
            retrieval_anchor,
        )
        .await
    }

    pub async fn update_scope_paths(&self, id: &str, scope_paths: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = id.to_owned();
        let scope_paths: serde_json::Value = serde_json::from_str(scope_paths)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.scope_paths: {e}")))?;

        sqlx::query!(
            r#"UPDATE notes SET
                scope_paths = $1,
                updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            scope_paths,
            id
        )
        .execute(self.db.pool())
        .await?;

        let note = note_select_where_id!(&id).fetch_one(self.db.pool()).await?;

        self.events.send(djinn_memory::events::note_updated(&note));
        Ok(note)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_db_note_with_permalink_and_retrieval_anchor(
        &self,
        project_id: &str,
        permalink: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
        retrieval_anchor: Option<&str>,
    ) -> Result<Note> {
        self.create_db_note_with_permalink_internal(
            project_id,
            permalink,
            title,
            content,
            note_type,
            tags,
            retrieval_anchor,
        )
        .await
    }

    pub async fn update_tags(&self, id: &str, tags: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = id.to_owned();
        let tags: serde_json::Value = serde_json::from_str(tags)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.tags: {e}")))?;

        sqlx::query!(
            r#"UPDATE notes SET
                tags = $1,
                updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            tags,
            id
        )
        .execute(self.db.pool())
        .await?;

        let note = note_select_where_id!(&id).fetch_one(self.db.pool()).await?;

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
        self.create_db_note_with_permalink_internal(
            project_id, permalink, title, content, note_type, tags, None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_db_note_with_permalink_internal(
        &self,
        project_id: &str,
        permalink: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
        retrieval_anchor: Option<&str>,
    ) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = uuid::Uuid::now_v7().to_string();
        let project_id = project_id.to_owned();
        let permalink = permalink.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let retrieval_anchor = retrieval_anchor.map(str::to_owned);
        let note_type = note_type.to_owned();
        let folder = folder_for_type(&note_type).to_owned();
        let tags = tags.to_owned();

        let content_hash = note_content_hash(&content);
        let tags_json: serde_json::Value = serde_json::from_str(&tags)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.tags: {e}")))?;
        let empty_scope: serde_json::Value = serde_json::json!([]);

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
                         storage, note_type, folder, tags, content, retrieval_anchor, content_hash, scope_paths)
                     VALUES ($1, $2, $3, $4, '', 'db', $5, $6, $7, $8, $9, $10, $11)",
                    id,
                    project_id,
                    permalink,
                    title,
                    note_type,
                    folder,
                    tags_json,
                    content,
                    retrieval_anchor,
                    content_hash,
                    empty_scope
                )
                .execute(&mut *tx)
                .await?;

                index_links_for_note(&mut tx, &id, &project_id, &content).await?;
                resolve_links_for_note(&mut tx, &id, &title, &permalink, &project_id).await?;

                let note = note_select_where_id!(&id).fetch_one(&mut *tx).await?;

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
            project_id, None, title, content, note_type, None, tags, "[]", None,
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
        self.create_internal(
            project_id, None, title, content, note_type, status, tags, "[]", None,
        )
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
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_scope_and_retrieval_anchor(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        status: Option<&str>,
        tags: &str,
        scope_paths: &str,
        retrieval_anchor: Option<&str>,
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
            retrieval_anchor,
        )
        .await
    }

    pub async fn create_with_retrieval_anchor(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
        retrieval_anchor: Option<&str>,
    ) -> Result<Note> {
        self.create_internal(
            project_id,
            None,
            title,
            content,
            note_type,
            None,
            tags,
            "[]",
            retrieval_anchor,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_status_and_retrieval_anchor(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        note_type: &str,
        status: Option<&str>,
        tags: &str,
        retrieval_anchor: Option<&str>,
    ) -> Result<Note> {
        self.create_internal(
            project_id,
            None,
            title,
            content,
            note_type,
            status,
            tags,
            "[]",
            retrieval_anchor,
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
        retrieval_anchor: Option<&str>,
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
        let retrieval_anchor = retrieval_anchor.map(str::to_owned);
        let note_type = note_type.to_owned();
        let folder = folder_for_type_with_status(&note_type, status).to_owned();
        let tags_json: serde_json::Value = serde_json::from_str(tags)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.tags: {e}")))?;
        let scope_paths_json: serde_json::Value = serde_json::from_str(scope_paths)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.scope_paths: {e}")))?;

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
                //
                // `status` is bound explicitly when the caller supplied one;
                // otherwise the column-level default (`'active'`) takes
                // effect so callers that ignore lifecycle get the legacy
                // behavior of "active by default".
                let normalized_status: Option<String> = match status {
                    Some(s) => {
                        let normalized = djinn_memory::note_status::normalize(Some(s));
                        if normalized.is_empty() {
                            None
                        } else {
                            if !djinn_memory::note_status::is_valid(&normalized) {
                                return Err(Error::InvalidData(format!(
                                    "invalid note lifecycle status: {normalized}"
                                )));
                            }
                            Some(normalized)
                        }
                    }
                    None => None,
                };
                sqlx::query!(
                    "INSERT INTO notes
                        (id, project_id, permalink, title, file_path,
                         storage, note_type, folder, status, tags, content, retrieval_anchor, content_hash, scope_paths)
                     VALUES ($1, $2, $3, $4, '', 'db', $5, $6, COALESCE($7, 'active'), $8, $9, $10, $11, $12)",
                    id,
                    project_id,
                    permalink,
                    title,
                    note_type,
                    folder,
                    normalized_status,
                    tags_json,
                    content,
                    retrieval_anchor,
                    content_hash,
                    scope_paths_json
                )
                .execute(&mut *tx)
                .await?;

                index_links_for_note(&mut tx, &id, &project_id, &content).await?;
                resolve_links_for_note(&mut tx, &id, &title, &permalink, &project_id).await?;

                let note = note_select_where_id!(&id).fetch_one(&mut *tx).await?;

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
        Ok(sqlx::query_as::<_, Note>(
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, status, tags::text AS tags, content,
                      retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                      access_count, confidence, abstract as abstract_, overview,
                      scope_paths::text AS scope_paths
               FROM notes WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_by_permalink(
        &self,
        project_id: &str,
        permalink: &str,
    ) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Note>(
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, status, tags::text AS tags, content,
                      retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                      access_count, confidence, abstract as abstract_, overview,
                      scope_paths::text AS scope_paths
               FROM notes WHERE project_id = $1 AND permalink = $2"#,
        )
        .bind(project_id)
        .bind(permalink)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn find_by_content_hash(
        &self,
        project_id: &str,
        content_hash: &str,
    ) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Note>(
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, status, tags::text AS tags, content,
                      retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                      access_count, confidence, abstract as abstract_, overview,
                      scope_paths::text AS scope_paths
               FROM notes
               WHERE project_id = $1 AND content_hash = $2
               ORDER BY created_at ASC
               LIMIT 1"#,
        )
        .bind(project_id)
        .bind(content_hash)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_summary_state(&self, id: &str) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Note>(
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, status, tags::text AS tags, content,
                      retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                      access_count, confidence, abstract as abstract_, overview,
                      scope_paths::text AS scope_paths
               FROM notes WHERE id = $1"#,
        )
        .bind(id)
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
                edge_kinds: None,
                entity_types: None,
            })
            .await?;
        if let Some(r) = results.into_iter().next() {
            return self.get(&r.id).await;
        }
        Ok(None)
    }

    pub async fn list(&self, project_id: &str, folder: Option<&str>) -> Result<Vec<Note>> {
        self.list_with_status(project_id, folder, None).await
    }

    pub async fn list_with_status(
        &self,
        project_id: &str,
        folder: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;
        let status = djinn_memory::note_status::normalize(status);
        if !djinn_memory::note_status::is_valid(&status) {
            return Err(Error::InvalidData(format!(
                "invalid note lifecycle status: {status}"
            )));
        }
        if let Some(folder) = folder {
            Ok(sqlx::query_as::<_, Note>(
                r#"SELECT id, project_id, permalink, title, file_path,
                          storage, note_type, folder, status, tags::text AS tags, content,
                          retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                          access_count, confidence, abstract as abstract_, overview,
                          scope_paths::text AS scope_paths
                   FROM notes WHERE project_id = $1 AND folder = $2 AND status = $3
                   ORDER BY folder, title"#,
            )
            .bind(project_id)
            .bind(folder)
            .bind(&status)
            .fetch_all(self.db.pool())
            .await?)
        } else {
            Ok(sqlx::query_as::<_, Note>(
                r#"SELECT id, project_id, permalink, title, file_path,
                          storage, note_type, folder, status, tags::text AS tags, content,
                          retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                          access_count, confidence, abstract as abstract_, overview,
                          scope_paths::text AS scope_paths
                   FROM notes WHERE project_id = $1 AND status = $2
                   ORDER BY folder, title"#,
            )
            .bind(project_id)
            .bind(&status)
            .fetch_all(self.db.pool())
            .await?)
        }
    }

    pub async fn update_status(&self, id: &str, status: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let status = djinn_memory::note_status::normalize(Some(status));
        if !djinn_memory::note_status::is_valid(&status) {
            return Err(Error::InvalidData(format!(
                "invalid note lifecycle status: {status}"
            )));
        }

        // Lock before comparing status so a concurrent transition cannot make
        // an UPDATE fallback return a row from an earlier statement snapshot.
        // The locked row is also the same-status result, without a write.
        let mut tx = self.db.pool().begin().await?;
        let current: Note = sqlx::query_as(
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, status, tags::text AS tags, content,
                      retrieval_anchor, created_at, updated_at, lifecycle_changed_at,
                      last_accessed, access_count, confidence, abstract AS abstract_,
                      overview, scope_paths::text AS scope_paths
               FROM notes WHERE id = $1 FOR UPDATE"#,
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;

        let note = if current.status == status {
            current
        } else {
            sqlx::query_as(
                r#"UPDATE notes
                   SET status = $1,
                       updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                       lifecycle_changed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                   WHERE id = $2
                   RETURNING id, project_id, permalink, title, file_path,
                             storage, note_type, folder, status, tags::text AS tags, content,
                             retrieval_anchor, created_at, updated_at, lifecycle_changed_at,
                             last_accessed, access_count, confidence, abstract AS abstract_,
                             overview, scope_paths::text AS scope_paths"#,
            )
            .bind(&status)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?
        };
        tx.commit().await?;
        self.events.send(djinn_memory::events::note_updated(&note));
        Ok(note)
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
        let tags: serde_json::Value = serde_json::from_str(tags)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.tags: {e}")))?;
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
                    r#"UPDATE notes SET
                        title   = $1,
                        content = $2,
                        tags    = $3,
                        content_hash = $4,
                        updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                     WHERE id = $5"#,
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

                let note: Note = note_select_where_id!(&id).fetch_one(&mut *tx).await?;
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

    pub async fn update_retrieval_anchor(
        &self,
        id: &str,
        retrieval_anchor: Option<&str>,
    ) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = id.to_owned();
        let retrieval_anchor = retrieval_anchor.map(str::to_owned);
        let mut tx = self.db.pool().begin().await?;

        sqlx::query!(
            r#"UPDATE notes SET
                retrieval_anchor = $1,
                updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $2"#,
            retrieval_anchor,
            id
        )
        .execute(&mut *tx)
        .await?;

        let note: Note = note_select_where_id!(&id).fetch_one(&mut *tx).await?;

        tx.commit().await?;
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
            r#"UPDATE notes SET
                abstract = $1,
                overview = $2,
                updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             WHERE id = $3"#,
            abstract_summary,
            overview,
            id
        )
        .execute(&mut *tx)
        .await?;

        let note: Note = note_select_where_id!(&id).fetch_one(&mut *tx).await?;

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
        crate::retry::retry_on_serialization_failure(crate::retry::DEFAULT_MAX_TX_RETRIES, || {
            let id_owned = id_owned.clone();
            async move {
                sqlx::query!("DELETE FROM notes WHERE id = $1", id_owned)
                    .execute(self.db.pool())
                    .await?;
                Ok::<_, crate::Error>(())
            }
        })
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
            r#"UPDATE notes SET
                last_accessed = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                access_count = access_count + 1
             WHERE id = $1"#,
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
                    r#"UPDATE notes SET
                        title      = $1,
                        file_path  = '',
                        note_type  = $2,
                        folder     = $3,
                        permalink  = $4,
                        updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                     WHERE id = $5"#,
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

                let note: Note = note_select_where_id!(&id).fetch_one(&mut *tx).await?;
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
    pub(super) fn spawn_note_embedding_sync(&self, note: &Note) {
        if self.embedding_provider().is_none() {
            return;
        }
        let repo = self.clone();
        let note_id = note.id.clone();
        let title = note.title.clone();
        let note_type = note.note_type.clone();
        let tags = note.tags.clone();
        let content = note.content.clone();
        let retrieval_anchor = note.retrieval_anchor.clone();
        tokio::spawn(async move {
            repo.sync_note_embedding(
                &note_id,
                &title,
                &note_type,
                &tags,
                &content,
                retrieval_anchor.as_deref(),
            )
            .await;
        });
    }
}
