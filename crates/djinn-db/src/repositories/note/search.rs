use super::*;
use crate::repositories::note::rrf::rrf_fuse;

/// Sanitize a user query into valid FTS5 syntax.
///
/// Strips FTS5 operators and special characters, then wraps each remaining
/// token in double-quotes so they are treated as literal terms joined by
/// implicit AND.  Returns `None` if the query contains no usable tokens.
fn sanitize_fts5_query(raw: &str) -> Option<String> {
    let tokens: Vec<&str> = raw
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| {
            let t = t.to_uppercase();
            !t.is_empty() && t != "AND" && t != "OR" && t != "NOT" && t != "NEAR"
        })
        .collect();
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .into_iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

impl NoteRepository {
    /// Full-text search with FTS candidate generation and RRF-fused ranking.
    ///
    /// `query` is a natural-language search string. It is sanitized into safe
    /// FTS5 syntax before execution.
    /// Results are ordered by relevance (best match first).
    pub async fn search(
        &self,
        project_id: &str,
        query: &str,
        task_id: Option<&str>,
        folder: Option<&str>,
        note_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<NoteSearchResult>> {
        self.db.ensure_initialized().await?;

        let Some(safe_query) = sanitize_fts5_query(query) else {
            return Ok(vec![]);
        };

        let folder = folder.unwrap_or("");
        let note_type = note_type.unwrap_or("");
        let limit = limit as i64;

        let candidate_rows = sqlx::query_as::<_, (String, f64)>(
            "SELECT n.id, bm25(notes_fts, 3.0, 1.0, 2.0) as bm25_score
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.project_id = ?2
               AND (?3 = '' OR n.folder = ?3)
               AND (?4 = '' OR n.note_type = ?4)
             ORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)
             LIMIT ?5",
        )
        .bind(&safe_query)
        .bind(project_id)
        .bind(folder)
        .bind(note_type)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await?;

        if candidate_rows.is_empty() {
            return Ok(vec![]);
        }

        let candidate_ids: Vec<String> = candidate_rows.iter().map(|(id, _)| id.clone()).collect();
        let lexical_scores: Vec<(String, f64)> = candidate_rows
            .into_iter()
            .map(|(id, bm25_score)| (id, -bm25_score))
            .collect();

        let temporal_scores = self.temporal_scores(project_id, &candidate_ids).await?;
        let graph_scores = self.graph_proximity_scores(&candidate_ids, 2).await?;
        let task_scores = self.task_affinity_scores(project_id, task_id).await?;

        let confidence_map = self.note_confidence_map(&candidate_ids).await?;

        let signals = vec![
            (lexical_scores, 60.0),
            (temporal_scores, 60.0),
            (graph_scores, 60.0),
            (task_scores, 60.0),
        ];
        let fused = rrf_fuse(&signals, &confidence_map);
        let ranked_ids: Vec<String> = fused
            .into_iter()
            .filter_map(|(id, _)| candidate_ids.contains(&id).then_some(id))
            .take(limit as usize)
            .collect();

        if ranked_ids.is_empty() {
            return Ok(vec![]);
        }

        let placeholders = std::iter::repeat_n("?", ranked_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, permalink, title, folder, note_type,
                    COALESCE(abstract, substr(content, 1, 200)) as abstract_text
             FROM notes
             WHERE project_id = ? AND id IN ({})",
            placeholders
        );
        let mut q = sqlx::query_as::<_, (String, String, String, String, String, String)>(&sql)
            .bind(project_id);
        for id in &ranked_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(self.db.pool()).await?;
        let by_id: HashMap<String, (String, String, String, String, String)> = rows
            .into_iter()
            .map(|(id, permalink, title, folder, note_type, abstract_text)| {
                (id, (permalink, title, folder, note_type, abstract_text))
            })
            .collect();

        Ok(ranked_ids
            .into_iter()
            .filter_map(|id| {
                by_id
                    .get(&id)
                    .map(
                        |(permalink, title, folder, note_type, abstract_text)| NoteSearchResult {
                            id,
                            permalink: permalink.clone(),
                            title: title.clone(),
                            folder: folder.clone(),
                            note_type: note_type.clone(),
                            snippet: abstract_text.clone(),
                        },
                    )
            })
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
        folder: Option<&str>,
        note_type: Option<&str>,
        depth: i64,
    ) -> Result<Vec<NoteCompact>> {
        self.db.ensure_initialized().await?;

        let mut sql = "SELECT id, permalink, title, note_type, folder, updated_at
             FROM notes WHERE project_id = ?1"
            .to_owned();

        let mut binds: Vec<String> = vec![project_id.to_string()];

        if let Some(f) = folder {
            let idx = binds.len() + 1;
            if depth == 1 {
                sql.push_str(&format!(" AND folder = ?{idx}"));
            } else {
                sql.push_str(&format!(
                    " AND (folder = ?{idx} OR folder LIKE ?{idx} || '/%')"
                ));
            }
            binds.push(f.to_string());
        }

        if let Some(t) = note_type {
            let idx = binds.len() + 1;
            sql.push_str(&format!(" AND note_type = ?{idx}"));
            binds.push(t.to_string());
        }

        sql.push_str(" ORDER BY folder, title");

        let mut query = sqlx::query_as::<_, NoteCompact>(&sql);
        for b in &binds {
            query = query.bind(b);
        }

        Ok(query.fetch_all(self.db.pool()).await?)
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
