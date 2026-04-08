use super::*;
use crate::note_hash::note_content_hash;

struct CreateNoteParams<'a> {
    project_id: &'a str,
    project_path: Option<&'a Path>,
    title: &'a str,
    content: &'a str,
    note_type: &'a str,
    tags: &'a str,
    storage: &'a str,
    scope_paths: &'a str,
}

impl<'a> CreateNoteParams<'a> {
    fn file(
        project_id: &'a str,
        project_path: Option<&'a Path>,
        title: &'a str,
        content: &'a str,
        note_type: &'a str,
        tags: &'a str,
    ) -> Self {
        Self {
            project_id,
            project_path,
            title,
            content,
            note_type,
            tags,
            storage: "file",
            scope_paths: "[]",
        }
    }

    fn db(
        project_id: &'a str,
        title: &'a str,
        content: &'a str,
        note_type: &'a str,
        tags: &'a str,
    ) -> Self {
        Self {
            project_id,
            project_path: None,
            title,
            content,
            note_type,
            tags,
            storage: "db",
            scope_paths: "[]",
        }
    }

    fn db_with_scope(
        project_id: &'a str,
        title: &'a str,
        content: &'a str,
        note_type: &'a str,
        tags: &'a str,
        scope_paths: &'a str,
    ) -> Self {
        Self {
            project_id,
            project_path: None,
            title,
            content,
            note_type,
            tags,
            storage: "db",
            scope_paths,
        }
    }
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
        self.create_internal(CreateNoteParams::db(
            project_id, title, content, note_type, tags,
        ))
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
        self.create_internal(CreateNoteParams::db_with_scope(
            project_id,
            title,
            content,
            note_type,
            tags,
            scope_paths,
        ))
        .await
    }

    pub async fn update_scope_paths(&self, id: &str, scope_paths: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = id.to_owned();
        let scope_paths = scope_paths.to_owned();

        sqlx::query(
            "UPDATE notes SET
                scope_paths = ?2,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(&id)
        .bind(&scope_paths)
        .execute(self.db.pool())
        .await?;

        let note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(self.db.pool())
            .await?;

        self.events.send(DjinnEventEnvelope::note_updated(&note));
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

        let mut tx = self.db.pool().begin().await?;

        let content_hash = note_content_hash(&content);

        sqlx::query(
            "INSERT INTO notes
                (id, project_id, permalink, title, file_path,
                 storage, note_type, folder, tags, content, content_hash, scope_paths)
             VALUES (?1, ?2, ?3, ?4, '', 'db', ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(&id)
        .bind(&project_id)
        .bind(&permalink)
        .bind(&title)
        .bind(&note_type)
        .bind(&folder)
        .bind(&tags)
        .bind(&content)
        .bind(&content_hash)
        .bind("[]")
        .execute(&mut *tx)
        .await?;

        index_links_for_note(&mut tx, &id, &project_id, &content).await?;
        resolve_links_for_note(&mut tx, &id, &title, &permalink, &project_id).await?;

        let note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;
        self.events.send(DjinnEventEnvelope::note_created(&note));
        Ok(note)
    }

    fn note_file_path(&self, project_path: &Path, note_type: &str, title: &str) -> PathBuf {
        let root = self.worktree_root.as_deref().unwrap_or(project_path);
        file_path_for(root, note_type, title)
    }

    fn existing_note_file_path(&self, current: &Note) -> PathBuf {
        if let Some(worktree_root) = self.worktree_root.as_deref() {
            file_path_for(worktree_root, &current.note_type, &current.title)
        } else {
            PathBuf::from(&current.file_path)
        }
    }

    fn write_note_files(
        &self,
        primary_file_path: &Path,
        mirror_file_path: Option<&Path>,
        title: &str,
        note_type: &str,
        tags: &str,
        content: &str,
    ) -> Result<()> {
        write_note_file(primary_file_path, title, note_type, tags, content)?;

        if let Some(mirror_file_path) = mirror_file_path
            && mirror_file_path != primary_file_path
        {
            write_note_file(mirror_file_path, title, note_type, tags, content)?;
        }

        Ok(())
    }

    fn remove_note_files(&self, primary_file_path: &Path, mirror_file_path: Option<&Path>) {
        let _ = std::fs::remove_file(primary_file_path);

        if let Some(mirror_file_path) = mirror_file_path
            && mirror_file_path != primary_file_path
        {
            let _ = std::fs::remove_file(mirror_file_path);
        }
    }

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
        self.create_internal(CreateNoteParams::file(
            project_id,
            Some(project_path),
            title,
            content,
            note_type,
            tags,
        ))
        .await
    }

    async fn create_internal(&self, params: CreateNoteParams<'_>) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let CreateNoteParams {
            project_id,
            project_path,
            title,
            content,
            note_type,
            tags,
            storage,
            scope_paths,
        } = params;

        let id = uuid::Uuid::now_v7().to_string();
        let permalink = permalink_for(note_type, title);
        let file_path =
            project_path.map(|project_path| self.note_file_path(project_path, note_type, title));
        let file_path_str = project_path
            .map(|project_path| {
                file_path_for(project_path, note_type, title)
                    .to_string_lossy()
                    .to_string()
            })
            .unwrap_or_default();

        let project_id = project_id.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let note_type = note_type.to_owned();
        let folder = folder_for_type(&note_type).to_owned();
        let tags = tags.to_owned();
        let storage = storage.to_owned();
        let scope_paths = scope_paths.to_owned();

        if storage == "file" {
            let file_path = file_path.as_ref().ok_or_else(|| {
                Error::InvalidData("file-backed notes require a project path".to_string())
            })?;
            let canonical_file_path = PathBuf::from(&file_path_str);
            let (primary_file_path, mirror_file_path) =
                if is_singleton(&note_type) && self.worktree_root.is_some() {
                    (canonical_file_path.as_path(), Some(file_path.as_path()))
                } else {
                    (file_path.as_path(), None)
                };
            self.write_note_files(
                primary_file_path,
                mirror_file_path,
                &title,
                &note_type,
                &tags,
                &content,
            )?;
        }

        let content_hash = note_content_hash(&content);

        let note_result: Result<Note> = async {
            let mut tx = self.db.pool().begin().await?;

            sqlx::query(
                "INSERT INTO notes
                    (id, project_id, permalink, title, file_path,
                     storage, note_type, folder, tags, content, content_hash, scope_paths)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )
            .bind(&id)
            .bind(&project_id)
            .bind(&permalink)
            .bind(&title)
            .bind(&file_path_str)
            .bind(&storage)
            .bind(&note_type)
            .bind(&folder)
            .bind(&tags)
            .bind(&content)
            .bind(&content_hash)
            .bind(&scope_paths)
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
            if storage == "file" {
                let canonical_file_path = PathBuf::from(&file_path_str);
                let worktree_file_path = file_path.as_deref();
                let (primary_file_path, mirror_file_path) =
                    if is_singleton(&note_type) && self.worktree_root.is_some() {
                        (canonical_file_path.as_path(), worktree_file_path)
                    } else {
                        (worktree_file_path.unwrap_or(canonical_file_path.as_path()), None)
                    };
                self.remove_note_files(primary_file_path, mirror_file_path);
            }
        })?;

        self.events.send(DjinnEventEnvelope::note_created(&note));
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
                        storage, note_type, folder, tags, content,
                        created_at, updated_at, last_accessed,
                        access_count, confidence, abstract as abstract_, overview,
                        scope_paths
                 FROM notes WHERE project_id = ?1 AND permalink = ?2",
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
            "SELECT id, project_id, permalink, title, file_path,
                        storage, note_type, folder, tags, content,
                        created_at, updated_at, last_accessed,
                        access_count, confidence, abstract as abstract_, overview,
                        scope_paths
                 FROM notes
                 WHERE project_id = ?1 AND content_hash = ?2
                 ORDER BY created_at ASC
                 LIMIT 1",
        )
        .bind(project_id)
        .bind(content_hash)
        .fetch_optional(self.db.pool())
        .await?)
    }

    pub async fn get_summary_state(&self, id: &str) -> Result<Option<Note>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
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
        if let Some(n) = self.get_by_permalink(project_id, identifier).await? {
            return Ok(Some(n));
        }
        let results = self
            .search(project_id, identifier, None, None, None, 1)
            .await?;
        if let Some(r) = results.into_iter().next() {
            return self.get(&r.id).await;
        }
        Ok(None)
    }

    pub async fn list(&self, project_id: &str, folder: Option<&str>) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;
        if let Some(folder) = folder {
            Ok(sqlx::query_as::<_, Note>(
                "SELECT id, project_id, permalink, title, file_path,
                            storage, note_type, folder, tags, content,
                            created_at, updated_at, last_accessed,
                            access_count, confidence, abstract as abstract_, overview,
                            scope_paths
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
                            storage, note_type, folder, tags, content,
                            created_at, updated_at, last_accessed,
                            access_count, confidence, abstract as abstract_, overview,
                            scope_paths
                     FROM notes WHERE project_id = ?1
                     ORDER BY folder, title",
            )
            .bind(project_id)
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

        if current.storage == "file" {
            let canonical_file_path = PathBuf::from(&current.file_path);
            let worktree_file_path = self.existing_note_file_path(&current);
            let (primary_file_path, mirror_file_path) =
                if is_singleton(&current.note_type) && self.worktree_root.is_some() {
                    (canonical_file_path.as_path(), Some(worktree_file_path.as_path()))
                } else {
                    (worktree_file_path.as_path(), None)
                };
            self.write_note_files(
                primary_file_path,
                mirror_file_path,
                title,
                &current.note_type,
                tags,
                content,
            )?;
        }

        let id = id.to_owned();
        let title = title.to_owned();
        let content = content.to_owned();
        let tags = tags.to_owned();
        let permalink = current.permalink.clone();

        let mut tx = self.db.pool().begin().await?;

        let content_hash = note_content_hash(&content);

        sqlx::query(
            "UPDATE notes SET
                title   = ?2,
                content = ?3,
                tags    = ?4,
                content_hash = ?5,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(&id)
        .bind(&title)
        .bind(&content)
        .bind(&tags)
        .bind(&content_hash)
        .execute(&mut *tx)
        .await?;

        index_links_for_note(&mut tx, &id, &current.project_id, &content).await?;
        resolve_links_for_note(&mut tx, &id, &title, &permalink, &current.project_id).await?;

        let note: Note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;

        self.events.send(DjinnEventEnvelope::note_updated(&note));
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

        sqlx::query(
            "UPDATE notes SET
                abstract = ?2,
                overview = ?3,
                updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE id = ?1",
        )
        .bind(&id)
        .bind(abstract_summary)
        .bind(overview)
        .execute(&mut *tx)
        .await?;

        let note: Note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;
        self.events.send(DjinnEventEnvelope::note_updated(&note));
        Ok(note)
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;

        let current = self
            .get(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("note not found: {id}")))?;

        let id_owned = id.to_owned();
        let id_for_event = id.to_owned();

        sqlx::query("DELETE FROM notes WHERE id = ?1")
            .bind(&id_owned)
            .execute(self.db.pool())
            .await?;

        if current.storage == "file" {
            let canonical_file_path = PathBuf::from(&current.file_path);
            let worktree_file_path = self.existing_note_file_path(&current);
            let (primary_file_path, mirror_file_path) =
                if is_singleton(&current.note_type) && self.worktree_root.is_some() {
                    (canonical_file_path.as_path(), Some(worktree_file_path.as_path()))
                } else {
                    (worktree_file_path.as_path(), None)
                };
            self.remove_note_files(primary_file_path, mirror_file_path);
        }

        self.events
            .send(DjinnEventEnvelope::note_deleted(&id_for_event));
        Ok(())
    }

    pub async fn touch_accessed(&self, id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        let note = self
            .get_summary_state(id)
            .await?
            .ok_or_else(|| Error::InvalidData(format!("note not found: {id}")))?;

        sqlx::query(
            "UPDATE notes SET
                last_accessed = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                access_count = access_count + 1
             WHERE id = ?1",
        )
        .bind(id)
        .execute(self.db.pool())
        .await?;

        if note.abstract_.is_none() || note.overview.is_none() {
            self.events
                .send(DjinnEventEnvelope::note_missing_summary(&note));
        }

        Ok(())
    }

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
            .ok_or_else(|| Error::InvalidData(format!("note not found: {id}")))?;

        if current.storage != "file" {
            return Err(Error::InvalidData(
                "db-backed notes cannot be moved on disk".to_string(),
            ));
        }

        let current_file_path = self.existing_note_file_path(&current);
        let new_file_path = self.note_file_path(project_path, new_note_type, new_title);
        let new_permalink = permalink_for(new_note_type, new_title);
        let new_folder = folder_for_type(new_note_type).to_owned();
        let new_file_path_str = file_path_for(project_path, new_note_type, new_title)
            .to_string_lossy()
            .to_string();

        if let Some(parent) = new_file_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::InvalidData(format!("create_dir_all {}: {e}", parent.display()))
            })?;
        }
        std::fs::rename(&current_file_path, &new_file_path).map_err(|e| {
            Error::InvalidData(format!(
                "rename {} → {}: {e}",
                current_file_path.display(),
                new_file_path.display()
            ))
        })?;
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

        index_links_for_note(&mut tx, id, &current.project_id, &current.content).await?;
        resolve_links_for_note(&mut tx, id, new_title, &new_permalink, &current.project_id).await?;

        let note: Note = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;

        self.events.send(DjinnEventEnvelope::note_updated(&note));
        Ok(note)
    }
}
