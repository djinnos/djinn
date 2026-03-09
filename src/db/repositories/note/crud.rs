use super::*;

impl NoteRepository {
    /// Create a new note. Writes the markdown file then inserts the index row.
    ///
    /// `project_path` is the root directory of the project (the `.djinn/`
    /// subdirectory is created automatically). `tags` must be a JSON array
    /// string, e.g. `'["rust","db"]'`.
    ///
    /// For singleton types (`brief`, `roadmap`) the note is inserted with
    /// a fixed permalink and file path. If the singleton already exists in the
    /// DB the caller must use `update` instead (the DB UNIQUE constraint will
    /// surface the conflict as an error).
    pub async fn create(
        &self,
        project_id: &str,
        project_path: &Path,
        title: &str,
        content: &str,
        note_type: &str,
        tags: &str,
    ) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = uuid::Uuid::now_v7().to_string();
        let permalink = permalink_for(note_type, title);
        let file_path = file_path_for(project_path, note_type, title);
        let file_path_str = file_path.to_string_lossy().to_string();

        let project_id = project_id.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let note_type = note_type.to_owned();
        let folder = folder_for_type(&note_type).to_owned();
        let tags = tags.to_owned();

        // Write file to disk before inserting into DB. Directory creation is
        // attempted here; a failure returns an error before touching the DB.
        write_note_file(&file_path, &title, &note_type, &tags, &content)?;

        let note_result: Result<Note> = async {
            let mut tx = self.db.pool().begin().await?;

            sqlx::query(
                "INSERT INTO notes
                    (id, project_id, permalink, title, file_path,
                     note_type, folder, tags, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .bind(&id)
            .bind(&project_id)
            .bind(&permalink)
            .bind(&title)
            .bind(&file_path_str)
            .bind(&note_type)
            .bind(&folder)
            .bind(&tags)
            .bind(&content)
            .execute(&mut *tx)
            .await?;

            index_links_for_note(&mut tx, &id, &project_id, &content).await?;
            resolve_links_for_note(&mut tx, &id, &title, &permalink, &project_id).await?;

            let note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
                .bind(&id)
                .fetch_one(&mut *tx)
                .await?;

            tx.commit().await?;
            Ok(note)
        }
        .await;

        let note = note_result.inspect_err(|_e| {
            // Best-effort cleanup: remove file if DB insert failed.
            let _ = std::fs::remove_file(&file_path);
        })?;

        let _ = self.events.send(DjinnEvent::NoteCreated(note.clone()));
        Ok(note)
    }

    pub async fn get(&self, id: &str) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
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
            "SELECT id, project_id, permalink, title, file_path,
                        note_type, folder, tags, content,
                        created_at, updated_at, last_accessed
                 FROM notes WHERE project_id = ?1 AND permalink = ?2",
        )
        .bind(project_id)
        .bind(permalink)
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Resolve a note by permalink (primary) or title search (fallback).
    ///
    /// This is the canonical way to look up a note when the caller has a
    /// human-supplied identifier that could be either a permalink slug or a
    /// (partial) title.
    pub async fn resolve(&self, project_id: &str, identifier: &str) -> Result<Option<Note>> {
        if let Some(n) = self.get_by_permalink(project_id, identifier).await? {
            return Ok(Some(n));
        }
        // Fallback: title search, take best match.
        let results = self.search(project_id, identifier, None, None, 1).await?;
        if let Some(r) = results.into_iter().next() {
            return self.get(&r.id).await;
        }
        Ok(None)
    }

    /// List notes for a project, optionally filtered by folder.
    pub async fn list(&self, project_id: &str, folder: Option<&str>) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;
        if let Some(folder) = folder {
            Ok(sqlx::query_as::<_, Note>(
                "SELECT id, project_id, permalink, title, file_path,
                            note_type, folder, tags, content,
                            created_at, updated_at, last_accessed
                     FROM notes WHERE project_id = ?1 AND folder = ?2
                     ORDER BY folder, title",
            )
            .bind(project_id)
            .bind(folder)
            .fetch_all(self.db.pool())
            .await?)
        } else {
            Ok(sqlx::query_as::<_, Note>(
                "SELECT id, project_id, permalink, title, file_path,
                            note_type, folder, tags, content,
                            created_at, updated_at, last_accessed
                     FROM notes WHERE project_id = ?1
                     ORDER BY folder, title",
            )
            .bind(project_id)
            .fetch_all(self.db.pool())
            .await?)
        }
    }

    /// Update a note's title, content, and tags. The file is overwritten
    /// in-place (file_path and permalink stay fixed after creation).
    pub async fn update(&self, id: &str, title: &str, content: &str, tags: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        // Fetch current note to get file_path and note_type.
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::Internal(format!("note not found: {id}")))?;

        write_note_file(
            Path::new(&current.file_path),
            title,
            &current.note_type,
            tags,
            content,
        )?;

        let id = id.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let tags = tags.to_owned();

        let permalink = current.permalink.clone();

        let mut tx = self.db.pool().begin().await?;

        sqlx::query(
            "UPDATE notes SET
                title   = ?2,
                content = ?3,
                tags    = ?4,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(&id)
        .bind(&title)
        .bind(&content)
        .bind(&tags)
        .execute(&mut *tx)
        .await?;

        index_links_for_note(&mut tx, &id, &current.project_id, &content).await?;
        resolve_links_for_note(&mut tx, &id, &title, &permalink, &current.project_id).await?;

        let note: Note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;

        let _ = self.events.send(DjinnEvent::NoteUpdated(note.clone()));
        Ok(note)
    }

    /// Delete a note. Removes the DB row (and FTS5 index via trigger) first,
    /// then attempts to remove the file from disk.
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;

        // Fetch file_path before deleting from DB.
        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::Internal(format!("note not found: {id}")))?;

        let id_owned = id.to_owned();
        let id_for_event = id.to_owned();

        sqlx::query("DELETE FROM notes WHERE id = ?1")
            .bind(&id_owned)
            .execute(self.db.pool())
            .await?;

        // Best-effort file removal — don't fail if file is already gone.
        let _ = std::fs::remove_file(&current.file_path);

        let _ = self
            .events
            .send(DjinnEvent::NoteDeleted { id: id_for_event });
        Ok(())
    }

    /// Update `last_accessed` without emitting a change event (read-access
    /// tracking should not flood the SSE stream).
    pub async fn touch_accessed(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "UPDATE notes SET
                last_accessed = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Move a note to a new location (rename file, update permalink and folder).
    ///
    /// The new position is described by `new_note_type` + `new_title`. If the type
    /// changes the file moves to the new type's folder. The content and tags are
    /// preserved unchanged.
    pub async fn move_note(
        &self,
        id: &str,
        project_path: &Path,
        new_title: &str,
        new_note_type: &str,
    ) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::Internal(format!("note not found: {id}")))?;

        let new_file_path = file_path_for(project_path, new_note_type, new_title);
        let new_permalink = permalink_for(new_note_type, new_title);
        let new_folder = folder_for_type(new_note_type).to_owned();
        let new_file_path_str = new_file_path.to_string_lossy().to_string();

        // Create destination directory and rename the file.
        if let Some(parent) = new_file_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::Internal(format!("create_dir_all {}: {e}", parent.display()))
            })?;
        }
        std::fs::rename(&current.file_path, &new_file_path).map_err(|e| {
            Error::Internal(format!(
                "rename {} → {}: {e}",
                current.file_path,
                new_file_path.display()
            ))
        })?;
        // Rewrite frontmatter to reflect new title/type.
        write_note_file(
            &new_file_path,
            new_title,
            new_note_type,
            &current.tags,
            &current.content,
        )?;

        let mut tx = self.db.pool().begin().await?;

        sqlx::query(
            "UPDATE notes SET
                title      = ?2,
                file_path  = ?3,
                note_type  = ?4,
                folder     = ?5,
                permalink  = ?6,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .bind(new_title)
        .bind(&new_file_path_str)
        .bind(new_note_type)
        .bind(&new_folder)
        .bind(&new_permalink)
        .execute(&mut *tx)
        .await?;

        // Re-resolve previously-broken links that match the new title/permalink.
        resolve_links_for_note(&mut tx, id, new_title, &new_permalink, &current.project_id).await?;

        let note: Note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;

        let _ = self.events.send(DjinnEvent::NoteUpdated(note.clone()));
        Ok(note)
    }
}
