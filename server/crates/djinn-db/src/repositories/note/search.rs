use super::*;
use crate::database::NoteSearchBackend as DatabaseNoteSearchBackend;
use crate::repositories::note::embeddings::embedding_branch_filter_sql;
use crate::repositories::note::rrf::{
    KNOWLEDGE_INJECTION_CANDIDATE_WINDOW, RankingProfile, injection_rrf_k, rrf_fuse,
    rrf_fuse_with_ranks,
};
use crate::repositories::note::scope_rank::{
    ScopeCandidate, normalize_scope_path, rank_scope_candidates,
};
use crate::repositories::proposal::ProposalRepository;
use djinn_memory::{ContradictionCandidate, MemorySearchEntityRow, ProposalSearchResult, TypeRisk};
use std::time::Duration;
use tokio::time::Instant;

/// Sort a signal list by score descending, note ID ascending, and truncate.
///
/// Truncation is always applied **after** sorting, so it can only remove the
/// weakest members of a list — never a high scorer that happened to arrive late
/// or whose ID sorts late.
fn cap_by_score(mut scores: Vec<(String, f64)>, limit: usize) -> Vec<(String, f64)> {
    scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scores.truncate(limit);
    scores
}

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

/// How much wider than the candidate window raw signal generation runs, so the
/// note-type/status eligibility filter still leaves a full window per signal.
const INJECTION_RAW_SIGNAL_MULTIPLIER: usize = 4;

/// Hard bound on rows scanned by the ranked-scope prefilter.
///
/// The scan is ordered by a score-correlated distance proxy (see
/// [`NoteRepository::ranked_scope_signal`]), so the cap can only ever discard
/// the *coarsest* matches — never an exact or near match. `n.id` breaks ties so
/// the cut is deterministic.
const INJECTION_SCOPE_SCAN_CAP: usize = 2000;

/// SQL expression yielding the canonical form of one `scope_paths` element,
/// applying the same rules as
/// [`normalize_scope_path`](crate::repositories::note::scope_rank::normalize_scope_path):
/// `\` folded to `/`, any leading `./` removed, repeated separators collapsed,
/// trailing separator dropped.
///
/// It must stay in step with the Rust rules. If SQL normalized *less* than Rust
/// does, the prefilter would stop being a superset and would silently discard
/// notes whose stored scope path is merely non-canonical — which is exactly the
/// bug this expression exists to prevent.
///
/// Absolute and `..`-bearing stored values are deliberately **not** repaired
/// here; they survive the prefilter and are then rejected in Rust, so the two
/// sides agree on the outcome.
const NORMALIZED_SCOPE_VALUE_SQL: &str = "rtrim(regexp_replace(regexp_replace(\
     replace(sp.value, '\\', '/'), '^(\\./)+', ''), '/+', '/', 'g'), '/')";

/// One-based ranks of a candidate within each contributing signal list, in the
/// fixed order lexical, embedding, temporal, graph, task-affinity, scope.
///
/// `None` means the candidate was absent from that signal's list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InjectionSignalRanks {
    pub lexical: Option<usize>,
    pub semantic: Option<usize>,
    pub temporal: Option<usize>,
    pub graph: Option<usize>,
    pub task_affinity: Option<usize>,
    pub scope: Option<usize>,
}

/// One fused knowledge-injection candidate with its full trace provenance.
#[derive(Debug, Clone)]
pub struct KnowledgeInjectionCandidate {
    pub note: Note,
    /// 1-based position in the single fused list handed to packing.
    pub fused_rank: usize,
    pub fused_score: f64,
    pub signal_ranks: InjectionSignalRanks,
}

/// Inputs to [`NoteRepository::search_knowledge_injection_candidates`].
#[derive(Debug)]
pub struct KnowledgeInjectionSearchParams<'a> {
    pub project_id: &'a str,
    /// Task text used for lexical retrieval.
    pub query: &'a str,
    pub task_id: Option<&'a str>,
    pub note_types: &'a [&'a str],
    /// Repository paths already validated against the base-revision tree. An
    /// empty slice yields an empty scope signal, never a recency fallback.
    pub task_paths: &'a [String],
    /// Requested injected cutoff. Sets `rrf_k` only.
    pub top_k: usize,
    /// Optional embedding list. `None` contributes an empty signal.
    pub semantic_scores: Option<Vec<(String, f64)>>,
}

/// The single ordered candidate list handed to packing, plus the ranking
/// identity recorded in retrieval traces.
#[derive(Debug, Clone)]
pub struct KnowledgeInjectionSearchResult {
    pub candidates: Vec<KnowledgeInjectionCandidate>,
    pub profile: RankingProfile,
    pub rrf_k: f64,
    pub candidate_window: usize,
}

#[derive(Debug, Clone, Default)]
struct NoteSearchStageTimings {
    lexical: Option<Duration>,
    semantic: Option<Duration>,
    temporal: Option<Duration>,
    graph: Option<Duration>,
    rrf_fuse: Option<Duration>,
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
        self.ranked_lexical_scores_in_mode(
            LexicalSearchMode::Ranked,
            project_id,
            folder,
            note_type,
            query,
            limit,
        )
        .await
    }

    /// [`Self::ranked_lexical_scores`] with the term-joining mode chosen by the
    /// caller. Both modes share the same SQL, bind order, and scoring; only the
    /// sanitized query expression differs.
    async fn ranked_lexical_scores_in_mode(
        &self,
        mode: LexicalSearchMode,
        project_id: &str,
        folder: &str,
        note_type: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<(String, f64)>> {
        debug_assert!(
            matches!(
                mode,
                LexicalSearchMode::Ranked | LexicalSearchMode::RankedAny
            ),
            "only the Ranked plans share this bind order"
        );
        let Some(plan) = self.lexical_search_plan(mode, query)? else {
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
                |(id, permalink, title, folder, note_type, content, abstract_, overview, score)| {
                    NoteDedupCandidate {
                        id,
                        permalink,
                        title,
                        folder,
                        note_type,
                        content,
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
    async fn search_rows(
        &self,
        params: NoteSearchParams<'_>,
    ) -> Result<(Vec<MemorySearchEntityRow>, NoteSearchStageTimings)> {
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
            return Ok((vec![], NoteSearchStageTimings::default()));
        }
        // Some with values but none matching "note" or "proposal" → empty.
        if !wants_notes && !wants_proposals {
            return Ok((vec![], NoteSearchStageTimings::default()));
        }

        // ── note-side RRF pipeline ──────────────────────────────────────────
        let (note_results, note_timings) = if wants_notes {
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
            (vec![], NoteSearchStageTimings::default())
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

        Ok((
            merge_search_results(note_results, proposal_results, limit),
            note_timings,
        ))
    }

    /// Compatibility search surface returning only rows.
    pub async fn search(&self, params: NoteSearchParams<'_>) -> Result<Vec<MemorySearchEntityRow>> {
        Ok(self.search_with_stats(params).await?.rows)
    }

    /// Additive timed companion to [`Self::search`].
    ///
    /// The compatibility search API remains row-only. This repository-owned
    /// result contains no trace data and does not emit telemetry.
    pub async fn search_with_stats(
        &self,
        params: NoteSearchParams<'_>,
    ) -> Result<TimedNoteSearchResult> {
        let (rows, note_timings) = self.search_rows(params).await?;
        Ok(TimedNoteSearchResult {
            summary: NoteSearchSummary {
                candidate_count: rows.iter().filter(|row| row.entity == "note").count(),
                result_count: rows.len(),
            },
            rows,
            lexical_duration: note_timings.lexical,
            semantic_duration: note_timings.semantic,
            temporal_duration: note_timings.temporal,
            graph_duration: note_timings.graph,
            rrf_fuse_duration: note_timings.rrf_fuse,
        })
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
    ) -> Result<(Vec<NoteSearchResult>, NoteSearchStageTimings)> {
        let folder = folder.unwrap_or("");
        let note_type = note_type.unwrap_or("");

        let lexical_start = Instant::now();
        let lexical_scores = self
            .ranked_lexical_scores(project_id, folder, note_type, query, limit)
            .await?;
        let lexical_duration = lexical_start.elapsed();

        let semantic_requested = semantic_scores.is_some();
        let semantic_scores = semantic_scores.unwrap_or_default();

        let (candidate_ids, semantic_duration) = if semantic_requested {
            let semantic_start = Instant::now();
            let candidate_ids = merge_candidate_ids(&[&lexical_scores, &semantic_scores]);
            (candidate_ids, Some(semantic_start.elapsed()))
        } else {
            let candidate_ids = lexical_scores.iter().map(|(id, _)| id.clone()).collect();
            (candidate_ids, None)
        };

        if candidate_ids.is_empty() {
            return Ok((
                vec![],
                NoteSearchStageTimings {
                    lexical: Some(lexical_duration),
                    semantic: semantic_duration,
                    ..NoteSearchStageTimings::default()
                },
            ));
        }

        let temporal_start = Instant::now();
        let temporal_scores = self.temporal_scores(project_id, &candidate_ids).await?;
        let temporal_duration = temporal_start.elapsed();

        let graph_start = Instant::now();
        let (graph_scores, _graph_warnings) = self
            .graph_proximity_scores_with_edge_kinds(&candidate_ids, 2, edge_kinds)
            .await?;
        let graph_duration = graph_start.elapsed();

        let task_scores = self.task_affinity_scores(project_id, task_id).await?;

        let confidence_map = self.note_confidence_map(&candidate_ids).await?;

        let rrf_start = Instant::now();
        let signals = vec![
            (lexical_scores, 60.0),
            (semantic_scores, 60.0),
            (temporal_scores, 60.0),
            (graph_scores, 60.0),
            (task_scores, 60.0),
        ];
        let fused = rrf_fuse(&signals, &confidence_map);
        let rrf_fuse_duration = rrf_start.elapsed();
        let fused_score_map: HashMap<String, f64> = fused.iter().cloned().collect();
        let ranked_ids: Vec<String> = fused
            .into_iter()
            .filter_map(|(id, _)| candidate_ids.contains(&id).then_some(id))
            .take(limit as usize)
            .collect();

        if ranked_ids.is_empty() {
            return Ok((
                vec![],
                NoteSearchStageTimings {
                    lexical: Some(lexical_duration),
                    semantic: semantic_duration,
                    temporal: Some(temporal_duration),
                    graph: Some(graph_duration),
                    rrf_fuse: Some(rrf_fuse_duration),
                },
            ));
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

        let note_results = ranked_ids
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
            .collect();

        Ok((
            note_results,
            NoteSearchStageTimings {
                lexical: Some(lexical_duration),
                semantic: semantic_duration,
                temporal: Some(temporal_duration),
                graph: Some(graph_duration),
                rrf_fuse: Some(rrf_fuse_duration),
            },
        ))
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

    // ── Knowledge-injection ranked retrieval (proposal 5205) ───────────────

    /// The deterministic ranked-scope signal list for `task_paths`.
    ///
    /// The SQL clause is a *superset* prefilter that bounds the scan; exact
    /// component comparability, distance, best-pair aggregation, ordering, and
    /// truncation are decided by
    /// [`rank_scope_candidates`](crate::repositories::note::scope_rank::rank_scope_candidates),
    /// which is pure and unit-tested. Global notes (empty `scope_paths`) are
    /// deliberately excluded from this signal; they still reach fusion through
    /// lexical, temporal, graph, or task-affinity lists.
    pub async fn ranked_scope_signal(
        &self,
        project_id: &str,
        task_paths: &[String],
        note_types: &[&str],
        window: usize,
    ) -> Result<Vec<(String, f64)>> {
        self.db.ensure_initialized().await?;
        if task_paths.is_empty() || note_types.is_empty() {
            return Ok(Vec::new());
        }

        // Normalize the task side once, here, so the SQL prefilter compares
        // canonical forms. An unsafe path (absolute or containing `.`/`..`) is
        // dropped rather than resolved, matching `normalized_components`.
        let task_paths: Vec<String> = task_paths
            .iter()
            .filter_map(|path| normalize_scope_path(path))
            .collect();
        if task_paths.is_empty() {
            return Ok(Vec::new());
        }

        let types_in = note_types
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(", ");

        // Postgres positional binds: $1 = project_id; per-task-path binds start
        // at $2 and consume three placeholders each (LIKE/LIKE/=). The ORDER BY
        // below reuses those same placeholders and adds none of its own.
        let mut path_binds: Vec<String> = Vec::new();
        let mut next = 2;
        let mut exists_parts = Vec::new();
        let mut distance_parts = Vec::new();
        // The stored side is normalized in SQL with the same rules Rust uses:
        // `\` folded to `/`, leading `./` removed, repeated separators
        // collapsed, trailing separator dropped. Without this the prefilter is
        // not a superset of what `component_distance` accepts, and a note
        // scoped to `./src/app` is discarded before Rust ever sees it.
        let scope_value = NORMALIZED_SCOPE_VALUE_SQL;
        for task_path in &task_paths {
            let p_like_task = next;
            let p_like_scope = next + 1;
            let p_eq = next + 2;
            next += 3;
            path_binds.push(task_path.clone());
            path_binds.push(task_path.clone());
            path_binds.push(task_path.clone());
            let overlap = format!(
                "${p_like_task} LIKE {scope_value} || '/%' \
                 OR {scope_value} LIKE ${p_like_scope} || '/%' \
                 OR {scope_value} = ${p_eq}"
            );
            exists_parts.push(format!(
                "EXISTS (SELECT 1 FROM jsonb_array_elements_text(n.scope_paths) AS sp(value) \
                 WHERE {overlap})"
            ));
            // Component count of this task path, computed here rather than in
            // SQL because it is a constant per path. Interpolated as a plain
            // integer literal, so it carries no injection surface.
            let task_components = task_path.split('/').filter(|c| !c.is_empty()).count();
            distance_parts.push(format!(
                "(SELECT MIN(abs({task_components} - \
                    COALESCE(array_length(string_to_array({scope_value}, '/'), 1), 0))) \
                  FROM jsonb_array_elements_text(n.scope_paths) AS sp(value) \
                  WHERE {overlap})"
            ));
        }
        let exists_or = exists_parts.join(" OR ");
        // `LEAST` ignores NULL arguments in Postgres, so a task path that
        // matches nothing on this row simply does not contribute.
        let best_distance = format!("LEAST({})", distance_parts.join(", "));

        // NOTE: dynamic SQL (note_type IN list, per-task-path EXISTS clauses,
        // and the distance expression built at runtime) — compile-time check
        // not possible.
        //
        // The scan cap must not defeat `rank_scope_candidates`' sort-then-
        // truncate guarantee (AC3). Ordering by `n.id` would do exactly that:
        // on a project with more matching notes than the cap, an exact-match
        // note whose ID sorts late would be dropped before scoring ever ran.
        //
        // So the cap is ordered by a *score-correlated* proxy instead: the
        // minimum component-count difference between any task path and any
        // overlapping scope path. The Rust score is `1 / (1 + min_distance)`,
        // strictly decreasing in that distance, so ascending distance is
        // descending score. Exact matches therefore always survive the cap,
        // then nearest ancestors, and only the coarsest matches can be cut.
        // `n.id` remains the tie-break so the cut stays deterministic.
        //
        // The proxy is computed over the LIKE prefilter, which is a superset of
        // true component comparability; exactness is still decided in Rust.
        let sql = format!(
            "SELECT n.id, n.scope_paths::text AS scope_paths
             FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type IN ({types_in})
               AND jsonb_array_length(n.scope_paths) > 0
               AND ({exists_or})
             ORDER BY {best_distance} NULLS LAST, n.id
             LIMIT {INJECTION_SCOPE_SCAN_CAP}"
        );

        let mut query = sqlx::query_as::<_, (String, String)>(&sql).bind(project_id);
        for value in &path_binds {
            query = query.bind(value);
        }
        let rows = query.fetch_all(self.db.pool()).await?;

        let candidates: Vec<ScopeCandidate> = rows
            .into_iter()
            .map(|(note_id, scope_paths)| ScopeCandidate {
                note_id,
                scope_paths: serde_json::from_str::<Vec<String>>(&scope_paths).unwrap_or_default(),
            })
            .collect();

        Ok(rank_scope_candidates(&task_paths, &candidates, window))
    }

    /// Ranked knowledge-injection retrieval under
    /// [`RankingProfile::KnowledgeInjectionV1`].
    ///
    /// This is the *only* caller of the injection profile. It fuses lexical,
    /// embedding, temporal, graph, task-affinity, and validated-scope lists,
    /// each requesting and retaining at most
    /// [`KNOWLEDGE_INJECTION_CANDIDATE_WINDOW`] eligible notes, and returns at
    /// most that many fused candidates in a single ordered list. Missing or
    /// inapplicable signals contribute an empty list and never change another
    /// signal's window. `top_k` only sets `rrf_k`; it never changes the window.
    ///
    /// Packing (`pack_ranked_knowledge_notes`) is the sole owner of the
    /// confidence floor, top-k, and byte budget; nothing here filters by
    /// confidence or truncates to `top_k`.
    pub async fn search_knowledge_injection_candidates(
        &self,
        params: KnowledgeInjectionSearchParams<'_>,
    ) -> Result<KnowledgeInjectionSearchResult> {
        self.db.ensure_initialized().await?;

        let KnowledgeInjectionSearchParams {
            project_id,
            query,
            task_id,
            note_types,
            task_paths,
            top_k,
            semantic_scores,
        } = params;

        let window = KNOWLEDGE_INJECTION_CANDIDATE_WINDOW;
        let rrf_k = injection_rrf_k(top_k);
        let empty = || KnowledgeInjectionSearchResult {
            candidates: Vec::new(),
            profile: RankingProfile::KnowledgeInjectionV1,
            rrf_k,
            candidate_window: window,
        };
        if note_types.is_empty() {
            return Ok(empty());
        }

        // Raw candidate generation runs wider than the window so that the
        // note-type/status eligibility filter below still has `window`
        // *eligible* rows to retain per signal.
        let raw_limit = window.saturating_mul(INJECTION_RAW_SIGNAL_MULTIPLIER);

        // The lexical list is one contributing signal of a fusion, not the
        // eligibility gate, so it disjoins its terms (`RankedAny`). AND-joining
        // a whole task title plus description returned zero notes for 22 of 25
        // sampled production tasks, and with scope/semantic/graph contributing
        // nothing that single empty list was the entire candidate universe.
        //
        // Lexical and scope read disjoint predicates over the same table and
        // neither feeds the other, so they overlap rather than serialize. Both
        // are hundreds of milliseconds against the production corpus and this
        // runs on the dispatch prompt path, where the retired scope-overlap
        // query already cost ~1.5s on average.
        let (lexical_scores, scope_scores) = tokio::join!(
            self.ranked_lexical_scores_in_mode(
                LexicalSearchMode::RankedAny,
                project_id,
                "",
                "",
                query,
                raw_limit as i64,
            ),
            self.ranked_scope_signal(project_id, task_paths, note_types, window),
        );
        let lexical_scores = lexical_scores?;
        let scope_scores = scope_scores?;
        let semantic_scores = semantic_scores.unwrap_or_default();

        // `seed_ids` are only the *seeds* for spreading activation, not the
        // candidate universe. Graph proximity returns neighbours outside its
        // seed set and task affinity is derived from task/epic memory refs
        // independently of any seed, so both must be allowed to introduce
        // candidates. Computing eligibility over the seeds alone would filter
        // exactly those introductions away and reduce two of the six signals to
        // re-orderers of what lexical/semantic/scope already found.
        let seed_ids = merge_candidate_ids(&[&lexical_scores, &semantic_scores, &scope_scores]);

        let (graph_result, task_scores) = tokio::join!(
            self.graph_proximity_scores_with_edge_kinds(&seed_ids, 2, None),
            self.task_affinity_scores(project_id, task_id),
        );
        let (graph_scores, _graph_warnings) = graph_result?;
        let task_scores = task_scores?;

        // Bound each contributing list by score before it widens the universe,
        // so the eligibility query stays well inside Postgres' bind limit. This
        // truncation is score-ordered (with note-ID ties), never ID-ordered, so
        // it can only drop the weakest members of a signal.
        let graph_scores = cap_by_score(graph_scores, raw_limit);
        let task_scores = cap_by_score(task_scores, raw_limit);

        // The candidate universe is the union of *every* signal's candidates.
        let universe = merge_candidate_ids(&[
            &lexical_scores,
            &semantic_scores,
            &scope_scores,
            &graph_scores,
            &task_scores,
        ]);
        if universe.is_empty() {
            return Ok(empty());
        }

        // Temporal is a re-scorer: it ranks the universe rather than widening
        // it, so it runs after the universe is complete and therefore covers
        // graph- and task-introduced candidates too.
        let temporal_scores = self.temporal_scores(project_id, &universe).await?;

        // Every signal is filtered by the same search filters — project, active
        // status, and the injected note types — and only then truncated to the
        // window, so each retains at most `window` *eligible* notes.
        let eligible = self
            .injection_eligible_ids(project_id, note_types, &universe)
            .await?;
        let retain = |scores: Vec<(String, f64)>| -> Vec<(String, f64)> {
            let mut kept: Vec<(String, f64)> = scores
                .into_iter()
                .filter(|(id, _)| eligible.contains(id))
                .collect();
            kept.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            kept.truncate(window);
            kept
        };

        // Signal order is fixed and mirrored by `InjectionSignalRanks`.
        let signals = vec![
            (retain(lexical_scores), rrf_k),
            (retain(semantic_scores), rrf_k),
            (retain(temporal_scores), rrf_k),
            (retain(graph_scores), rrf_k),
            (retain(task_scores), rrf_k),
            (retain(scope_scores), rrf_k),
        ];

        let fusion_ids = merge_candidate_ids(
            &signals
                .iter()
                .map(|(list, _)| list.as_slice())
                .collect::<Vec<_>>(),
        );
        if fusion_ids.is_empty() {
            return Ok(empty());
        }
        let confidence_map = self.note_confidence_map(&fusion_ids).await?;

        let (fused, signal_ranks) = rrf_fuse_with_ranks(
            &signals,
            &confidence_map,
            RankingProfile::KnowledgeInjectionV1,
        );
        let selected: Vec<(String, f64)> = fused.into_iter().take(window).collect();
        if selected.is_empty() {
            return Ok(empty());
        }

        let selected_ids: Vec<String> = selected.iter().map(|(id, _)| id.clone()).collect();
        let notes_by_id = self
            .injection_notes_by_id(project_id, &selected_ids)
            .await?;

        let rank_of = |signal_index: usize, note_id: &str| -> Option<usize> {
            signal_ranks
                .get(signal_index)
                .and_then(|ranks| ranks.get(note_id).copied())
        };

        let candidates = selected
            .into_iter()
            .filter_map(|(id, fused_score)| {
                notes_by_id
                    .get(&id)
                    .map(|note| KnowledgeInjectionCandidate {
                        signal_ranks: InjectionSignalRanks {
                            lexical: rank_of(0, &id),
                            semantic: rank_of(1, &id),
                            temporal: rank_of(2, &id),
                            graph: rank_of(3, &id),
                            task_affinity: rank_of(4, &id),
                            scope: rank_of(5, &id),
                        },
                        note: note.clone(),
                        fused_rank: 0,
                        fused_score,
                    })
            })
            .enumerate()
            .map(|(index, mut candidate)| {
                candidate.fused_rank = index + 1;
                candidate
            })
            .collect();

        Ok(KnowledgeInjectionSearchResult {
            candidates,
            profile: RankingProfile::KnowledgeInjectionV1,
            rrf_k,
            candidate_window: window,
        })
    }

    /// Ids from `candidate_ids` that pass the shared injection search filters:
    /// same project, active status, and one of the injected note types.
    async fn injection_eligible_ids(
        &self,
        project_id: &str,
        note_types: &[&str],
        candidate_ids: &[String],
    ) -> Result<HashSet<String>> {
        if candidate_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let types_in = note_types
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(", ");
        // NOTE: dynamic SQL (note_type IN list + id IN list). project_id is $1,
        // so the id IN list starts at $2.
        let placeholders = crate::repositories::pg_placeholders(candidate_ids.len(), 2);
        let sql = format!(
            "SELECT id FROM notes
             WHERE project_id = $1
               AND status = 'active'
               AND note_type IN ({types_in})
               AND id IN ({placeholders})"
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql).bind(project_id);
        for id in candidate_ids {
            query = query.bind(id);
        }
        Ok(query.fetch_all(self.db.pool()).await?.into_iter().collect())
    }

    /// Hydrate full note rows for the fused injection candidates.
    async fn injection_notes_by_id(
        &self,
        project_id: &str,
        note_ids: &[String],
    ) -> Result<HashMap<String, Note>> {
        if note_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // NOTE: dynamic SQL (id IN list). Non-macro `query_as::<_, Note>`:
        // JSONB columns must be cast to text with plain aliases for `FromRow`.
        let placeholders = crate::repositories::pg_placeholders(note_ids.len(), 2);
        let sql = format!(
            "SELECT n.id, n.project_id, n.permalink, n.title, n.file_path,
                    n.storage, n.note_type, n.folder, n.status, n.tags::text AS tags, n.content,
                    n.retrieval_anchor, n.created_at, n.updated_at, n.lifecycle_changed_at, n.last_accessed,
                    n.access_count, n.confidence, n.abstract AS abstract_, n.overview,
                    n.scope_paths::text AS scope_paths
             FROM notes n
             WHERE n.project_id = $1 AND n.id IN ({placeholders})"
        );
        let mut query = sqlx::query_as::<_, Note>(&sql).bind(project_id);
        for id in note_ids {
            query = query.bind(id);
        }
        Ok(query
            .fetch_all(self.db.pool())
            .await?
            .into_iter()
            .map(|note| (note.id.clone(), note))
            .collect())
    }

    /// Query notes whose `scope_paths` overlap with the given task paths.
    ///
    /// A note matches if it is either global (`scope_paths` is empty JSON array)
    /// or any of its scope path entries is a prefix of any provided task path.
    /// When `task_paths` is empty, only global notes are returned.
    ///
    /// Used by the JIT pitfalls retrieval path **and, on this branch, still by
    /// `load_knowledge_context`**.
    ///
    /// Proposal `5205` retires this query for the knowledge-injection entry
    /// point in favour of
    /// [`Self::search_knowledge_injection_candidates`], but that cutover
    /// (delivery-order step 4) is deliberately not flipped here: the ranked
    /// machinery has landed while `prompt_context.rs` still calls this method.
    /// The JIT call site is out of scope and keeps using it either way.
    pub async fn query_by_scope_overlap(
        &self,
        project_id: &str,
        task_paths: &[String],
        note_types: &[&str],
        min_confidence: f64,
        limit: usize,
    ) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;

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
                    n.retrieval_anchor, n.created_at, n.updated_at, n.lifecycle_changed_at, n.last_accessed,
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

    /// Query the complete ranked scope-overlap universe, including note bodies
    /// and L0 summaries, without the production confidence/top-K gates.
    pub async fn query_by_scope_overlap_trace_notes(
        &self,
        project_id: &str,
        task_paths: &[String],
        note_types: &[&str],
        trace_candidate_cap: usize,
    ) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;

        // Build the note_type IN clause — these are controlled strings, safe to interpolate.
        let types_in = note_types
            .iter()
            .map(|t| format!("'{t}'"))
            .collect::<Vec<_>>()
            .join(", ");

        // No confidence bind: NULL rows must reach `Note` mapping and fail there.
        let mut path_binds: Vec<String> = Vec::new();
        let mut next = 2;

        let scope_clause = if task_paths.is_empty() {
            "jsonb_array_length(n.scope_paths) = 0".to_string()
        } else {
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

        // `confidence >= -infinity` would incorrectly filter SQL NULL confidence.
        let sql = format!(
            "SELECT n.id, n.project_id, n.permalink, n.title, n.file_path,
                    n.storage, n.note_type, n.folder, n.status, n.tags::text AS tags, n.content,
                    n.retrieval_anchor, n.created_at, n.updated_at, n.lifecycle_changed_at, n.last_accessed,
                    n.access_count, n.confidence, n.abstract AS abstract_, n.overview,
                    n.scope_paths::text AS scope_paths
             FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type IN ({types_in})
               AND {scope_clause}
             ORDER BY n.confidence DESC, n.updated_at DESC
             LIMIT {trace_candidate_cap}"
        );

        let mut query = sqlx::query_as::<_, Note>(&sql);
        query = query.bind(project_id);
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
    /// here is `trace_candidate_cap`, allowing downstream trace classifiers
    /// (`mwtv`) to label `min_confidence` and `not_top_k` drop reasons from the
    /// full ordered candidate set.
    ///
    /// Returns [`ScopeOverlapTraceCandidate`] rows that map 1:1 to
    /// [`TraceCandidate`](crate::repositories::retrieval_trace::TraceCandidate)
    /// for JSONB persistence. The identity fields (`id`, `permalink`, `title`),
    /// ranking metadata (`rank`, `confidence`), and provenance (`folder`,
    /// `note_type`, `scope_paths`) form the complete data-layer contract
    /// consumed by `mwtv` (classification) and `liso` (`memory_recall_trace`
    /// tooling).
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
                    n.retrieval_anchor, n.created_at, n.updated_at, n.lifecycle_changed_at, n.last_accessed,
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
