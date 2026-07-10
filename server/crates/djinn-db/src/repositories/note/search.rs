use super::*;
use crate::database::NoteSearchBackend as DatabaseNoteSearchBackend;
use crate::repositories::note::embeddings::embedding_branch_filter_sql;
use crate::repositories::note::rrf::rrf_fuse;
use crate::repositories::proposal::ProposalRepository;
use djinn_memory::{ContradictionCandidate, MemorySearchEntityRow, ProposalSearchResult, TypeRisk};

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
            DatabaseNoteSearchBackend::PostgresTsvector => LexicalSearchBackend::PostgresTsvector,
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
        // NOTE: dynamic SQL (backend-specific FTS query built from a runtime plan) — compile-time check not possible
        let sql = executable_lexical_search_sql(&plan);

        let mut q = sqlx::query_as::<sqlx::Postgres, (String, f64)>(&sql);
        if plan.needs_query_bind() {
            q = q.bind(&plan.query);
        }
        // The Postgres Ranked plan uses two distinct placeholders for folder
        // ($3 guard, $4 equality) and note_type ($5 guard, $6 equality), so
        // those values must be bound twice. SQLite FTS5 reuses numbered
        // placeholders natively, so bind once there.
        let repeat_filter_binds = matches!(
            plan.backend,
            crate::repositories::note::LexicalSearchBackend::PostgresTsvector
        );
        q = q.bind(project_id).bind(folder);
        if repeat_filter_binds {
            q = q.bind(folder);
        }
        q = q.bind(note_type);
        if repeat_filter_binds {
            q = q.bind(note_type);
        }
        let rows = q.bind(limit).fetch_all(self.db.pool()).await?;

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
        // NOTE: dynamic SQL (backend-specific FTS dedup query built from a runtime plan) — compile-time check not possible
        let sql = executable_lexical_search_sql(&plan);

        let mut q = sqlx::query_as::<
            sqlx::Postgres,
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
        >(&sql);
        if plan.needs_query_bind() {
            q = q.bind(&plan.query);
        }
        let rows = q
            .bind(project_id)
            .bind(folder)
            .bind(note_type)
            .bind(threshold)
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
        // NOTE: dynamic SQL (backend-specific FTS contradiction query built from a runtime plan) — compile-time check not possible
        let sql = executable_lexical_search_sql(&plan);

        let mut q =
            sqlx::query_as::<sqlx::Postgres, (String, String, String, String, String, f64)>(&sql);
        if plan.needs_query_bind() {
            q = q.bind(&plan.query);
        }
        let rows = q
            .bind(note_id)
            .bind(threshold)
            .fetch_all(self.db.pool())
            .await?;

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

        // This dedup lexical scan runs a `ts_rank()` fan-out during background
        // post-session knowledge extraction (one per extracted note). Bound it
        // with the same background-search permit consolidation uses so a burst
        // of finishing sessions can't saturate the interactive pool.
        let _permit = self.db.background_search_permit().await;

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
    ///
    /// When `entity_types` is `None` or contains `"proposal"`, proposal rows
    /// from `ProposalRepository::search_proposals` are merged into the result
    /// set. Proposal search errors are logged and silently skipped (best-effort).
    pub async fn search(&self, params: NoteSearchParams<'_>) -> Result<Vec<MemorySearchEntityRow>> {
        self.db.ensure_initialized().await?;

        let NoteSearchParams {
            project_id,
            query,
            task_id,
            folder,
            note_type,
            limit,
            semantic_scores,
            edge_kinds,
            entity_types,
        } = params;

        // ── entity_types gate ────────────────────────────────────────────────
        let wants_notes = entity_types
            .map(|ets| ets.iter().any(|e| e == "note"))
            .unwrap_or(true);
        let wants_proposals = entity_types
            .map(|ets| ets.iter().any(|e| e == "proposal"))
            .unwrap_or(true);

        // Some([]) → no entities requested → empty result.
        if entity_types.is_some_and(|ets| ets.is_empty()) {
            return Ok(vec![]);
        }
        // Some with values but none matching "note" or "proposal" → empty.
        if !wants_notes && !wants_proposals {
            return Ok(vec![]);
        }

        // ── note-side RRF pipeline ──────────────────────────────────────────
        let note_results: Vec<NoteSearchResult> = if wants_notes {
            self.search_notes_inner(
                project_id,
                query,
                task_id,
                folder,
                note_type,
                limit as i64,
                semantic_scores,
                edge_kinds,
            )
            .await?
        } else {
            vec![]
        };

        // ── proposal-side FTS ───────────────────────────────────────────────
        let proposal_results: Vec<ProposalSearchResult> = if wants_proposals {
            match self.search_proposals_inner(query, limit).await {
                Ok(results) => results,
                Err(e) => {
                    tracing::warn!("proposal search failed (falling back to notes-only): {e}");
                    vec![]
                }
            }
        } else {
            vec![]
        };

        Ok(merge_search_results(note_results, proposal_results, limit))
    }

    /// Internal note-only search: the original RRF pipeline logic, returning
    /// `NoteSearchResult` rows. Called by `search` when `wants_notes` is true.
    #[allow(clippy::too_many_arguments)]
    async fn search_notes_inner(
        &self,
        project_id: &str,
        query: &str,
        task_id: Option<&str>,
        folder: Option<&str>,
        note_type: Option<&str>,
        limit: i64,
        semantic_scores: Option<Vec<(String, f64)>>,
        edge_kinds: Option<&[String]>,
    ) -> Result<Vec<NoteSearchResult>> {
        let folder = folder.unwrap_or("");
        let note_type = note_type.unwrap_or("");

        let lexical_scores = self
            .ranked_lexical_scores(project_id, folder, note_type, query, limit)
            .await?;
        let semantic_scores = semantic_scores.unwrap_or_default();
        let candidate_ids = merge_candidate_ids(&[&lexical_scores, &semantic_scores]);

        if candidate_ids.is_empty() {
            return Ok(vec![]);
        }

        let temporal_scores = self.temporal_scores(project_id, &candidate_ids).await?;
        let (graph_scores, _graph_warnings) = self
            .graph_proximity_scores_with_edge_kinds(&candidate_ids, 2, edge_kinds)
            .await?;
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

        // NOTE: dynamic SQL (IN list built from ranked candidate ids).
        // project_id is $1, so the IN list starts at $2 (Postgres binds).
        let placeholders = crate::repositories::pg_placeholders(ranked_ids.len(), 2);
        let sql = format!(
            "SELECT id, permalink, title, folder, note_type,
                    COALESCE(abstract, substr(content, 1, 200)) as abstract_text
             FROM notes
             WHERE project_id = $1 AND status = 'active' AND id IN ({})",
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

    /// Delegate to `ProposalRepository::search_proposals`. Constructed
    /// on-the-fly from the shared `Database` + `EventBus`.
    async fn search_proposals_inner(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ProposalSearchResult>> {
        let proposal_repo = ProposalRepository::new(self.db.clone(), self.events.clone());
        proposal_repo.search_proposals(query, limit).await
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
        // Postgres positional binds. Fixed params occupy $1..=$5
        // (project_id, folder×2, note_type×2); the branch-filter IN list
        // follows starting at $6, and the note-id IN list starts right
        // after the branch values. Bind order below must match exactly.
        let (branch_filter_sql, branch_filter_values) =
            embedding_branch_filter_sql(task_branch.as_deref(), 6);
        let note_ids_start = 6 + branch_filter_values.len();
        let placeholders = crate::repositories::pg_placeholders(note_ids.len(), note_ids_start);
        let folder_val = folder.unwrap_or("");
        let note_type_val = note_type.unwrap_or("");
        // NOTE: dynamic SQL (IN list + branch filter clause built at runtime) — compile-time check not possible
        let sql = format!(
            "SELECT n.id FROM notes n
             JOIN note_embedding_meta m ON m.note_id = n.id
             WHERE n.project_id = $1
               AND ($2 = '' OR n.folder = $3)
               AND ($4 = '' OR n.note_type = $5)
               AND {branch_filter_sql}
               AND n.status = 'active'
               AND n.id IN ({})",
            placeholders
        );

        let mut query = sqlx::query_scalar::<_, String>(&sql)
            .bind(project_id)
            .bind(folder_val)
            .bind(folder_val)
            .bind(note_type_val)
            .bind(note_type_val);
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
                 WHERE project_id = $1 AND (id = $2 OR short_id = $3)
                 LIMIT 1",
        )
        .bind(project_id)
        .bind(task_id)
        .bind(task_id)
        .fetch_optional(self.db.pool())
        .await?
        .map(|short_id| task_branch_name(&short_id)))
    }

    /// Generate a markdown catalog (table of contents) for all notes in a
    /// project, grouped by folder and sorted alphabetically within each.
    pub async fn catalog(&self, project_id: &str) -> Result<String> {
        self.db.ensure_initialized().await?;

        let notes: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT folder, title, permalink, updated_at
             FROM notes WHERE project_id = $1 AND status = 'active'
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

        // NOTE: dynamic SQL (hours interval inlined when > 0) — compile-time check not possible.
        // Non-macro `query_as`: the JSONB→text cast must use a *plain* column
        // alias (`AS scope_paths`). The `AS "scope_paths!"` non-null assertion
        // is macro-only — in non-macro it yields a result column literally
        // named `scope_paths!`, which `FromRow` can't map to `NoteCompact`.
        let sql = if hours > 0 {
            format!(
                r#"SELECT id, permalink, title, note_type, folder, status, updated_at, scope_paths::text AS scope_paths
                 FROM notes
                 WHERE project_id = $1
                   AND status = 'active'
                   AND updated_at >= to_char((now() at time zone 'utc') - (interval '1 hour' * {hours}), 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                 ORDER BY updated_at DESC LIMIT $2"#
            )
        } else {
            r#"SELECT id, permalink, title, note_type, folder, status, updated_at, scope_paths::text AS scope_paths
             FROM notes WHERE project_id = $1 AND status = 'active'
             ORDER BY updated_at DESC LIMIT $2"#
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
        status: Option<&str>,
    ) -> Result<Vec<NoteCompact>> {
        self.list_compact_by_status(project_id, folder, note_type, depth, status)
            .await
    }

    /// List compact note summaries with an explicit lifecycle status filter.
    ///
    /// Passing `Some("archived")` is the reversible/listable archive path used
    /// by lifecycle tooling. Passing `None` or an empty status defaults to active
    /// for normal browsing.
    pub async fn list_compact_by_status(
        &self,
        project_id: &str,
        folder: Option<&str>,
        note_type: Option<&str>,
        depth: i64,
        status: Option<&str>,
    ) -> Result<Vec<NoteCompact>> {
        self.db.ensure_initialized().await?;
        let status = djinn_memory::note_status::normalize(status);
        if !djinn_memory::note_status::is_valid(&status) {
            return Err(Error::InvalidData(format!(
                "invalid note lifecycle status: {status}"
            )));
        }

        // NOTE: dynamic SQL (folder/note_type/status clauses appended at runtime) — compile-time check not possible.
        // Postgres positional binds: project_id is $1; appended clauses number
        // their placeholders from $2 onward. Plain `AS scope_paths` alias (the
        // `!` non-null assertion is macro-only — see `recent`).
        let mut sql = r#"SELECT id, permalink, title, note_type, folder, status, updated_at, scope_paths::text AS scope_paths
             FROM notes WHERE project_id = $1 AND status = $2"#
            .to_owned();

        let mut binds: Vec<String> = vec![project_id.to_string(), status];
        // Next free placeholder index ($1 is project_id, $2 is status).
        let mut next = 3;

        if let Some(f) = folder {
            if depth == 1 {
                sql.push_str(&format!(" AND folder = ${next}"));
                next += 1;
                binds.push(f.to_string());
            } else {
                let p_eq = next;
                let p_like = next + 1;
                sql.push_str(&format!(
                    " AND (folder = ${p_eq} OR folder LIKE ${p_like} || '/%')"
                ));
                next += 2;
                binds.push(f.to_string());
                binds.push(f.to_string());
            }
        }

        if let Some(t) = note_type {
            sql.push_str(&format!(" AND note_type = ${next}"));
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

        // Postgres positional binds. Fixed params: $1 = project_id,
        // $2 = min_confidence. Per-task-path EXISTS binds start at $3 and each
        // path consumes three placeholders (LIKE/LIKE/=).
        let mut path_binds: Vec<String> = Vec::new();
        let mut next = 3;

        let scope_clause = if task_paths.is_empty() {
            // Only global notes (empty JSONB array → length 0).
            "jsonb_array_length(n.scope_paths) = 0".to_string()
        } else {
            // Global notes OR bidirectional scope overlap:
            // - task path is under note scope (note is more general — parent match)
            // - note scope is under task path (note is more specific — child match)
            let mut exists_parts = Vec::new();
            for task_path in task_paths {
                let p_like_task = next;
                let p_like_scope = next + 1;
                let p_eq = next + 2;
                next += 3;
                path_binds.push(task_path.clone());
                path_binds.push(task_path.clone());
                path_binds.push(task_path.clone());
                exists_parts.push(format!(
                    "EXISTS (SELECT 1 FROM jsonb_array_elements_text(n.scope_paths) AS sp(value) \
                     WHERE ${p_like_task} LIKE sp.value || '/%' \
                        OR sp.value LIKE ${p_like_scope} || '/%' \
                        OR sp.value = ${p_eq})"
                ));
            }
            let exists_or = exists_parts.join(" OR ");
            format!("(jsonb_array_length(n.scope_paths) = 0 OR {exists_or})")
        };

        // NOTE: dynamic SQL (note_type IN list and per-task-path EXISTS clauses built at runtime) — compile-time check not possible.
        // Non-macro `query_as::<_, Note>`: JSONB columns must be cast to text
        // with plain aliases so `FromRow` maps them onto the `String` fields.
        let sql = format!(
            "SELECT n.id, n.project_id, n.permalink, n.title, n.file_path,
                    n.storage, n.note_type, n.folder, n.status, n.tags::text AS tags, n.content,
                    n.retrieval_anchor, n.created_at, n.updated_at, n.last_accessed,
                    n.access_count, n.confidence, n.abstract AS abstract_, n.overview,
                    n.scope_paths::text AS scope_paths
             FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type IN ({types_in})
               AND n.status = 'active'
               AND n.confidence >= $2
               AND {scope_clause}
             ORDER BY n.confidence DESC, n.updated_at DESC
             LIMIT {limit}"
        );

        let mut query = sqlx::query_as::<_, Note>(&sql);
        query = query.bind(project_id); // $1
        query = query.bind(min_confidence); // $2
        for val in &path_binds {
            query = query.bind(val);
        }

        Ok(query.fetch_all(self.db.pool()).await?)
    }

    /// Query unfiltered scope-overlap candidates for retrieval tracing.
    ///
    /// Uses the same eligibility and ordering as [`Self::query_by_scope_overlap`]
    /// (project, active status, note type, global-note handling, and
    /// bidirectional scope overlap), but intentionally omits the production
    /// confidence threshold and production injection limit. The only cap applied
    /// here is `trace_candidate_cap`, allowing downstream trace classifiers to
    /// label `min_confidence` and `not_top_k` drop reasons from the full ordered
    /// candidate set.
    pub async fn query_by_scope_overlap_trace_candidates(
        &self,
        project_id: &str,
        task_paths: &[String],
        note_types: &[&str],
        trace_candidate_cap: usize,
    ) -> Result<Vec<ScopeOverlapTraceCandidate>> {
        self.db.ensure_initialized().await?;

        // Build the note_type IN clause — these are controlled strings, safe to interpolate.
        let types_in = note_types
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(", ");

        // Postgres positional binds. Fixed param: $1 = project_id. Per-task-path
        // EXISTS binds start at $2 and each path consumes three placeholders
        // (LIKE/LIKE/=), matching `query_by_scope_overlap` without its
        // confidence bind.
        let mut path_binds: Vec<String> = Vec::new();
        let mut next = 2;

        let scope_clause = if task_paths.is_empty() {
            // Only global notes (empty JSONB array → length 0).
            "jsonb_array_length(n.scope_paths) = 0".to_string()
        } else {
            // Global notes OR bidirectional scope overlap:
            // - task path is under note scope (note is more general — parent match)
            // - note scope is under task path (note is more specific — child match)
            let mut exists_parts = Vec::new();
            for task_path in task_paths {
                let p_like_task = next;
                let p_like_scope = next + 1;
                let p_eq = next + 2;
                next += 3;
                path_binds.push(task_path.clone());
                path_binds.push(task_path.clone());
                path_binds.push(task_path.clone());
                exists_parts.push(format!(
                    "EXISTS (SELECT 1 FROM jsonb_array_elements_text(n.scope_paths) AS sp(value) \
                     WHERE ${p_like_task} LIKE sp.value || '/%' \
                        OR sp.value LIKE ${p_like_scope} || '/%' \
                        OR sp.value = ${p_eq})"
                ));
            }
            let exists_or = exists_parts.join(" OR ");
            format!("(jsonb_array_length(n.scope_paths) = 0 OR {exists_or})")
        };

        // NOTE: dynamic SQL (note_type IN list and per-task-path EXISTS clauses built at runtime) — compile-time check not possible.
        // Non-macro `query_as::<_, ScopeOverlapTraceCandidate>`: JSONB columns
        // must be cast to text with plain aliases so `FromRow` maps them onto
        // the `String` fields. Row rank is 1-based and derived from the exact
        // production ordering.
        let sql = format!(
            "SELECT n.id, n.permalink, n.title, n.folder, n.note_type,
                    n.scope_paths::text AS scope_paths, n.confidence,
                    ROW_NUMBER() OVER (ORDER BY n.confidence DESC, n.updated_at DESC) AS rank
             FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type IN ({types_in})
               AND n.status = 'active'
               AND {scope_clause}
             ORDER BY n.confidence DESC, n.updated_at DESC
             LIMIT {trace_candidate_cap}"
        );

        let mut query = sqlx::query_as::<_, ScopeOverlapTraceCandidate>(&sql);
        query = query.bind(project_id); // $1
        for val in &path_binds {
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

        // Postgres positional binds. $1 = project_id; per-changed-path EXISTS
        // binds start at $2, three placeholders per path (LIKE/LIKE/=).
        let mut bind_values: Vec<String> = vec![project_id.to_string()];
        let mut overlap_parts = Vec::new();
        let mut next = 2;

        for changed_path in changed_paths {
            let p_like_changed = next;
            let p_like_scope = next + 1;
            let p_eq = next + 2;
            next += 3;
            bind_values.push(changed_path.clone());
            bind_values.push(changed_path.clone());
            bind_values.push(changed_path.clone());
            overlap_parts.push(format!(
                "EXISTS (SELECT 1 FROM jsonb_array_elements_text(n.scope_paths) AS sp(value) \
                 WHERE ${p_like_changed} LIKE sp.value || '/%' \
                    OR sp.value LIKE ${p_like_scope} || '/%' \
                    OR sp.value = ${p_eq})"
            ));
        }

        let overlap_clause = overlap_parts.join(" OR ");
        // NOTE: dynamic SQL (per-changed-path EXISTS clauses built at runtime) — compile-time check not possible.
        // Non-macro `query_as::<_, Note>`: JSONB columns cast to text with
        // plain aliases for `FromRow`.
        let sql = format!(
            "SELECT n.id, n.project_id, n.permalink, n.title, n.file_path,
                    n.storage, n.note_type, n.folder, n.status, n.tags::text AS tags, n.content,
                    n.retrieval_anchor, n.created_at, n.updated_at, n.last_accessed,
                    n.access_count, n.confidence, n.abstract AS abstract_, n.overview,
                    n.scope_paths::text AS scope_paths
             FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND jsonb_array_length(n.scope_paths) > 0
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

        // `memory_refs` is a JSONB array of strings; use containment to match
        // any task whose array contains the requested permalink. We pass the
        // probe as a JSONB array literal so the index can drive the lookup.
        let probe = serde_json::Value::Array(vec![serde_json::Value::String(permalink.to_owned())]);
        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            r#"SELECT id, short_id, title, status FROM tasks
             WHERE memory_refs @> $1
             ORDER BY priority, created_at"#,
        )
        .bind(sqlx::types::Json(probe))
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

    /// Find proposals reachable through tasks whose `memory_refs` contains `permalink`.
    ///
    /// Walks `proposal_epics` to connect proposals → epics → tasks that reference
    /// the given permalink.  Returns minimal proposal info: `(id, short_id, title, status)`.
    pub async fn proposal_refs(&self, permalink: &str) -> Result<Vec<serde_json::Value>> {
        self.db.ensure_initialized().await?;

        // Same JSONB containment probe as `task_refs`.
        let probe = serde_json::Value::Array(vec![serde_json::Value::String(permalink.to_owned())]);
        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            r#"SELECT DISTINCT p.id, p.short_id, p.title, p.status
             FROM proposals p
             JOIN proposal_epics pe ON pe.proposal_id = p.id
             JOIN tasks t ON t.epic_id = pe.epic_id
             WHERE t.memory_refs @> $1
             ORDER BY p.id"#,
        )
        .bind(sqlx::types::Json(probe))
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

/// Merge note and proposal search results by descending score, truncating to
/// `limit`. Notes and proposals are converted to `MemorySearchEntityRow` via
/// their respective `From` impls.
fn merge_search_results(
    notes: Vec<NoteSearchResult>,
    proposals: Vec<ProposalSearchResult>,
    limit: usize,
) -> Vec<MemorySearchEntityRow> {
    let mut rows: Vec<MemorySearchEntityRow> = Vec::with_capacity(notes.len() + proposals.len());
    rows.extend(notes.into_iter().map(MemorySearchEntityRow::from));
    rows.extend(proposals.into_iter().map(MemorySearchEntityRow::from));
    rows.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    rows.truncate(limit);
    rows
}

#[cfg(test)]
mod contradiction_tests {
    use super::*;
    use crate::database::Database;
    use djinn_core::events::EventBus;
    use djinn_memory::TypeRisk;

    async fn make_repo_and_project(_tmp: &tempfile::TempDir) -> (NoteRepository, String) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let id = uuid::Uuid::now_v7().to_string();
        let owner = "test";
        let repo_slug = format!("contradiction-{id}");
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(&id)
        .bind("test")
        .bind(owner)
        .bind(repo_slug)
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
            repo.create(&project_id, &format!("Noise {i}"), content, "adr", "[]")
                .await
                .unwrap();
        }

        // Existing pattern note with specific rare content
        let shared = "tokio_spawn_contradiction_xqz concurrent_xqz execution_xqz async_xqz \
                      rust_xqz service_xqz pattern_xqz distributed_xqz systems_xqz \
                      architectural_xqz decision_xqz record_xqz implementation_xqz guide_xqz";
        let existing = repo
            .create(&project_id, "Existing Pattern", shared, "pattern", "[]")
            .await
            .unwrap();

        // New note with identical content — should be detected
        let new_note = repo
            .create(&project_id, "New Pattern", shared, "pattern", "[]")
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
        let owner = "test";
        let repo_slug = format!("scope-overlap-{id}");
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(&id)
        .bind("test")
        .bind(owner)
        .bind(repo_slug)
        .execute(db.pool())
        .await
        .unwrap();
        (NoteRepository::new(db, EventBus::noop()), tmp, id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_scoped_by_path_overlap_matches_parent_and_child_scopes_only() {
        let (repo, _tmp, project_id) = make_repo_and_project().await;

        let parent = repo
            .create_with_scope(
                &project_id,
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
            .create(&project_id, "Global Note", "content", "pattern", "[]")
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
        let (repo, _tmp, project_id) = make_repo_and_project().await;
        repo.create_with_scope(
            &project_id,
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

    async fn set_scope_trace_signals(
        repo: &NoteRepository,
        note_id: &str,
        confidence: f64,
        updated_at: &str,
    ) {
        sqlx::query("UPDATE notes SET confidence = $1, updated_at = $2 WHERE id = $3")
            .bind(confidence)
            .bind(updated_at)
            .bind(note_id)
            .execute(repo.db.pool())
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_by_scope_overlap_trace_candidates_keeps_unfiltered_ordered_candidates() {
        let (repo, _tmp, project_id) = make_repo_and_project().await;
        let other_project_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
        )
        .bind(&other_project_id)
        .bind("other")
        .bind("test")
        .bind(format!("scope-overlap-other-{other_project_id}"))
        .execute(repo.db.pool())
        .await
        .unwrap();

        let task_paths = vec!["server/src/server/state/mod.rs".to_string()];
        let cases = [
            (
                "High Parent",
                0.95,
                "2026-01-07T00:00:00.000Z",
                r#"["server/src"]"#,
            ),
            (
                "Recent Tie",
                0.90,
                "2026-01-08T00:00:00.000Z",
                r#"["server/src/server/state"]"#,
            ),
            (
                "Older Tie",
                0.90,
                "2026-01-06T00:00:00.000Z",
                r#"["server/src/server/state"]"#,
            ),
            (
                "Below Production Limit",
                0.80,
                "2026-01-05T00:00:00.000Z",
                "[]",
            ),
            (
                "Below Threshold",
                0.40,
                "2026-01-04T00:00:00.000Z",
                r#"["server/src/server/state/mod.rs"]"#,
            ),
            (
                "Capped Out",
                0.30,
                "2026-01-03T00:00:00.000Z",
                r#"["server/src"]"#,
            ),
        ];

        let mut notes = Vec::new();
        for (title, confidence, updated_at, scope_paths) in cases {
            let note = repo
                .create_with_scope(
                    &project_id,
                    title,
                    "content",
                    "pattern",
                    None,
                    "[]",
                    scope_paths,
                )
                .await
                .unwrap();
            set_scope_trace_signals(&repo, &note.id, confidence, updated_at).await;
            notes.push(note);
        }

        let unrelated = repo
            .create_with_scope(
                &project_id,
                "Unrelated Scope",
                "content",
                "pattern",
                None,
                "[]",
                r#"["desktop/src"]"#,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &unrelated.id, 0.99, "2026-01-09T00:00:00.000Z").await;

        let archived = repo
            .create_with_scope(
                &project_id,
                "Archived Scope",
                "content",
                "pattern",
                None,
                "[]",
                r#"["server/src"]"#,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &archived.id, 0.98, "2026-01-09T00:00:00.000Z").await;
        sqlx::query("UPDATE notes SET status = 'archived' WHERE id = $1")
            .bind(&archived.id)
            .execute(repo.db.pool())
            .await
            .unwrap();

        let wrong_type = repo
            .create_with_scope(
                &project_id,
                "Wrong Type",
                "content",
                "adr",
                None,
                "[]",
                r#"["server/src"]"#,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &wrong_type.id, 0.97, "2026-01-09T00:00:00.000Z").await;

        let other_project = repo
            .create_with_scope(
                &other_project_id,
                "Other Project",
                "content",
                "pattern",
                None,
                "[]",
                r#"["server/src"]"#,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &other_project.id, 0.96, "2026-01-09T00:00:00.000Z").await;

        let production = repo
            .query_by_scope_overlap(&project_id, &task_paths, &["pattern"], 0.5, 3)
            .await
            .unwrap();
        let production_titles: Vec<_> = production.iter().map(|note| note.title.as_str()).collect();
        assert_eq!(
            production_titles,
            vec!["High Parent", "Recent Tie", "Older Tie"]
        );

        let candidates = repo
            .query_by_scope_overlap_trace_candidates(&project_id, &task_paths, &["pattern"], 5)
            .await
            .unwrap();
        let candidate_titles: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.title.as_str())
            .collect();
        assert_eq!(
            candidate_titles,
            vec![
                "High Parent",
                "Recent Tie",
                "Older Tie",
                "Below Production Limit",
                "Below Threshold",
            ]
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.rank)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(candidates[4].confidence, 0.40);
        assert_eq!(candidates[3].scope_paths, "[]");
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.note_type == "pattern")
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.id != unrelated.id)
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.id != archived.id)
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.id != wrong_type.id)
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.id != other_project.id)
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.id != notes[5].id)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_by_scope_overlap_trace_candidates_empty_task_paths_matches_global_only() {
        let (repo, _tmp, project_id) = make_repo_and_project().await;
        let global = repo
            .create_with_scope(
                &project_id,
                "Global Trace Candidate",
                "content",
                "pattern",
                None,
                "[]",
                "[]",
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &global.id, 0.20, "2026-01-01T00:00:00.000Z").await;
        let scoped = repo
            .create_with_scope(
                &project_id,
                "Scoped Trace Candidate",
                "content",
                "pattern",
                None,
                "[]",
                r#"["server/src"]"#,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &scoped.id, 0.99, "2026-01-02T00:00:00.000Z").await;

        let candidates = repo
            .query_by_scope_overlap_trace_candidates(&project_id, &[], &["pattern"], 10)
            .await
            .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, global.id);
        assert_eq!(candidates[0].rank, 1);
    }
}
