// djinn:allow-oversize — note context queries accumulated over many features; split when touched substantively.
use std::collections::{HashMap, HashSet};

use djinn_memory::{
    BuildContextResponse, ContradictsAnnotation, Note, NoteAbstract, NoteOverview,
    ProposalOverview, SupersedesAnnotation,
};

use crate::error::DbResult as Result;
use crate::repositories::note::rrf::rrf_fuse;
use crate::repositories::note::{
    LexicalSearchMode, build_lexical_search_plan, executable_lexical_search_sql,
    normalize_lexical_score,
};
use crate::repositories::proposal::ProposalRepository;

use super::NoteRepository;

/// Default minimum confidence threshold for related notes in build_context.
/// Notes below this value are excluded from context results. This prevents
/// contradicted or very low-confidence notes from polluting context.
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.1;

impl NoteRepository {
    /// Build context from a seed note with progressive disclosure and token budget pruning.
    ///
    /// # Arguments
    /// * `project_id` - The project ID
    /// * `seed_permalink` - The permalink of the seed note
    /// * `budget` - Optional token budget (default: 4096). Seeds are uncapped and always returned.
    /// * `task_id` - Optional task ID for task-affinity scoring in RRF pipeline
    /// * `max_related` - Maximum related notes to consider (before budget pruning)
    /// * `min_confidence` - Minimum confidence threshold for related notes (default 0.1).
    ///   Notes below this threshold are excluded. Superseded/stale-citation notes that pass
    ///   the threshold are annotated in the response.
    ///
    /// # Tier Disclosure Strategy
    /// * L2 (Primary/Seed): Full content, uncapped, never dropped
    /// * L1 (Direct neighbors): Overview text (fallback first 500 chars), 60% of post-seed budget
    /// * L0 (Discovered non-direct): Abstract text (fallback first 100 chars), 40% of post-seed budget
    #[allow(clippy::too_many_arguments)]
    pub async fn build_context(
        &self,
        project_id: &str,
        seed_permalink: &str,
        budget: Option<usize>,
        task_id: Option<&str>,
        max_related: usize,
        min_confidence: Option<f64>,
        edge_kinds: Option<&[String]>,
    ) -> Result<BuildContextResponse> {
        self.db.ensure_initialized().await?;

        let budget = budget.unwrap_or(4096);

        // Get the seed note with full content. Archived/deprecated notes remain
        // addressable through administrative APIs, but prompt-context assembly
        // is active-only by repository default.
        let Some(seed) = self.get_by_permalink(project_id, seed_permalink).await? else {
            return Ok(BuildContextResponse {
                primary: vec![],
                related_l1: vec![],
                related_l0: vec![],
                supersedes: vec![],
                contradicts: vec![],
                proposals: vec![],
            });
        };
        if seed.status != djinn_memory::note_status::ACTIVE {
            return Ok(BuildContextResponse {
                primary: vec![],
                related_l1: vec![],
                related_l0: vec![],
                supersedes: vec![],
                contradicts: vec![],
                proposals: vec![],
            });
        }

        // Get direct neighbors via wikilink graph
        let direct_neighbor_ids = self.get_direct_neighbors(project_id, &seed.id).await?;

        // Build direct neighbor set for quick lookup
        let direct_set: HashSet<String> = direct_neighbor_ids.iter().cloned().collect();

        // Run RRF retrieval pipeline to get discovered notes (includes FTS, temporal, graph, task-affinity)
        let discovered_ids = self
            .run_rrf_discovery(project_id, &seed, task_id, max_related * 2, edge_kinds)
            .await?;

        // Partition discovered notes: L1 (direct) vs L0 (non-direct)
        let mut l1_candidates: Vec<(String, f64)> = Vec::new();
        let mut l0_candidates: Vec<(String, f64)> = Vec::new();

        for (id, score) in discovered_ids {
            if id == seed.id {
                continue;
            }
            if direct_set.contains(&id) {
                l1_candidates.push((id, score));
            } else {
                l0_candidates.push((id, score));
            }
        }

        // Sort by RRF score descending
        l1_candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        l0_candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        // Build confidence map for all candidates to support filtering and
        // superseded annotation.
        let min_conf = min_confidence.unwrap_or(DEFAULT_MIN_CONFIDENCE);
        let all_candidate_ids: Vec<String> = l1_candidates
            .iter()
            .map(|(id, _)| id.clone())
            .chain(l0_candidates.iter().map(|(id, _)| id.clone()))
            .collect();
        let confidence_map = self.note_confidence_map(&all_candidate_ids).await?;

        // Filter candidates below min_confidence threshold.
        l1_candidates.retain(|(id, _)| confidence_map.get(id).copied().unwrap_or(1.0) >= min_conf);
        l0_candidates.retain(|(id, _)| confidence_map.get(id).copied().unwrap_or(1.0) >= min_conf);

        // Filter superseded source notes: if a candidate note has a
        // `supersedes` edge to another note that is also in the result set
        // (either as the seed or as another candidate), drop the superseded
        // note so the canonical and its source do not compete in the prompt.
        // The substrate is undirected, so we check both endpoints.
        let mut context_ids: HashSet<String> = HashSet::new();
        context_ids.insert(seed.id.clone());
        for (id, _) in l1_candidates.iter().chain(l0_candidates.iter()) {
            context_ids.insert(id.clone());
        }
        if !context_ids.is_empty() {
            let context_id_list: Vec<String> = context_ids.iter().cloned().collect();
            let supersedes_map = self.supersedes_neighbors(&context_id_list).await?;
            l1_candidates.retain(|(id, _)| {
                !is_superseded_by_sibling(id, &supersedes_map, &context_ids, &seed.id)
            });
            l0_candidates.retain(|(id, _)| {
                !is_superseded_by_sibling(id, &supersedes_map, &context_ids, &seed.id)
            });
        }

        // Fetch note data and apply budget-aware pruning
        let l1_notes = self.fetch_l1_notes(&l1_candidates).await?;
        let l0_notes = self.fetch_l0_notes(&l0_candidates).await?;

        // Apply budget pruning
        let (mut pruned_l1, mut pruned_l0) =
            apply_budget_pruning(l1_notes, l0_notes, budget, &seed.content);

        // Annotate superseded notes: notes whose confidence is at or below the
        // STALE_CITATION threshold (0.3) are marked as superseded so consumers
        // know the content may be outdated.
        use crate::repositories::note::scoring::STALE_CITATION;
        for note in &mut pruned_l1 {
            if confidence_map.get(&note.id).copied().unwrap_or(1.0) <= STALE_CITATION {
                note.superseded = true;
                note.overview_text = format!("[SUPERSEDED] {}", note.overview_text);
            }
        }
        for note in &mut pruned_l0 {
            if confidence_map.get(&note.id).copied().unwrap_or(1.0) <= STALE_CITATION {
                note.superseded = true;
                note.abstract_text = format!("[SUPERSEDED] {}", note.abstract_text);
            }
        }

        // Build supersedes/contradicts annotations for the final context set.
        let mut final_context_ids: HashSet<String> = HashSet::new();
        final_context_ids.insert(seed.id.clone());
        for note in pruned_l1.iter() {
            final_context_ids.insert(note.id.clone());
        }
        for note in pruned_l0.iter() {
            final_context_ids.insert(note.id.clone());
        }

        let supersedes_annotations = self
            .supersedes_annotations(&seed.id, &final_context_ids)
            .await?;
        let contradicts_annotations = self.contradicts_annotations(&final_context_ids).await?;

        // ── relevant proposals for seed ──────────────────────────────────────
        let proposals = self
            .find_relevant_proposals(project_id, &seed)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("relevant-proposals-for-seed query failed (skipping): {e}");
                vec![]
            });

        Ok(BuildContextResponse {
            primary: vec![seed],
            related_l1: pruned_l1,
            related_l0: pruned_l0,
            supersedes: supersedes_annotations,
            contradicts: contradicts_annotations,
            proposals,
        })
    }

    /// Find proposals relevant to a seed note for `build_context`.
    ///
    /// Two complementary paths, deduplicated by `proposal_id` with the higher
    /// score kept:
    ///
    /// 1. **Epic-derived**: proposals whose graduated epics contain tasks with
    ///    `memory_refs` that reference the seed's permalink.
    /// 2. **Title-body match**: proposals targeting the current project whose
    ///    `title` or `body` lexically matches the seed's title or content
    ///    (ILIKE/plainto_tsquery — "planner sees motivating proposal", not
    ///    exhaustive relevance).
    ///
    /// Capped at 3 results.
    async fn find_relevant_proposals(
        &self,
        project_id: &str,
        seed: &Note,
    ) -> Result<Vec<ProposalOverview>> {
        let proposal_repo = ProposalRepository::new(self.db.clone(), self.events.clone());

        let mut scored: HashMap<String, (f64, djinn_core::models::Proposal)> = HashMap::new();

        // ── Path 1: epic-derived via proposal_refs (walks proposal_epics → tasks → memory_refs) ──
        if let Ok(refs) = self.proposal_refs(&seed.permalink).await {
            for val in refs {
                if let (Some(id), Some(short_id)) = (
                    val.get("id").and_then(|v| v.as_str()),
                    val.get("short_id").and_then(|v| v.as_str()),
                ) {
                    if let Ok(Some(proposal)) = proposal_repo.get(id).await {
                        scored.entry(id.to_string()).or_insert((0.5, proposal));
                    }
                    // Avoid unused warning
                    let _ = short_id;
                }
            }
        }

        // ── Path 2: title/body match via proposal_targets for this project ──
        let probe_text = format!(
            "{} {}",
            seed.title,
            seed.content.chars().take(200).collect::<String>()
        );
        let probe_tokens: Vec<&str> = probe_text
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.len() >= 3)
            .take(8)
            .collect();

        if !probe_tokens.is_empty() {
            // Build ILIKE conditions: any token appearing in the concatenated
            // proposal searchable text qualifies.  Using OR (not AND) because
            // the goal is "planner sees motivating proposal", not exhaustive
            // relevance — one matching keyword is enough.
            let mut conditions = Vec::new();
            for (i, _token) in probe_tokens.iter().enumerate() {
                conditions.push(format!(
                    "(p.title || ' ' || p.body || ' ' || COALESCE(p.acceptance_criteria::text, '')) ILIKE ${}",
                    i + 2
                ));
            }
            let where_clause = conditions.join(" OR ");

            // NOTE: dynamic SQL (ILIKE conditions built from seed tokens) —
            // compile-time check not possible.
            let sql = format!(
                r#"SELECT p.id, p.short_id, p.title, p.body, p.body_format,
                          p.acceptance_criteria::text AS acceptance_criteria,
                          p.status, p.author_user_id, p.superseded_by,
                          p.created_at, p.updated_at, p.closed_at,
                          p.latest_revision_seq, p.last_reconciled_revision_seq,
                          p.pending_reconcile, p.build_owner_user_id,
                          p.refinement_owner_user_id, p.build_frozen,
                          p.build_breakdown_task_id,
                          p.linked_spike_task_id, p.needs_evidence_claim
                   FROM proposals p
                   JOIN proposal_targets pt ON pt.proposal_id = p.id
                   WHERE pt.project_id = $1
                     AND p.status NOT IN ('archived', 'rejected')
                     AND ({})
                   LIMIT 5"#,
                where_clause
            );

            let mut q = sqlx::query_as::<sqlx::Postgres, djinn_core::models::Proposal>(&sql)
                .bind(project_id);
            for token in &probe_tokens {
                q = q.bind(format!("%{}%", token));
            }

            if let Ok(rows) = q.fetch_all(self.db.pool()).await {
                for proposal in rows {
                    let entry = scored.entry(proposal.id.clone()).or_insert((0.3, proposal));
                    // Path 2 base score is 0.3; if already present from path 1
                    // (score 0.5), keep the higher.
                    if entry.0 < 0.3 {
                        entry.0 = 0.3;
                    }
                }
            }
        }

        // Sort by score descending, cap at 3.
        let mut ranked: Vec<(f64, djinn_core::models::Proposal)> = scored.into_values().collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        ranked.truncate(3);

        // Convert to ProposalOverview.
        Ok(ranked
            .into_iter()
            .map(|(score, p)| {
                let acceptance_criteria =
                    djinn_core::models::parse_json_array(&p.acceptance_criteria);
                ProposalOverview {
                    id: p.id,
                    short_id: p.short_id,
                    title: p.title,
                    body_format: p.body_format,
                    acceptance_criteria,
                    status: p.status,
                    score: Some(score),
                }
            })
            .collect())
    }

    /// Build `SupersedesAnnotation` entries for notes in the context set.
    /// For each note that has a `supersedes` edge to another note also in the
    /// context set, we annotate the superseded note with the ID of the note
    /// that supersedes it.
    async fn supersedes_annotations(
        &self,
        _seed_id: &str,
        context_ids: &HashSet<String>,
    ) -> Result<Vec<SupersedesAnnotation>> {
        if context_ids.is_empty() {
            return Ok(Vec::new());
        }

        let id_list: Vec<String> = context_ids.iter().cloned().collect();
        let sql = format!(
            "SELECT note_a_id, note_b_id, kind FROM note_associations
             WHERE kind = 'supersedes'
               AND (note_a_id IN ({}) OR note_b_id IN ({}))",
            crate::repositories::pg_placeholders(id_list.len(), 1),
            crate::repositories::pg_placeholders(id_list.len(), 1 + id_list.len())
        );

        let mut q = sqlx::query_as::<_, (String, String, String)>(&sql);
        for id in &id_list {
            q = q.bind(id);
        }
        for id in &id_list {
            q = q.bind(id);
        }

        let rows: Vec<(String, String, String)> = q.fetch_all(self.db.pool()).await?;

        let mut annotations: Vec<SupersedesAnnotation> = Vec::new();
        for (note_a_id, note_b_id, kind) in rows {
            // Undirected substrate: note_a_id < note_b_id (canonical pair ordering).
            // note_a is the source (older), note_b is the canonical (newer).
            // If the superseded note (note_a) is in the context set, annotate it
            // with note_b as the superseding note.
            if context_ids.contains(&note_a_id) {
                annotations.push(SupersedesAnnotation {
                    superseded_by: note_b_id.clone(),
                    edge_kind: kind.clone(),
                });
            }
            // Also handle if seed is note_b and note_a is in context.
            // In this case, seed is the superseding note; note_a is superseded.
            // This is already handled above if note_a is in context_ids.
        }

        Ok(annotations)
    }

    /// Build `ContradictsAnnotation` entries for notes in the context set.
    async fn contradicts_annotations(
        &self,
        context_ids: &HashSet<String>,
    ) -> Result<Vec<ContradictsAnnotation>> {
        if context_ids.is_empty() {
            return Ok(Vec::new());
        }

        let id_list: Vec<String> = context_ids.iter().cloned().collect();
        let sql = format!(
            "SELECT note_a_id, note_b_id, kind FROM note_associations
             WHERE kind = 'contradicts'
               AND (note_a_id IN ({}) OR note_b_id IN ({}))",
            crate::repositories::pg_placeholders(id_list.len(), 1),
            crate::repositories::pg_placeholders(id_list.len(), 1 + id_list.len())
        );

        let mut q = sqlx::query_as::<_, (String, String, String)>(&sql);
        for id in &id_list {
            q = q.bind(id);
        }
        for id in &id_list {
            q = q.bind(id);
        }

        let rows: Vec<(String, String, String)> = q.fetch_all(self.db.pool()).await?;

        let mut annotations: Vec<ContradictsAnnotation> = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for (note_a_id, note_b_id, kind) in rows {
            // Root annotations at notes in the context set. If only one
            // endpoint is in the final context, surface the outside note as the
            // contradicting note_id. If both endpoints are present, surface both
            // directions so either affected entry can discover its counterpart.
            if context_ids.contains(&note_a_id) && seen.insert((note_b_id.clone(), kind.clone())) {
                annotations.push(ContradictsAnnotation {
                    note_id: note_b_id.clone(),
                    edge_kind: kind.clone(),
                });
            }
            if context_ids.contains(&note_b_id) && seen.insert((note_a_id.clone(), kind.clone())) {
                annotations.push(ContradictsAnnotation {
                    note_id: note_a_id,
                    edge_kind: kind,
                });
            }
        }

        Ok(annotations)
    }

    /// Get direct wikilink neighbors (one hop from seed).
    async fn get_direct_neighbors(&self, project_id: &str, seed_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query_scalar::<_, String>(
            r#"SELECT target_id AS "target_id!: String" FROM note_links
             WHERE source_id = $1
               AND target_id IS NOT NULL
               AND target_id IN (SELECT id FROM notes WHERE project_id = $2 AND status = 'active')"#,
        )
        .bind(seed_id)
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows)
    }

    /// Run RRF retrieval pipeline for discovered notes.
    async fn run_rrf_discovery(
        &self,
        project_id: &str,
        seed: &Note,
        task_id: Option<&str>,
        limit: usize,
        edge_kinds: Option<&[String]>,
    ) -> Result<Vec<(String, f64)>> {
        // Build query from seed content (first 200 chars for efficiency)
        let query = seed.content.chars().take(200).collect::<String>();

        // Get FTS candidates
        let fts_candidates = self.fts_candidates(project_id, &query, limit).await?;

        // Get temporal scores for all notes
        let temporal_scores = self.temporal_scores_all(project_id).await?;

        // Get graph proximity scores from seed
        let (graph_scores, _graph_warnings) = self
            .graph_proximity_scores_with_edge_kinds(std::slice::from_ref(&seed.id), 2, edge_kinds)
            .await?;

        // Get task-affinity scores
        let task_scores = self.task_affinity_scores(project_id, task_id).await?;

        let mut confidence_ids: Vec<String> =
            fts_candidates.iter().map(|(id, _)| id.clone()).collect();
        confidence_ids.extend(temporal_scores.iter().map(|(id, _)| id.clone()));
        confidence_ids.extend(graph_scores.iter().map(|(id, _)| id.clone()));
        confidence_ids.extend(task_scores.iter().map(|(id, _)| id.clone()));
        confidence_ids.sort();
        confidence_ids.dedup();

        let confidence_map = self.note_confidence_map(&confidence_ids).await?;

        // Prepare signals for RRF
        let signals = vec![
            (fts_candidates, 60.0),
            (temporal_scores, 60.0),
            (graph_scores, 60.0),
            (task_scores, 60.0),
        ];

        let fused = rrf_fuse(&signals, &confidence_map);
        Ok(fused.into_iter().take(limit).collect())
    }

    /// Get FTS candidates for discovery query.
    async fn fts_candidates(
        &self,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, f64)>> {
        let Some(plan) = build_lexical_search_plan(
            self.lexical_search_backend(),
            LexicalSearchMode::Discovery,
            query,
        )?
        else {
            return Ok(vec![]);
        };

        let sql = executable_lexical_search_sql(&plan);
        // NOTE: dynamic SQL — compile-time check not possible
        let mut q = sqlx::query_as::<sqlx::Postgres, (String, f64)>(&sql);
        if plan.needs_query_bind() {
            q = q.bind(&plan.query);
        }
        let rows: Vec<(String, f64)> = q
            .bind(project_id)
            .bind(limit as i64)
            .fetch_all(self.db.pool())
            .await?;

        Ok(rows
            .into_iter()
            .map(|(id, score)| (id, normalize_lexical_score(&plan, score)))
            .collect())
    }

    /// Get temporal scores for all notes in project.
    async fn temporal_scores_all(&self, project_id: &str) -> Result<Vec<(String, f64)>> {
        let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
            "SELECT id, access_count, created_at, updated_at
             FROM notes
             WHERE project_id = $1
               AND status = 'active'",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};

        let now = SystemClockTrait::new().now();
        const HALF_LIFE_DAYS: f64 = 7.0;
        const HOTNESS_ALPHA: f64 = 0.2;

        let mut scores: Vec<(String, f64)> = rows
            .into_iter()
            .map(
                |(id, access_count, created_at, updated_at): (String, i64, String, String)| {
                    let created_age_days = age_days(&created_at, now);
                    let updated_age_days = age_days(&updated_at, now);

                    let safe_created_age = created_age_days.max(f64::EPSILON);
                    let safe_updated_age = updated_age_days.max(f64::EPSILON);

                    let base_actr =
                        ((access_count.max(0) as f64) + 1.0).ln() - safe_created_age.ln();
                    let recency_boost = 2f64.powf(-(safe_updated_age / HALF_LIFE_DAYS));
                    let hotness = HOTNESS_ALPHA * recency_boost;
                    let score = base_actr + hotness;

                    (id, score)
                },
            )
            .collect();

        scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(scores)
    }

    /// Fetch L1 notes (direct neighbors) with overview disclosure.
    async fn fetch_l1_notes(
        &self,
        candidates: &[(String, f64)],
    ) -> Result<Vec<(NoteOverview, usize)>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }

        let ids: Vec<String> = candidates.iter().map(|(id, _)| id.clone()).collect();
        // Postgres $N binds; no fixed params precede the IN list.
        let sql = format!(
            "SELECT id, permalink, title, note_type, COALESCE(overview, substr(content, 1, 500)) as disclosure_text
             FROM notes
             WHERE status = 'active' AND id IN ({})",
            crate::repositories::pg_placeholders(ids.len(), 1)
        );

        // NOTE: dynamic SQL — compile-time check not possible (runtime IN list)
        let mut query = sqlx::query_as::<_, (String, String, String, String, String)>(&sql);
        for id in &ids {
            query = query.bind(id);
        }

        let rows: Vec<(String, String, String, String, String)> =
            query.fetch_all(self.db.pool()).await?;
        let score_map: HashMap<String, f64> = candidates.iter().cloned().collect();

        let notes: Vec<(NoteOverview, usize)> = rows
            .into_iter()
            .map(
                |(id, permalink, title, note_type, disclosure_text): (
                    String,
                    String,
                    String,
                    String,
                    String,
                )| {
                    let score = score_map.get(&id).copied();
                    let token_estimate = disclosure_text.len() / 4;
                    let overview = NoteOverview {
                        id,
                        permalink,
                        title,
                        note_type,
                        overview_text: disclosure_text,
                        score: score.map(|s| s as f32),
                        superseded: false,
                    };
                    (overview, token_estimate)
                },
            )
            .collect();

        Ok(notes)
    }

    /// Fetch L0 notes (discovered non-direct) with abstract disclosure.
    async fn fetch_l0_notes(
        &self,
        candidates: &[(String, f64)],
    ) -> Result<Vec<(NoteAbstract, usize)>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }

        let ids: Vec<String> = candidates.iter().map(|(id, _)| id.clone()).collect();
        // Postgres $N binds; no fixed params precede the IN list.
        let sql = format!(
            "SELECT id, permalink, title, note_type, COALESCE(abstract, substr(content, 1, 100)) as disclosure_text
             FROM notes
             WHERE status = 'active' AND id IN ({})",
            crate::repositories::pg_placeholders(ids.len(), 1)
        );

        // NOTE: dynamic SQL — compile-time check not possible (runtime IN list)
        let mut query = sqlx::query_as::<_, (String, String, String, String, String)>(&sql);
        for id in &ids {
            query = query.bind(id);
        }

        let rows: Vec<(String, String, String, String, String)> =
            query.fetch_all(self.db.pool()).await?;
        let score_map: HashMap<String, f64> = candidates.iter().cloned().collect();

        let notes: Vec<(NoteAbstract, usize)> = rows
            .into_iter()
            .map(
                |(id, permalink, title, note_type, disclosure_text): (
                    String,
                    String,
                    String,
                    String,
                    String,
                )| {
                    let score = score_map.get(&id).copied();
                    let token_estimate = disclosure_text.len() / 4;
                    let abstract_ = NoteAbstract {
                        id,
                        permalink,
                        title,
                        note_type,
                        abstract_text: disclosure_text,
                        score: score.map(|s| s as f32),
                        superseded: false,
                    };
                    (abstract_, token_estimate)
                },
            )
            .collect();

        Ok(notes)
    }
}

/// Determine whether `note_id` is the superseded source of a `supersedes`
/// edge whose canonical endpoint is also present in the current result set.
///
/// A candidate note is dropped when it participates in a `supersedes` edge
/// with another note in the same context set (the seed or another candidate),
/// because the canonical and its source should not compete in the same prompt.
///
/// Concretely:
///   * If the candidate's `supersedes` neighbor is the **seed**, the candidate
///     is always dropped (the seed is primary and always kept).
///   * If the candidate's `supersedes` neighbor is another **candidate**, the
///     one with the lower id is dropped (source convention: `note_a_id` is the
///     source, `note_b_id` is the canonical).
///
/// This satisfies both test scenarios:
///   - Seed = canonical, source is a candidate → the candidate (source) is
///     dropped because its neighbor (the canonical seed) is in the result set.
///   - Seed = source, canonical is a candidate → the candidate (canonical) is
///     dropped because its neighbor (the source seed) is in the result set.
fn is_superseded_by_sibling(
    note_id: &str,
    supersedes_map: &HashMap<String, Vec<String>>,
    context_ids: &HashSet<String>,
    seed_id: &str,
) -> bool {
    let Some(neighbors) = supersedes_map.get(note_id) else {
        return false;
    };
    for neighbor in neighbors {
        // The neighbor must be in the current result set.
        if !context_ids.contains(neighbor) {
            continue;
        }
        // If the neighbor is the seed, this candidate is always dropped
        // (the seed is primary and kept regardless of direction).
        if neighbor == seed_id {
            return true;
        }
        // If the neighbor is another candidate, drop the lower-id note
        // (source convention: lower id = source, higher id = canonical).
        if note_id < neighbor.as_str() {
            return true;
        }
    }
    false
}

/// Apply budget pruning to L1 and L0 notes.
///
/// Budget allocation:
/// - Seeds: uncapped (always included)
/// - L1: 60% of remaining budget
/// - L0: 40% of remaining budget
///
/// Notes are pruned from the tail (lowest ranked) first within each tier.
fn apply_budget_pruning(
    l1_notes: Vec<(NoteOverview, usize)>,
    l0_notes: Vec<(NoteAbstract, usize)>,
    budget: usize,
    seed_content: &str,
) -> (Vec<NoteOverview>, Vec<NoteAbstract>) {
    // Calculate seed token usage
    let seed_tokens = seed_content.len() / 4;

    // Calculate remaining budget for related notes
    let remaining_budget = budget.saturating_sub(seed_tokens);

    // Allocate 60% to L1, 40% to L0
    let l1_budget = (remaining_budget * 60) / 100;
    let l0_budget = remaining_budget - l1_budget; // Remainder goes to L0

    // Prune L1 notes from tail
    let l1_result = prune_notes(l1_notes, l1_budget);

    // Prune L0 notes from tail
    let l0_result = prune_notes(l0_notes, l0_budget);

    (l1_result, l0_result)
}

/// Prune notes from the tail (lowest ranked last) to fit within budget.
/// Assumes notes are sorted by rank (highest first).
fn prune_notes<T>(notes: Vec<(T, usize)>, budget: usize) -> Vec<T> {
    let mut result = Vec::new();
    let mut used = 0usize;

    for (note, tokens) in notes {
        if used + tokens <= budget {
            result.push(note);
            used += tokens;
        } else {
            // Can't fit this note, skip (and all remaining since they're lower ranked)
            break;
        }
    }

    result
}

/// Calculate age in days from timestamp string.
fn age_days(timestamp: &str, now: std::time::SystemTime) -> f64 {
    let Ok(duration) = now.duration_since(std::time::UNIX_EPOCH) else {
        return f64::EPSILON;
    };
    let now_unix = duration.as_secs_f64();

    let timestamp = timestamp.trim().trim_end_matches('Z');
    let Some((date_part, time_part)) = timestamp
        .split_once(' ')
        .or_else(|| timestamp.split_once('T'))
    else {
        return f64::EPSILON;
    };
    let Some((y, m, d)) = parse_ymd(date_part) else {
        return f64::EPSILON;
    };
    let Some((hh, mm, ss)) = parse_hms(time_part) else {
        return f64::EPSILON;
    };

    let days = days_from_civil(y, m, d);
    let timestamp_unix = days as f64 * 86_400.0 + (hh as f64 * 3600.0) + (mm as f64 * 60.0) + ss;
    let seconds = (now_unix - timestamp_unix).max(0.0);
    (seconds / 86_400.0).max(f64::EPSILON)
}

fn parse_ymd(value: &str) -> Option<(i32, u32, u32)> {
    let mut parts = value.split('-');
    let y = parts.next()?.parse::<i32>().ok()?;
    let m = parts.next()?.parse::<u32>().ok()?;
    let d = parts.next()?.parse::<u32>().ok()?;
    Some((y, m, d))
}

fn parse_hms(value: &str) -> Option<(u32, u32, f64)> {
    let mut parts = value.split(':');
    let hh = parts.next()?.parse::<u32>().ok()?;
    let mm = parts.next()?.parse::<u32>().ok()?;
    let ss = parts.next()?.parse::<f64>().ok()?;
    Some((hh, mm, ss))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = year - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = month as i32 + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day as i32 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era as i64) * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::DEFAULT_MIN_CONFIDENCE;
    use crate::repositories::note::NoteAssociationKind;
    use crate::repositories::note::scoring::STALE_CITATION;
    use crate::{Database, NoteRepository, ProjectRepository};
    use djinn_core::events::EventBus;
    use tokio::sync::broadcast;

    async fn setup_repo() -> (tempfile::TempDir, NoteRepository, String) {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel::<djinn_core::events::DjinnEventEnvelope>(16);
        let bus = {
            let tx = tx.clone();
            EventBus::new(move |event| {
                let _ = tx.send(event);
            })
        };

        db.ensure_initialized().await.unwrap();
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let repo = NoteRepository::new(db, bus);
        (tmp, repo, project.id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_default_min_confidence_excludes_very_low_confidence() {
        let (_tmp, repo, project_id) = setup_repo().await;

        let seed = repo
            .create(
                &project_id,
                "Seed",
                "Seed about architecture patterns.",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Create a note with confidence below default threshold (0.1)
        let low = repo
            .create(
                &project_id,
                "Low Conf",
                "architecture patterns low content referencing [[Seed]].",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        repo.set_confidence(&low.id, 0.05).await.unwrap();

        // Create a normal note
        let _normal = repo
            .create(
                &project_id,
                "Normal Conf",
                "architecture patterns normal content referencing [[Seed]].",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        let result = repo
            .build_context(
                &project_id,
                &seed.permalink,
                Some(8192),
                None,
                20,
                None,
                None,
            )
            .await
            .unwrap();

        let all_ids: Vec<&str> = result
            .related_l1
            .iter()
            .map(|n| n.id.as_str())
            .chain(result.related_l0.iter().map(|n| n.id.as_str()))
            .collect();

        assert!(
            !all_ids.contains(&low.id.as_str()),
            "note with confidence 0.05 should be excluded with default min_confidence={}",
            DEFAULT_MIN_CONFIDENCE,
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_min_confidence_zero_includes_all() {
        let (_tmp, repo, project_id) = setup_repo().await;

        let seed = repo
            .create(
                &project_id,
                "Seed",
                "Seed about architecture patterns.",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        let low = repo
            .create(
                &project_id,
                "Low Conf",
                "architecture patterns low content referencing [[Seed]].",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        repo.set_confidence(&low.id, 0.05).await.unwrap();

        let result = repo
            .build_context(
                &project_id,
                &seed.permalink,
                Some(8192),
                None,
                20,
                Some(0.0),
                None,
            )
            .await
            .unwrap();

        let all_ids: Vec<&str> = result
            .related_l1
            .iter()
            .map(|n| n.id.as_str())
            .chain(result.related_l0.iter().map(|n| n.id.as_str()))
            .collect();

        assert!(
            all_ids.contains(&low.id.as_str()),
            "note with confidence 0.05 should be included with min_confidence=0.0",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_stale_citation_notes_annotated_superseded() {
        let (_tmp, repo, project_id) = setup_repo().await;

        let seed = repo
            .create(
                &project_id,
                "Seed",
                "Seed about architecture patterns.",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        let stale = repo
            .create(
                &project_id,
                "Stale Note",
                "architecture patterns stale content referencing [[Seed]].",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        repo.set_confidence(&stale.id, STALE_CITATION)
            .await
            .unwrap();

        let normal = repo
            .create(
                &project_id,
                "Normal Note",
                "architecture patterns normal content referencing [[Seed]].",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Use min_confidence=0.0 to include everything so we can test annotations
        let result = repo
            .build_context(
                &project_id,
                &seed.permalink,
                Some(8192),
                None,
                20,
                Some(0.0),
                None,
            )
            .await
            .unwrap();

        // Find stale note in results
        let stale_l1 = result.related_l1.iter().find(|n| n.id == stale.id);
        let stale_l0 = result.related_l0.iter().find(|n| n.id == stale.id);

        if let Some(note) = stale_l1 {
            assert!(note.superseded, "stale L1 note should be superseded");
            assert!(
                note.overview_text.starts_with("[SUPERSEDED]"),
                "stale L1 note should have [SUPERSEDED] prefix",
            );
        } else if let Some(note) = stale_l0 {
            assert!(note.superseded, "stale L0 note should be superseded");
            assert!(
                note.abstract_text.starts_with("[SUPERSEDED]"),
                "stale L0 note should have [SUPERSEDED] prefix",
            );
        } else {
            panic!("stale note should appear in results with min_confidence=0.0");
        }

        // Normal note should not be superseded
        let normal_l1 = result.related_l1.iter().find(|n| n.id == normal.id);
        let normal_l0 = result.related_l0.iter().find(|n| n.id == normal.id);
        if let Some(note) = normal_l1 {
            assert!(!note.superseded, "normal L1 note should not be superseded");
        }
        if let Some(note) = normal_l0 {
            assert!(!note.superseded, "normal L0 note should not be superseded");
        }
    }

    // ─── Supersession retrieval exclusion (yk9t dm4w) ───────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_excludes_superseded_source_when_canonical_is_seed() {
        let (_tmp, repo, project_id) = setup_repo().await;

        // Create a canonical note as the seed.
        let canonical = repo
            .create(
                &project_id,
                "Canonical Seed",
                "architecture patterns canonical seed content about design decisions.",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Create a source note that the canonical supersedes. Its content
        // overlaps with the seed so it appears in RRF results.
        let source = repo
            .create(
                &project_id,
                "Superseded Source",
                "architecture patterns canonical seed content about design decisions superseded.",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Record the supersession edge: canonical supersedes source.
        repo.record_supersedes(&canonical.id, &source.id, 1.0)
            .await
            .unwrap();

        // Build context with the canonical note as seed. Use min_confidence=0.0
        // so the confidence filter does not interfere.
        let result = repo
            .build_context(
                &project_id,
                &canonical.permalink,
                Some(8192),
                None,
                20,
                Some(0.0),
                None,
            )
            .await
            .unwrap();

        // The canonical seed is always returned as primary.
        assert_eq!(result.primary.len(), 1);
        assert_eq!(result.primary[0].id, canonical.id);

        // The superseded source must NOT appear in L0/L1 results.
        let all_related_ids: Vec<&str> = result
            .related_l1
            .iter()
            .map(|n| n.id.as_str())
            .chain(result.related_l0.iter().map(|n| n.id.as_str()))
            .collect();
        assert!(
            !all_related_ids.contains(&source.id.as_str()),
            "superseded source should be excluded from build_context results \
             when the canonical note is the seed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_excludes_canonical_when_source_is_seed() {
        let (_tmp, repo, project_id) = setup_repo().await;

        // Create a canonical note.
        let canonical = repo
            .create(
                &project_id,
                "Canonical Note",
                "architecture patterns canonical content about design decisions and tradeoffs.",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Create a source note that the canonical supersedes. Use it as seed.
        let source = repo
            .create(
                &project_id,
                "Source Seed",
                "architecture patterns canonical content about design decisions and tradeoffs superseded.",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Record the supersession edge: canonical supersedes source.
        repo.record_supersedes(&canonical.id, &source.id, 1.0)
            .await
            .unwrap();

        // Build context with the SOURCE note as seed.
        let result = repo
            .build_context(
                &project_id,
                &source.permalink,
                Some(8192),
                None,
                20,
                Some(0.0),
                None,
            )
            .await
            .unwrap();

        // The source seed is always returned as primary.
        assert_eq!(result.primary.len(), 1);
        assert_eq!(result.primary[0].id, source.id);

        // The canonical must NOT appear in L0/L1 results — it shares a
        // supersedes edge with the seed.
        let all_related_ids: Vec<&str> = result
            .related_l1
            .iter()
            .map(|n| n.id.as_str())
            .chain(result.related_l0.iter().map(|n| n.id.as_str()))
            .collect();
        assert!(
            !all_related_ids.contains(&canonical.id.as_str()),
            "canonical should be excluded from build_context results \
             when the superseded source is the seed"
        );
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_surfaces_supersedes_and_contradicts_annotations() {
        let (_tmp, repo, project_id) = setup_repo().await;

        let contradictor = repo
            .create(
                &project_id,
                "Contradictor",
                "contradicting architecture patterns context note.",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let source_seed = repo
            .create(
                &project_id,
                "Source Seed",
                "architecture patterns source seed links to [[Contradictor]].",
                "adr",
                "[]",
            )
            .await
            .unwrap();
        let canonical = repo
            .create(
                &project_id,
                "Canonical Replacement",
                "architecture patterns canonical replacement.",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        repo.upsert_typed_association(
            &source_seed.id,
            &canonical.id,
            NoteAssociationKind::Supersedes,
            1.0,
        )
        .await
        .unwrap();
        repo.upsert_typed_association(
            &source_seed.id,
            &contradictor.id,
            NoteAssociationKind::Contradicts,
            0.9,
        )
        .await
        .unwrap();

        let result = repo
            .build_context(
                &project_id,
                &source_seed.permalink,
                Some(8192),
                None,
                20,
                Some(0.0),
                None,
            )
            .await
            .unwrap();

        assert!(
            result.supersedes.iter().any(|annotation| {
                annotation.superseded_by == canonical.id && annotation.edge_kind == "supersedes"
            }),
            "expected source seed to be annotated as superseded by canonical note: {:?}",
            result.supersedes
        );
        assert!(
            result.contradicts.iter().any(|annotation| {
                annotation.note_id == contradictor.id && annotation.edge_kind == "contradicts"
            }),
            "expected contradictor annotation in build_context response: {:?}",
            result.contradicts
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_surfaces_relevant_proposal_for_seed_with_matching_proposal_targets() {
        use crate::repositories::proposal::{ProposalCreateInput, ProposalRepository};

        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel::<djinn_core::events::DjinnEventEnvelope>(16);
        let bus = {
            let tx = tx.clone();
            EventBus::new(move |event| {
                let _ = tx.send(event);
            })
        };

        db.ensure_initialized().await.unwrap();
        let proj_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let project = proj_repo
            .create("test-project", "test", "test-project")
            .await
            .unwrap();
        let project_id = project.id.clone();
        let repo = NoteRepository::new(db.clone(), bus);
        let _tmp = tmp;

        // Create a seed note with distinctive content.
        let seed = repo
            .create(
                &project_id,
                "Widget Refactor Plan",
                "refactor the widget_zqk module to support streaming output and batched writes",
                "adr",
                "[]",
            )
            .await
            .unwrap();

        // Create a proposal with overlapping body text.
        let proposal_repo =
            ProposalRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let proposal = proposal_repo
            .create(ProposalCreateInput {
                title: "Widget Refactor Proposal",
                body: "The widget_zqk module needs streaming output support and batched writes for performance",
                acceptance_criteria: Some(r#"["streaming output works", "batched writes reduce latency"]"#),
                status: Some("approved"),
                body_format: None,
            })
            .await
            .unwrap();

        // Wire the proposal to this project via proposal_targets so the
        // title-body match path can find it.
        sqlx::query(
            "INSERT INTO proposal_targets (proposal_id, project_id, role) VALUES ($1, $2, 'primary')",
        )
        .bind(&proposal.id)
        .bind(&project_id)
        .execute(db.pool())
        .await
        .unwrap();

        let result = repo
            .build_context(
                &project_id,
                &seed.permalink,
                Some(8192),
                None,
                20,
                Some(0.0),
                None,
            )
            .await
            .unwrap();

        assert!(
            !result.proposals.is_empty(),
            "build_context should surface relevant proposals, got empty: seed={:?}",
            seed.permalink,
        );
        let found = result
            .proposals
            .iter()
            .find(|p| p.short_id == proposal.short_id);
        assert!(
            found.is_some(),
            "expected proposal {} (short_id={}) in build_context proposals: {:?}",
            proposal.id,
            proposal.short_id,
            result.proposals,
        );
        let overview = found.unwrap();
        assert_eq!(overview.title, "Widget Refactor Proposal");
        assert_eq!(overview.body_format, "markdown");
        assert_eq!(overview.status, "approved");
        assert_eq!(
            overview.acceptance_criteria,
            vec![
                "streaming output works".to_string(),
                "batched writes reduce latency".to_string()
            ]
        );
        assert!(
            overview.score.is_some(),
            "proposal overview should carry a score"
        );
    }
}
