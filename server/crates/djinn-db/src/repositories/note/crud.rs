use super::*;
use crate::note_hash::note_content_hash;
use std::fs;

struct CreateNoteParams<'a> {
    project_id: &'a str,
    project_path: Option<&'a Path>,
    title: &'a str,
    content: &'a str,
    note_type: &'a str,
    status: Option<&'a str>,
    tags: &'a str,
    storage: &'a str,
    scope_paths: &'a str,
}

#[derive(Debug)]
struct ParsedWorktreeNote {
    title: String,
    note_type: String,
    tags: String,
    content: String,
}

fn collect_markdown_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir)
        .map_err(|e| Error::InvalidData(format!("read_dir {}: {e}", dir.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| Error::InvalidData(format!("read_dir {}: {e}", dir.display())))?;
        let path = entry.path();
        if path.is_dir() {
            collect_markdown_files(&path, out)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

fn parse_worktree_note_file(
    worktree_root: &Path,
    file_path: &Path,
) -> Result<Option<ParsedWorktreeNote>> {
    let raw = fs::read_to_string(file_path)
        .map_err(|e| Error::InvalidData(format!("read note file {}: {e}", file_path.display())))?;
    let notes_root = worktree_root.join(".djinn");
    let relative = file_path
        .strip_prefix(&notes_root)
        .map_err(|e| Error::InvalidData(format!("strip_prefix {}: {e}", file_path.display())))?;

    let (frontmatter, body) = split_frontmatter(&raw);
    let note_type = frontmatter
        .and_then(|fm| frontmatter_value(fm, "type"))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| infer_note_type_from_relative_path(relative));
    let permalink = permalink_from_relative_path(relative);
    let title = frontmatter
        .and_then(|fm| frontmatter_value(fm, "title"))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| title_from_permalink(&permalink));
    let tags = frontmatter
        .and_then(|fm| frontmatter_value(fm, "tags"))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "[]".to_string());

    if title.is_empty() || note_type.is_empty() {
        return Ok(None);
    }

    Ok(Some(ParsedWorktreeNote {
        title,
        note_type,
        tags,
        content: body.to_string(),
    }))
}

fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    if let Some(rest) = raw.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        let frontmatter = &rest[..end];
        let body = rest[end + 5..]
            .strip_prefix('\n')
            .unwrap_or(&rest[end + 5..]);
        return (Some(frontmatter), body);
    }

    (None, raw)
}

fn frontmatter_value(frontmatter: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}: ");
    frontmatter.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(|value| value.trim().to_string())
    })
}

fn infer_note_type_from_relative_path(relative_path: &Path) -> String {
    let permalink = permalink_from_relative_path(relative_path);
    match permalink
        .rsplit_once('/')
        .map(|(folder, _)| folder)
        .unwrap_or_default()
    {
        "decisions/proposed" => "proposed_adr",
        "decisions" => "adr",
        "patterns" => "pattern",
        "cases" => "case",
        "pitfalls" => "pitfall",
        "research" => "research",
        "research/competitive" => "competitive",
        "research/technical" => "tech_spike",
        "requirements" => "requirement",
        "reference" => "reference",
        "design" => "design",
        "design/personas" => "persona",
        "design/journeys" => "journey",
        "design/specs" => "design_spec",
        "reference/repo-maps" => "repo_map",
        _ if permalink == "brief" => "brief",
        _ if permalink == "roadmap" => "roadmap",
        _ => "reference",
    }
    .to_string()
}

fn permalink_from_relative_path(relative_path: &Path) -> String {
    relative_path
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches(".md")
        .to_string()
}

fn title_from_permalink(permalink: &str) -> String {
    let slug = permalink.rsplit('/').next().unwrap_or(permalink);
    slug.split('-')
        .filter(|part| !part.is_empty())
        .map(capitalize_first)
        .collect::<Vec<_>>()
        .join(" ")
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
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
            status: None,
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
            status: None,
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
            status: None,
            tags,
            storage: "db",
            scope_paths,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn file_with_status(
        project_id: &'a str,
        project_path: Option<&'a Path>,
        title: &'a str,
        content: &'a str,
        note_type: &'a str,
        status: Option<&'a str>,
        tags: &'a str,
        scope_paths: &'a str,
    ) -> Self {
        Self {
            project_id,
            project_path,
            title,
            content,
            note_type,
            status,
            tags,
            storage: "file",
            scope_paths,
        }
    }
}

/// Returns `true` for note types whose `storage="db"` rows should remain
/// db-only even after edits. These are the types whose db-backed rows are
/// legitimately produced by the consolidation pipeline (see
/// `consolidation.rs`) and must not be auto-promoted to file storage.
pub(super) fn db_only_consolidation_type(note_type: &str) -> bool {
    matches!(note_type, "case" | "pattern" | "pitfall")
}

impl NoteRepository {
    pub async fn diff_worktree_notes_against_canonical(
        &self,
        project_id: &str,
        project_path: &Path,
        worktree_root: &Path,
    ) -> Result<Vec<WorktreeNoteDiff>> {
        self.db.ensure_initialized().await?;

        let notes_root = worktree_root.join(".djinn");
        if !notes_root.exists() {
            return Ok(Vec::new());
        }

        let mut markdown_files = Vec::new();
        collect_markdown_files(&notes_root, &mut markdown_files)?;

        let mut diffs = Vec::new();
        for file_path in markdown_files {
            let Some(parsed) = parse_worktree_note_file(worktree_root, &file_path)? else {
                continue;
            };

            let permalink = permalink_for(&parsed.note_type, &parsed.title);
            let canonical = self.get_by_permalink(project_id, &permalink).await?;
            let canonical_file_exists = canonical
                .as_ref()
                .map(|note| !note.file_path.is_empty() && Path::new(&note.file_path).exists())
                .unwrap_or_else(|| {
                    file_path_for(project_path, &parsed.note_type, &parsed.title).exists()
                });

            let (change_kind, canonical_note_id) = match canonical {
                Some(existing) => {
                    let unchanged = existing.title == parsed.title
                        && existing.content == parsed.content
                        && existing.tags == parsed.tags
                        && canonical_file_exists;
                    (
                        if unchanged {
                            WorktreeNoteChangeKind::Unchanged
                        } else {
                            WorktreeNoteChangeKind::Modified
                        },
                        Some(existing.id),
                    )
                }
                None => (WorktreeNoteChangeKind::Added, None),
            };

            diffs.push(WorktreeNoteDiff {
                permalink,
                title: parsed.title,
                note_type: parsed.note_type,
                tags: parsed.tags,
                change_kind,
                canonical_note_id,
                canonical_file_exists,
            });
        }

        diffs.sort_by(|a, b| a.permalink.cmp(&b.permalink));
        Ok(diffs)
    }

    pub async fn sync_worktree_notes_to_canonical(
        &self,
        project_id: &str,
        project_path: &Path,
        worktree_root: &Path,
    ) -> Result<usize> {
        self.db.ensure_initialized().await?;

        let notes_root = worktree_root.join(".djinn");
        if !notes_root.exists() {
            return Ok(0);
        }

        let mut markdown_files = Vec::new();
        collect_markdown_files(&notes_root, &mut markdown_files)?;

        let canonical_repo = Self::new(self.db.clone(), self.events.clone());
        let mut synced = 0usize;

        for file_path in markdown_files {
            let Some(parsed) = parse_worktree_note_file(worktree_root, &file_path)? else {
                continue;
            };

            let permalink = permalink_for(&parsed.note_type, &parsed.title);
            match canonical_repo
                .get_by_permalink(project_id, &permalink)
                .await?
            {
                Some(existing) => {
                    let expected_file_path =
                        file_path_for(project_path, &parsed.note_type, &parsed.title)
                            .to_string_lossy()
                            .to_string();
                    if existing.title != parsed.title
                        || existing.content != parsed.content
                        || existing.tags != parsed.tags
                        || existing.file_path != expected_file_path
                        || !Path::new(&existing.file_path).exists()
                    {
                        canonical_repo
                            .update(&existing.id, &parsed.title, &parsed.content, &parsed.tags)
                            .await?;
                        synced += 1;
                    }
                }
                None => {
                    canonical_repo
                        .create(
                            project_id,
                            project_path,
                            &parsed.title,
                            &parsed.content,
                            &parsed.note_type,
                            &parsed.tags,
                        )
                        .await?;
                    synced += 1;
                }
            }
        }

        Ok(synced)
    }

    pub async fn delete_worktree_notes_from_canonical(
        &self,
        project_id: &str,
        worktree_root: &Path,
    ) -> Result<usize> {
        self.db.ensure_initialized().await?;

        let notes_root = worktree_root.join(".djinn");
        if !notes_root.exists() {
            return Ok(0);
        }

        let mut markdown_files = Vec::new();
        collect_markdown_files(&notes_root, &mut markdown_files)?;

        let mut deleted = 0usize;
        for file_path in markdown_files {
            let Some(parsed) = parse_worktree_note_file(worktree_root, &file_path)? else {
                continue;
            };

            let permalink = permalink_for(&parsed.note_type, &parsed.title);
            if let Some(existing) = self.get_by_permalink(project_id, &permalink).await? {
                self.delete(&existing.id).await?;
                deleted += 1;
            }
        }

        Ok(deleted)
    }

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
                scope_paths = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(&scope_paths)
        .bind(&id)
        .execute(self.db.pool())
        .await?;

        let note = note_select_where_id!(id)
            .fetch_one(self.db.pool())
            .await?;

        self.events.send(DjinnEventEnvelope::note_updated(&note));
        Ok(note)
    }

    pub async fn update_tags(&self, id: &str, tags: &str) -> Result<Note> {
        self.db.ensure_initialized().await?;

        let id = id.to_owned();
        let tags = tags.to_owned();

        sqlx::query(
            "UPDATE notes SET
                tags = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(&tags)
        .bind(&id)
        .execute(self.db.pool())
        .await?;

        let note = note_select_where_id!(id)
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
             VALUES (?, ?, ?, ?, '', 'db', ?, ?, ?, ?, ?, ?)",
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

        let note = note_select_where_id!(id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;
        self.sync_note_embedding(
            &note.id,
            &note.title,
            &note.note_type,
            &note.tags,
            &note.content,
        )
        .await;
        self.events.send(DjinnEventEnvelope::note_created(&note));
        Ok(note)
    }

    fn note_file_path(
        &self,
        project_path: &Path,
        note_type: &str,
        title: &str,
        status: Option<&str>,
    ) -> PathBuf {
        let root = self.worktree_root.as_deref().unwrap_or(project_path);
        file_path_for_with_status(root, note_type, title, status)
    }

    fn existing_note_file_path(&self, current: &Note) -> PathBuf {
        if let Some(worktree_root) = self.worktree_root.as_deref() {
            file_path_for(worktree_root, &current.note_type, &current.title)
        } else {
            PathBuf::from(&current.file_path)
        }
    }

    fn should_write_canonical_with_mirror(&self, current: &Note) -> bool {
        self.worktree_root.is_some()
            && (is_singleton(&current.note_type) || Path::new(&current.file_path).exists())
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

    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_status(
        &self,
        project_id: &str,
        project_path: &Path,
        title: &str,
        content: &str,
        note_type: &str,
        status: Option<&str>,
        tags: &str,
    ) -> Result<Note> {
        self.create_internal(CreateNoteParams::file_with_status(
            project_id,
            Some(project_path),
            title,
            content,
            note_type,
            status,
            tags,
            "[]",
        ))
        .await
    }

    /// File-backed create that also persists `scope_paths` on the new row.
    ///
    /// Unlike [`create_db_note_with_scope`], this routes through the file
    /// storage branch so the markdown file is written to disk and the DB
    /// row carries a valid `file_path`. Used by the MCP write entry point
    /// when the caller supplies `scope_paths`.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_scope(
        &self,
        project_id: &str,
        project_path: &Path,
        title: &str,
        content: &str,
        note_type: &str,
        status: Option<&str>,
        tags: &str,
        scope_paths: &str,
    ) -> Result<Note> {
        self.create_internal(CreateNoteParams::file_with_status(
            project_id,
            Some(project_path),
            title,
            content,
            note_type,
            status,
            tags,
            scope_paths,
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
            status,
            tags,
            storage,
            scope_paths,
        } = params;

        let id = uuid::Uuid::now_v7().to_string();
        let permalink = permalink_for_with_status(note_type, title, status);
        let file_path = project_path
            .map(|project_path| self.note_file_path(project_path, note_type, title, status));
        let file_path_str = project_path
            .map(|project_path| {
                file_path_for_with_status(project_path, note_type, title, status)
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
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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

            let note = note_select_where_id!(id)
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
                        (
                            worktree_file_path.unwrap_or(canonical_file_path.as_path()),
                            None,
                        )
                    };
                self.remove_note_files(primary_file_path, mirror_file_path);
            }
        })?;

        self.sync_note_embedding(
            &note.id,
            &note.title,
            &note.note_type,
            &note.tags,
            &note.content,
        )
        .await;
        self.events.send(DjinnEventEnvelope::note_created(&note));
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

        // Heal-on-edit: if a pre-fix broken row has storage="db" but the note
        // type is one that *should* have a file on disk (i.e. not a
        // consolidation-owned db-only type), upgrade it to file storage now.
        // This recovers ADRs and other file-backed notes that were accidentally
        // created as db-only by the old MCP write path.
        let heal_candidate = current.storage == "db"
            && !db_only_consolidation_type(&current.note_type)
            && !is_singleton(&current.note_type);

        let project_path_opt: Option<String> = if heal_candidate {
            sqlx::query_scalar::<_, String>("SELECT path FROM projects WHERE id = ?")
                .bind(&current.project_id)
                .fetch_optional(self.db.pool())
                .await?
        } else {
            None
        };

        let (effective_storage, healed_file_path_str, healed_file_path): (
            String,
            Option<String>,
            Option<PathBuf>,
        ) = if heal_candidate && project_path_opt.is_some() {
            let project_path = PathBuf::from(project_path_opt.as_deref().unwrap());
            let new_path = self.note_file_path(&project_path, &current.note_type, title, None);
            let canonical_str = file_path_for(&project_path, &current.note_type, title)
                .to_string_lossy()
                .to_string();
            ("file".to_string(), Some(canonical_str), Some(new_path))
        } else {
            (current.storage.clone(), None, None)
        };

        if effective_storage == "file" {
            let canonical_file_path = if let Some(ref s) = healed_file_path_str {
                PathBuf::from(s)
            } else {
                PathBuf::from(&current.file_path)
            };
            let worktree_file_path = if let Some(ref p) = healed_file_path {
                p.clone()
            } else {
                self.existing_note_file_path(&current)
            };
            let (primary_file_path, mirror_file_path) =
                if self.should_write_canonical_with_mirror(&current) {
                    (
                        canonical_file_path.as_path(),
                        Some(worktree_file_path.as_path()),
                    )
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

        if let Some(file_path_str) = healed_file_path_str.as_ref() {
            sqlx::query(
                "UPDATE notes SET
                    title   = ?,
                    content = ?,
                    tags    = ?,
                    content_hash = ?,
                    storage = 'file',
                    file_path = ?,
                    updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                 WHERE id = ?",
            )
            .bind(&title)
            .bind(&content)
            .bind(&tags)
            .bind(&content_hash)
            .bind(file_path_str)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                "UPDATE notes SET
                    title   = ?,
                    content = ?,
                    tags    = ?,
                    content_hash = ?,
                    updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
                 WHERE id = ?",
            )
            .bind(&title)
            .bind(&content)
            .bind(&tags)
            .bind(&content_hash)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        }

        index_links_for_note(&mut tx, &id, &current.project_id, &content).await?;
        resolve_links_for_note(&mut tx, &id, &title, &permalink, &current.project_id).await?;

        let note: Note = note_select_where_id!(id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;

        self.sync_note_embedding(
            &note.id,
            &note.title,
            &note.note_type,
            &note.tags,
            &note.content,
        )
        .await;
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
                abstract = ?,
                overview = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(abstract_summary)
        .bind(overview)
        .bind(&id)
        .execute(&mut *tx)
        .await?;

        let note: Note = note_select_where_id!(id)
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

        sqlx::query("DELETE FROM notes WHERE id = ?")
            .bind(&id_owned)
            .execute(self.db.pool())
            .await?;

        if let Err(error) = self.delete_embedding(&id_owned).await {
            tracing::warn!(note_id = %id_owned, %error, "failed to delete note embedding during note removal");
        }

        if current.storage == "file" {
            let canonical_file_path = PathBuf::from(&current.file_path);
            let worktree_file_path = self.existing_note_file_path(&current);
            let (primary_file_path, mirror_file_path) =
                if self.should_write_canonical_with_mirror(&current) {
                    (
                        canonical_file_path.as_path(),
                        Some(worktree_file_path.as_path()),
                    )
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
                last_accessed = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'),
                access_count = access_count + 1
             WHERE id = ?",
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
        let new_file_path = self.note_file_path(project_path, new_note_type, new_title, None);
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
                title      = ?,
                file_path  = ?,
                note_type  = ?,
                folder     = ?,
                permalink  = ?,
                updated_at = DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')
             WHERE id = ?",
        )
        .bind(new_title)
        .bind(&new_file_path_str)
        .bind(new_note_type)
        .bind(&new_folder)
        .bind(&new_permalink)
        .bind(id)
        .execute(&mut *tx)
        .await?;

        index_links_for_note(&mut tx, id, &current.project_id, &current.content).await?;
        resolve_links_for_note(&mut tx, id, new_title, &new_permalink, &current.project_id).await?;

        let note: Note = note_select_where_id!(id)
            .fetch_one(&mut *tx)
            .await?;

        tx.commit().await?;

        self.sync_note_embedding(
            &note.id,
            &note.title,
            &note.note_type,
            &note.tags,
            &note.content,
        )
        .await;
        self.events.send(DjinnEventEnvelope::note_updated(&note));
        Ok(note)
    }
}
