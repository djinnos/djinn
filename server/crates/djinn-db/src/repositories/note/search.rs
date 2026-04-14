use super::*;
use crate::database::NoteSearchBackend as DatabaseNoteSearchBackend;
use crate::repositories::note::embeddings::embedding_branch_filter_sql;
use crate::repositories::note::rrf::rrf_fuse;
use djinn_core::models::{ContradictionCandidate, TypeRisk};

fn merge_candidate_ids(lists: &[&[(String, f64)]]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    for list in lists {
        for (id, _) in *list {
            if seen.insert(id.clone()) {
                ids.push(id.clone());
            }
        }
    }
    ids
}

impl NoteRepository {
    pub(crate) fn lexical_search_backend(&self) -> LexicalSearchBackend {
        match self.db.backend_capabilities().lexical_search {
            DatabaseNoteSearchBackend::SqliteFts5 => LexicalSearchBackend::SqliteFts5,
            DatabaseNoteSearchBackend::MysqlFulltext => LexicalSearchBackend::MysqlFulltext,
        }
    }

    fn lexical_search_plan(
        &self,
        mode: LexicalSearchMode,
        raw_query: &str,
    ) -> Result<Option<LexicalSearchPlan>> {
        build_lexical_search_plan(self.lexical_search_backend(), mode, raw_query)
    }

    async fn ranked_lexical_scores(
        &self,
        project_id: &str,
        folder: &str,
        note_type: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<(String, f64)>> {
        let Some(plan) = self.lexical_search_plan(LexicalSearchMode::Ranked, query)? else {
            return Ok(vec![]);
        };
        let sql = executable_lexical_search_sql(&plan);

        let rows = match self.db.pool_kind() {
            crate::database::DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, (String, f64)>(&sql)
                    .bind(&plan.query)
                    .bind(project_id)
                    .bind(folder)
                    .bind(note_type)
                    .bind(limit)
                    .fetch_all(pool)
                    .await?
            }
            crate::database::DatabasePool::Mysql(pool) => {
                sqlx::query_as::<sqlx::MySql, (String, f64)>(&sql)
                    .bind(&plan.query)
                    .bind(project_id)
                    .bind(folder)
                    .bind(note_type)
                    .bind(limit)
                    .fetch_all(pool)
                    .await?
            }
        };

        Ok(rows
            .into_iter()
            .map(|(id, score)| (id, normalize_lexical_score(&plan, score)))
            .collect())
    }

    async fn dedup_lexical_candidates(
        &self,
        project_id: &str,
        folder: &str,
        note_type: &str,
        text: &str,
        limit: i64,
    ) -> Result<Vec<NoteDedupCandidate>> {
        let Some(plan) = self.lexical_search_plan(LexicalSearchMode::Dedup, text)? else {
            return Ok(vec![]);
        };
        let threshold = lexical_search_threshold(plan.backend, LexicalSearchMode::Dedup)?
            .expect("dedup threshold is defined for all lexical backends");
        let sql = executable_lexical_search_sql(&plan);

        let rows = match self.db.pool_kind() {
            crate::database::DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<
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
                >(&sql)
                .bind(&plan.query)
                .bind(project_id)
                .bind(folder)
                .bind(note_type)
                .bind(threshold)
                .bind(limit)
                .fetch_all(pool)
                .await?
            }
            crate::database::DatabasePool::Mysql(pool) => {
                sqlx::query_as::<
                    sqlx::MySql,
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
                >(&sql)
                .bind(&plan.query)
                .bind(project_id)
                .bind(folder)
                .bind(note_type)
                .bind(threshold)
                .bind(limit)
                .fetch_all(pool)
                .await?
            }
        };

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
                        score: normalize_lexical_score(&plan, score),
                    }
                },
            )
            .collect())
    }

    async fn contradiction_lexical_candidates(
        &self,
        note_id: &str,
        note_type: &str,
        folder: &str,
        text: &str,
    ) -> Result<Vec<ContradictionCandidate>> {
        let Some(plan) = self.lexical_search_plan(LexicalSearchMode::Contradiction, text)? else {
            return Ok(vec![]);
        };
        let threshold = lexical_search_threshold(plan.backend, LexicalSearchMode::Contradiction)?
            .expect("contradiction threshold is defined for all lexical backends");
        let sql = executable_lexical_search_sql(&plan);

        let rows = match self.db.pool_kind() {
            crate::database::DatabasePool::Sqlite(pool) => {
                sqlx::query_as::<_, (String, String, String, String, String, f64)>(&sql)
                    .bind(&plan.query)
                    .bind(note_id)
                    .bind(threshold)
                    .fetch_all(pool)
                    .await?
            }
            crate::database::DatabasePool::Mysql(pool) => {
                sqlx::query_as::<sqlx::MySql, (String, String, String, String, String, f64)>(&sql)
                    .bind(&plan.query)
                    .bind(note_id)
                    .bind(threshold)
                    .fetch_all(pool)
                    .await?
            }
        };

        Ok(rows
            .into_iter()
            .filter_map(|(id, permalink, title, cand_folder, cand_type, score)| {
                let risk = if cand_type == note_type && cand_folder == folder {
                    TypeRisk::High
                } else if cand_type == note_type {
                    TypeRisk::Medium
                } else {
                    return None;
                };
                Some(ContradictionCandidate {
                    id,
                    permalink,
                    title,
                    folder: cand_folder,
                    note_type: cand_type,
                    score: normalize_lexical_score(&plan, score),
                    risk,
                })
            })
            .collect())
    }

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

        self.dedup_lexical_candidates(project_id, folder, note_type, text, limit as i64)
            .await
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

        self.contradiction_lexical_candidates(note_id, note_type, folder, text)
            .await
    }

    /// Full-text search with FTS candidate generation and RRF-fused ranking.
    ///
    /// `query` is a natural-language search string. It is sanitized into safe
    /// FTS5 syntax before execution.
    /// Results are ordered by relevance (best match first).
    pub async fn search(&self, params: NoteSearchParams<'_>) -> Result<Vec<NoteSearchResult>> {
        self.db.ensure_initialized().await?;

        let NoteSearchParams {
            project_id,
            query,
            task_id,
            folder,
            note_type,
            limit,
            semantic_scores,
        } = params;

        let folder = folder.unwrap_or("");
        let note_type = note_type.unwrap_or("");
        let limit = limit as i64;

        let lexical_scores = self
            .ranked_lexical_scores(project_id, folder, note_type, query, limit)
            .await?;
        let semantic_scores = semantic_scores.unwrap_or_default();
        let candidate_ids = merge_candidate_ids(&[&lexical_scores, &semantic_scores]);

        if candidate_ids.is_empty() {
            return Ok(vec![]);
        }

        let temporal_scores = self.temporal_scores(project_id, &candidate_ids).await?;
        let graph_scores = self.graph_proximity_scores(&candidate_ids, 2).await?;
        let task_scores = self.task_affinity_scores(project_id, task_id).await?;

        let confidence_map = self.note_confidence_map(&candidate_ids).await?;

        let signals = vec![
            (lexical_scores, 60.0),
            (semantic_scores, 60.0),
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

    pub async fn semantic_candidate_scores(
        &self,
        project_id: &str,
        query_embedding: &[f32],
        task_id: Option<&str>,
        folder: Option<&str>,
        note_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, f64)>> {
        self.db.ensure_initialized().await?;

        let task_branch = self.semantic_branch_for_task(project_id, task_id).await?;
        let raw_matches = self
            .query_similar_embeddings(
                query_embedding,
                EmbeddingQueryContext {
                    branch: task_branch.as_deref(),
                },
                limit.saturating_mul(5).max(limit),
            )
            .await?;
        if raw_matches.is_empty() {
            return Ok(vec![]);
        }

        let note_ids: Vec<String> = raw_matches.iter().map(|row| row.note_id.clone()).collect();
        let placeholders = std::iter::repeat_n("?", note_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let (branch_filter_sql, branch_filter_values) =
            embedding_branch_filter_sql(task_branch.as_deref());
        let sql = format!(
            "SELECT n.id FROM notes n
             JOIN note_embedding_meta m ON m.note_id = n.id
             WHERE n.project_id = ?1
               AND (?2 = '' OR n.folder = ?2)
               AND (?3 = '' OR n.note_type = ?3)
               AND {branch_filter_sql}
               AND n.id IN ({})",
            placeholders
        );

        let mut query = sqlx::query_scalar::<_, String>(&sql)
            .bind(project_id)
            .bind(folder.unwrap_or(""))
            .bind(note_type.unwrap_or(""));
        for branch in &branch_filter_values {
            query = query.bind(branch);
        }
        for note_id in &note_ids {
            query = query.bind(note_id);
        }

        let allowed_ids: HashSet<String> =
            query.fetch_all(self.db.pool()).await?.into_iter().collect();
        let mut scores: Vec<(String, f64)> = raw_matches
            .into_iter()
            .filter(|row| allowed_ids.contains(&row.note_id))
            .map(|row| (row.note_id, -row.distance))
            .collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scores.truncate(limit);
        Ok(scores)
    }

    async fn semantic_branch_for_task(
        &self,
        project_id: &str,
        task_id: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(task_id) = task_id else {
            return Ok(None);
        };

        Ok(sqlx::query_scalar::<_, String>(
            "SELECT short_id
                 FROM tasks
                 WHERE project_id = ?1 AND (id = ?2 OR short_id = ?2)
                 LIMIT 1",
        )
        .bind(project_id)
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?
        .map(|short_id| task_branch_name(&short_id)))
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
                "SELECT id, permalink, title, note_type, folder, updated_at, scope_paths
                 FROM notes
                 WHERE project_id = ?1
                   AND updated_at >= datetime('now', '-{hours} hours')
                 ORDER BY updated_at DESC LIMIT ?2"
            )
        } else {
            "SELECT id, permalink, title, note_type, folder, updated_at, scope_paths
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

        let mut sql = "SELECT id, permalink, title, note_type, folder, updated_at, scope_paths
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

    /// Query notes whose `scope_paths` overlap with the given task paths.
    ///
    /// A note matches if it is either global (`scope_paths` is empty JSON array)
    /// or any of its scope path entries is a prefix of any provided task path.
    /// When `task_paths` is empty, only global notes are returned.
    pub async fn query_by_scope_overlap(
        &self,
        project_id: &str,
        task_paths: &[String],
        note_types: &[&str],
        min_confidence: f64,
        limit: usize,
    ) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;

        // Build the note_type IN clause — these are controlled strings, safe to interpolate.
        let types_in = note_types
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(", ");

        let mut bind_values: Vec<String> = Vec::new();

        // ?1 = project_id
        bind_values.push(project_id.to_string());
        // ?2 = min_confidence
        // (handled separately as f64)

        let scope_clause = if task_paths.is_empty() {
            // Only global notes
            "json_array_length(n.scope_paths) = 0".to_string()
        } else {
            // Global notes OR bidirectional scope overlap:
            // - task path is under note scope (note is more general — parent match)
            // - note scope is under task path (note is more specific — child match)
            let mut exists_parts = Vec::new();
            for task_path in task_paths {
                let idx = bind_values.len() + 2; // +2 because ?2 is min_confidence
                bind_values.push(task_path.clone());
                exists_parts.push(format!(
                    "EXISTS (SELECT 1 FROM json_each(n.scope_paths) AS sp \
                     WHERE ?{idx} LIKE sp.value || '/%' \
                        OR sp.value LIKE ?{idx} || '/%' \
                        OR sp.value = ?{idx})"
                ));
            }
            let exists_or = exists_parts.join(" OR ");
            format!("(json_array_length(n.scope_paths) = 0 OR {exists_or})")
        };

        let sql = format!(
            "SELECT n.id, n.project_id, n.permalink, n.title, n.file_path,
                    n.storage, n.note_type, n.folder, n.tags, n.content,
                    n.created_at, n.updated_at, n.last_accessed,
                    n.access_count, n.confidence, n.abstract AS abstract_, n.overview,
                    n.scope_paths
             FROM notes n
             WHERE n.project_id = ?1
               AND n.note_type IN ({types_in})
               AND n.confidence >= ?2
               AND {scope_clause}
             ORDER BY n.confidence DESC, n.updated_at DESC
             LIMIT {limit}"
        );

        let mut query = sqlx::query_as::<_, Note>(&sql);
        query = query.bind(&bind_values[0]); // project_id
        query = query.bind(min_confidence); // ?2
        for val in &bind_values[1..] {
            query = query.bind(val);
        }

        Ok(query.fetch_all(self.db.pool()).await?)
    }

    /// Query notes whose non-empty `scope_paths` overlap with the given code paths.
    ///
    /// Unlike [`Self::query_by_scope_overlap`], this excludes global notes so callers
    /// can use it for change-driven scoped freshness decay without touching unrelated
    /// project-wide knowledge.
    pub async fn query_scoped_by_path_overlap(
        &self,
        project_id: &str,
        changed_paths: &[String],
        limit: usize,
    ) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;

        if changed_paths.is_empty() {
            return Ok(Vec::new());
        }

        let mut bind_values: Vec<String> = vec![project_id.to_string()];
        let mut overlap_parts = Vec::new();

        for changed_path in changed_paths {
            let idx = bind_values.len() + 1;
            bind_values.push(changed_path.clone());
            overlap_parts.push(format!(
                "EXISTS (SELECT 1 FROM json_each(n.scope_paths) AS sp \
                 WHERE ?{idx} LIKE sp.value || '/%' \
                    OR sp.value LIKE ?{idx} || '/%' \
                    OR sp.value = ?{idx})"
            ));
        }

        let overlap_clause = overlap_parts.join(" OR ");
        let sql = format!(
            "SELECT n.id, n.project_id, n.permalink, n.title, n.file_path,
                    n.storage, n.note_type, n.folder, n.tags, n.content,
                    n.created_at, n.updated_at, n.last_accessed,
                    n.access_count, n.confidence, n.abstract AS abstract_, n.overview,
                    n.scope_paths
             FROM notes n
             WHERE n.project_id = ?1
               AND json_array_length(n.scope_paths) > 0
               AND ({overlap_clause})
             ORDER BY n.updated_at DESC
             LIMIT {limit}"
        );

        let mut query = sqlx::query_as::<_, Note>(&sql);
        for value in &bind_values {
            query = query.bind(value);
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

#[cfg(test)]
mod scope_overlap_decay_tests {
    use super::*;
    use crate::database::Database;
    use djinn_core::events::EventBus;
    use std::collections::HashSet;

    async fn make_repo_and_project() -> (NoteRepository, tempfile::TempDir, String) {
        let tmp = crate::database::test_tempdir().unwrap();
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
        (NoteRepository::new(db, EventBus::noop()), tmp, id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_scoped_by_path_overlap_matches_parent_and_child_scopes_only() {
        let (repo, tmp, project_id) = make_repo_and_project().await;

        let parent = repo
            .create_with_scope(
                &project_id,
                tmp.path(),
                "Parent Scope",
                "content",
                "pattern",
                None,
                "[]",
                r#"["server/src"]"#,
            )
            .await
            .unwrap();
        let child = repo
            .create_with_scope(
                &project_id,
                tmp.path(),
                "Child Scope",
                "content",
                "pattern",
                None,
                "[]",
                r#"["server/src/server/state"]"#,
            )
            .await
            .unwrap();
        let unrelated = repo
            .create_with_scope(
                &project_id,
                tmp.path(),
                "Unrelated Scope",
                "content",
                "pattern",
                None,
                "[]",
                r#"["desktop/src"]"#,
            )
            .await
            .unwrap();
        let global = repo
            .create(
                &project_id,
                tmp.path(),
                "Global Note",
                "content",
                "pattern",
                "[]",
            )
            .await
            .unwrap();

        let matches = repo
            .query_scoped_by_path_overlap(
                &project_id,
                &["server/src/server/state/mod.rs".to_string()],
                20,
            )
            .await
            .unwrap();

        let ids: HashSet<String> = matches.into_iter().map(|note| note.id).collect();
        assert!(ids.contains(&parent.id));
        assert!(ids.contains(&child.id));
        assert!(!ids.contains(&unrelated.id));
        assert!(!ids.contains(&global.id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_scoped_by_path_overlap_is_noop_for_empty_changed_paths() {
        let (repo, tmp, project_id) = make_repo_and_project().await;
        repo.create_with_scope(
            &project_id,
            tmp.path(),
            "Scoped Note",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();

        let matches = repo
            .query_scoped_by_path_overlap(&project_id, &[], 20)
            .await
            .unwrap();
        assert!(matches.is_empty());
    }
}
