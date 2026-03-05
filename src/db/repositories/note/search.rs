use super::*;

impl NoteRepository {
    /// Full-text search with BM25 ranking and content snippets.
    ///
    /// `query` is an FTS5 query string (e.g. `"rust database"`).
    /// Results are ordered by relevance (best match first).
    pub async fn search(
        &self,
        project_id: &str,
        query: &str,
        folder: Option<&str>,
        note_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<NoteSearchResult>> {
        self.db.ensure_initialized().await?;

        let folder = folder.unwrap_or("");
        let note_type = note_type.unwrap_or("");
        let limit = limit as i64;

        let rows = sqlx::query_as::<_, (String, String, String, String, String, String)>(
            "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,
                    snippet(notes_fts, 1, '<b>', '</b>', '...', 32)
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.project_id = ?2
               AND (?3 = '' OR n.folder = ?3)
               AND (?4 = '' OR n.note_type = ?4)
             ORDER BY bm25(notes_fts)
             LIMIT ?5",
        )
        .bind(query)
        .bind(project_id)
        .bind(folder)
        .bind(note_type)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, permalink, title, folder, note_type, snippet)| NoteSearchResult {
                    id,
                    permalink,
                    title,
                    folder,
                    note_type,
                    snippet,
                },
            )
            .collect())
    }

    /// Generate a markdown catalog (table of contents) for all notes in a
    /// project, grouped by folder and sorted alphabetically within each.
    pub async fn catalog(&self, project_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;

        let notes = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT folder, title, permalink, updated_at
             FROM notes WHERE project_id = ?1
             ORDER BY folder, title",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        Ok(build_catalog(&notes))
    }

    /// List recently updated notes for a project, ordered by `updated_at` descending.
    ///
    /// `hours` limits to notes updated within the last N hours (0 = no limit).
    pub async fn recent(
        &self,
        project_id: &str,
        hours: i64,
        limit: i64,
    ) -> Result<Vec<NoteCompact>> {
        self.db.ensure_initialized().await?;

        let sql = if hours > 0 {
            format!(
                "SELECT id, permalink, title, note_type, folder, updated_at
                 FROM notes
                 WHERE project_id = ?1
                   AND updated_at >= datetime('now', '-{hours} hours')
                 ORDER BY updated_at DESC LIMIT ?2"
            )
        } else {
            "SELECT id, permalink, title, note_type, folder, updated_at
             FROM notes WHERE project_id = ?1
             ORDER BY updated_at DESC LIMIT ?2"
                .to_owned()
        };

        Ok(sqlx::query_as::<_, NoteCompact>(&sql)
            .bind(project_id)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?)
    }

    /// List compact note summaries in a folder with optional depth control.
    ///
    /// `depth`: 1 = exact folder only; 0 = all descendants.
    pub async fn list_compact(
        &self,
        project_id: &str,
        folder: &str,
        depth: i64,
    ) -> Result<Vec<NoteCompact>> {
        self.db.ensure_initialized().await?;

        let sql = if depth == 1 {
            "SELECT id, permalink, title, note_type, folder, updated_at
             FROM notes WHERE project_id = ?1 AND folder = ?2
             ORDER BY folder, title"
                .to_owned()
        } else {
            // depth=0 or depth>1: return all descendants
            "SELECT id, permalink, title, note_type, folder, updated_at
             FROM notes WHERE project_id = ?1
               AND (folder = ?2 OR folder LIKE ?2 || '/%')
             ORDER BY folder, title"
                .to_owned()
        };

        Ok(sqlx::query_as::<_, NoteCompact>(&sql)
            .bind(project_id)
            .bind(folder)
            .fetch_all(self.db.pool())
            .await?)
    }

    /// Find tasks whose `memory_refs` JSON array contains `permalink`.
    ///
    /// Returns minimal task info: `(id, short_id, title, status)`.
    pub async fn task_refs(&self, permalink: &str) -> Result<Vec<serde_json::Value>> {
        self.db.ensure_initialized().await?;

        let pattern = format!("%\"{permalink}\"%");

        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT id, short_id, title, status FROM tasks
             WHERE memory_refs LIKE ?1
             ORDER BY priority, created_at",
        )
        .bind(&pattern)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, short_id, title, status)| {
                serde_json::json!({
                    "id": id,
                    "short_id": short_id,
                    "title": title,
                    "status": status,
                })
            })
            .collect())
    }
}
