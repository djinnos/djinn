use super::*;
use crate::repositories::note::rrf::rrf_fuse;
use djinn_core::models::{ContradictionCandidate, TypeRisk};

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
    /// Find same-folder, same-type near-duplicate candidates for a note before write.
    ///
    /// The lookup stays repository-local so callers do not need direct SQLx access.
    /// Results are filtered to candidates whose normalized BM25 score exceeds -3.0.
    pub async fn dedup_candidates(
        &self,
        project_id: &str,
        folder: &str,
        note_type: &str,
        text: &str,
        limit: usize,
    ) -> Result<Vec<NoteDedupCandidate>> {
        self.db.ensure_initialized().await?;

        let Some(safe_query) = sanitize_fts5_query(text) else {
            return Ok(vec![]);
        };

        let limit = limit as i64;
        let rows = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                String,
                Option<String>,
                Option<String>,
                f64,
            ),
        >(
            "SELECT n.id, n.permalink, n.title, n.folder, n.note_type, n.abstract, n.overview,
                    -bm25(notes_fts, 3.0, 1.0, 2.0) as score
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.project_id = ?2
               AND n.folder = ?3
               AND n.note_type = ?4
               AND -bm25(notes_fts, 3.0, 1.0, 2.0) > ?5
             ORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)
             LIMIT ?6",
        )
        .bind(&safe_query)
        .bind(project_id)
        .bind(folder)
        .bind(note_type)
        .bind(-3.0_f64)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, permalink, title, folder, note_type, abstract_, overview, score)| {
                    NoteDedupCandidate {
                        id,
                        permalink,
                        title,
                        folder,
                        note_type,
                        abstract_,
                        overview,
                        score,
                    }
                },
            )
            .collect())
    }

    /// Find notes that may structurally contradict a newly written note.
    ///
    /// Uses a stricter BM25 threshold (-5.0) than dedup, searches across all
    /// folders and types, excludes self, and annotates each candidate with a
    /// `TypeRisk`. Returns only High and Medium risks (Low is filtered out).
    pub async fn detect_contradiction_candidates(
        &self,
        note_id: &str,
        note_type: &str,
        folder: &str,
        text: &str,
    ) -> Result<Vec<ContradictionCandidate>> {
        self.db.ensure_initialized().await?;

        let Some(safe_query) = sanitize_fts5_query(text) else {
            return Ok(vec![]);
        };

        let rows = sqlx::query_as::<_, (String, String, String, String, String, f64)>(
            "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,
                    -bm25(notes_fts, 3.0, 1.0, 2.0) as score
             FROM notes_fts
             JOIN notes n ON notes_fts.rowid = n.rowid
             WHERE notes_fts MATCH ?1
               AND n.id != ?2
               AND -bm25(notes_fts, 3.0, 1.0, 2.0) > 5.0
             ORDER BY bm25(notes_fts, 3.0, 1.0, 2.0)
             LIMIT 3",
        )
        .bind(&safe_query)
        .bind(note_id)
        .fetch_all(self.db.pool())
        .await?;

        let candidates = rows
            .into_iter()
            .filter_map(|(id, permalink, title, cand_folder, cand_type, score)| {
                let risk = if cand_type == note_type && cand_folder == folder {
                    TypeRisk::High
                } else if cand_type == note_type {
                    TypeRisk::Medium
                } else {
                    return None; // Low risk — filter out
                };
                Some(ContradictionCandidate {
                    id,
                    permalink,
                    title,
                    folder: cand_folder,
                    note_type: cand_type,
                    score,
                    risk,
                })
            })
            .collect();

        Ok(candidates)
    }

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
        let fused_score_map: HashMap<String, f64> = fused.iter().cloned().collect();
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
                let score = fused_score_map.get(&id).copied().unwrap_or(0.0);
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
                            score,
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

#[cfg(test)]
mod contradiction_tests {
    use super::*;
    use crate::database::Database;
    use djinn_core::events::EventBus;
    use djinn_core::models::TypeRisk;

    async fn make_repo_and_project(tmp: &tempfile::TempDir) -> (NoteRepository, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO projects (id, name, path) VALUES (?1, ?2, ?3)")
            .bind(&id)
            .bind("test")
            .bind(tmp.path().to_str().unwrap())
            .execute(db.pool())
            .await
            .unwrap();
        (NoteRepository::new(db, EventBus::noop()), id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detect_candidates_same_type_and_folder_is_high_risk() {
        let tmp = crate::database::test_tempdir().unwrap();
        let (repo, project_id) = make_repo_and_project(&tmp).await;

        // Add unrelated noise notes to boost IDF so the matching pair scores > 5.0
        let noise_content = [
            "database migration schema versioning rollback strategy deployment pipeline",
            "kubernetes pod scheduling resource limits cpu memory horizontal autoscaling",
            "graphql schema stitching federation gateway resolver batching dataloader",
            "redis caching eviction policy lru ttl distributed session storage cluster",
            "webpack bundling tree shaking code splitting lazy loading module federation",
        ];
        for (i, content) in noise_content.iter().enumerate() {
            repo.create(
                &project_id,
                tmp.path(),
                &format!("Noise {i}"),
                content,
                "adr",
                "[]",
            )
            .await
            .unwrap();
        }

        // Existing pattern note with specific rare content
        let shared = "tokio_spawn_contradiction_xqz concurrent_xqz execution_xqz async_xqz \
                      rust_xqz service_xqz pattern_xqz distributed_xqz systems_xqz \
                      architectural_xqz decision_xqz record_xqz implementation_xqz guide_xqz";
        let existing = repo
            .create(
                &project_id,
                tmp.path(),
                "Existing Pattern",
                shared,
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        // New note with identical content — should be detected
        let new_note = repo
            .create(
                &project_id,
                tmp.path(),
                "New Pattern",
                shared,
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let candidates = repo
            .detect_contradiction_candidates(&new_note.id, "pattern", "patterns", shared)
            .await
            .unwrap();

        assert!(
            candidates.iter().any(|c| c.id == existing.id),
            "existing note should be a candidate"
        );
        let cand = candidates.iter().find(|c| c.id == existing.id).unwrap();
        assert_eq!(
            cand.risk,
            TypeRisk::High,
            "same type+folder should be High risk"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detect_candidates_excludes_self() {
        let tmp = crate::database::test_tempdir().unwrap();
        let (repo, project_id) = make_repo_and_project(&tmp).await;

        let note = repo
            .create(
                &project_id,
                tmp.path(),
                "Solo Note",
                "unique content about tokio spawn concurrent execution patterns rust async",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let candidates = repo
            .detect_contradiction_candidates(
                &note.id,
                "pattern",
                "patterns",
                "unique content about tokio spawn concurrent execution patterns rust async",
            )
            .await
            .unwrap();

        assert!(
            candidates.iter().all(|c| c.id != note.id),
            "note should not be its own candidate"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detect_candidates_filters_out_different_type() {
        let tmp = crate::database::test_tempdir().unwrap();
        let (repo, project_id) = make_repo_and_project(&tmp).await;

        // Note of a DIFFERENT type — should be filtered (Low risk)
        repo.create(
            &project_id,
            tmp.path(),
            "Reference Note",
            "tokio spawn concurrent execution async rust service pattern for distributed systems",
            "reference",
            "[]",
        )
        .await
        .unwrap();

        let new_note = repo
            .create(
                &project_id,
                tmp.path(),
                "Pattern Note",
                "tokio spawn concurrent execution async rust service pattern for distributed systems",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let candidates = repo
            .detect_contradiction_candidates(
                &new_note.id,
                "pattern",
                "patterns",
                "tokio spawn concurrent execution async rust service pattern for distributed systems",
            )
            .await
            .unwrap();

        assert!(
            candidates.iter().all(|c| c.note_type == "pattern"),
            "different-type candidates should be filtered out (Low risk)"
        );
    }
}
