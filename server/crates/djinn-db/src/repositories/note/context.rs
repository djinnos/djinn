use std::collections::{HashMap, HashSet};

use djinn_core::models::{Note, NoteAbstract, NoteOverview};

use crate::error::DbResult as Result;
use crate::repositories::note::rrf::rrf_fuse;
use crate::repositories::note::{
    LexicalSearchMode, build_lexical_search_plan, executable_lexical_search_sql,
    normalize_lexical_score,
};

use super::NoteRepository;

/// Default minimum confidence threshold for related notes in build_context.
/// Notes below this value are excluded from context results. This prevents
/// contradicted or very low-confidence notes from polluting context.
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.1;

/// Tiered context response with budget-aware disclosure.
#[derive(Clone, Debug)]
pub struct BuildContextResponse {
    /// Full-content notes at the seed (primary focus) - never dropped by budget.
    pub primary: Vec<Note>,
    /// L1 overview notes: direct wikilink neighbors using overview text.
    pub related_l1: Vec<NoteOverview>,
    /// L0 abstract notes: discovered non-direct notes via RRF using abstract text.
    pub related_l0: Vec<NoteAbstract>,
}

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
    pub async fn build_context(
        &self,
        project_id: &str,
        seed_permalink: &str,
        budget: Option<usize>,
        task_id: Option<&str>,
        max_related: usize,
        min_confidence: Option<f64>,
    ) -> Result<BuildContextResponse> {
        self.db.ensure_initialized().await?;

        let budget = budget.unwrap_or(4096);

        // Get the seed note with full content
        let Some(seed) = self.get_by_permalink(project_id, seed_permalink).await? else {
            return Ok(BuildContextResponse {
                primary: vec![],
                related_l1: vec![],
                related_l0: vec![],
            });
        };

        // Get direct neighbors via wikilink graph
        let direct_neighbor_ids = self.get_direct_neighbors(project_id, &seed.id).await?;

        // Build direct neighbor set for quick lookup
        let direct_set: HashSet<String> = direct_neighbor_ids.iter().cloned().collect();

        // Run RRF retrieval pipeline to get discovered notes (includes FTS, temporal, graph, task-affinity)
        let discovered_ids = self
            .run_rrf_discovery(project_id, &seed, task_id, max_related * 2)
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

        Ok(BuildContextResponse {
            primary: vec![seed],
            related_l1: pruned_l1,
            related_l0: pruned_l0,
        })
    }

    /// Get direct wikilink neighbors (one hop from seed).
    async fn get_direct_neighbors(&self, project_id: &str, seed_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query_scalar!(
            r#"SELECT target_id AS "target_id!: String" FROM note_links
             WHERE source_id = ?
               AND target_id IS NOT NULL
               AND target_id IN (SELECT id FROM notes WHERE project_id = ?)"#,
            seed_id,
            project_id
        )
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
    ) -> Result<Vec<(String, f64)>> {
        // Build query from seed content (first 200 chars for efficiency)
        let query = seed.content.chars().take(200).collect::<String>();

        // Get FTS candidates
        let fts_candidates = self.fts_candidates(project_id, &query, limit).await?;

        // Get temporal scores for all notes
        let temporal_scores = self.temporal_scores_all(project_id).await?;

        // Get graph proximity scores from seed
        let graph_scores = self
            .graph_proximity_scores(std::slice::from_ref(&seed.id), 2)
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
        let mut q = sqlx::query_as::<sqlx::MySql, (String, f64)>(&sql);
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
        let rows = sqlx::query!(
            "SELECT id, access_count, created_at, updated_at
             FROM notes
             WHERE project_id = ?",
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;
        let rows: Vec<(String, i64, String, String)> = rows
            .into_iter()
            .map(|r| (r.id, r.access_count, r.created_at, r.updated_at))
            .collect();

        use std::time::SystemTime;

        let now = SystemTime::now();
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
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "SELECT id, permalink, title, note_type, COALESCE(overview, substr(content, 1, 500)) as disclosure_text
             FROM notes
             WHERE id IN ({})",
            placeholders
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
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "SELECT id, permalink, title, note_type, COALESCE(`abstract`, substr(content, 1, 100)) as disclosure_text
             FROM notes
             WHERE id IN ({})",
            placeholders
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
            .create("test-project", tmp.path().to_str().unwrap())
            .await
            .unwrap();
        let repo = NoteRepository::new(db, bus);
        (tmp, repo, project.id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn build_context_default_min_confidence_excludes_very_low_confidence() {
        let (tmp, repo, project_id) = setup_repo().await;

        let seed = repo
            .create(
                &project_id,
                tmp.path(),
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
                tmp.path(),
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
                tmp.path(),
                "Normal Conf",
                "architecture patterns normal content referencing [[Seed]].",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        let result = repo
            .build_context(&project_id, &seed.permalink, Some(8192), None, 20, None)
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
        let (tmp, repo, project_id) = setup_repo().await;

        let seed = repo
            .create(
                &project_id,
                tmp.path(),
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
                tmp.path(),
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
        let (tmp, repo, project_id) = setup_repo().await;

        let seed = repo
            .create(
                &project_id,
                tmp.path(),
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
                tmp.path(),
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
                tmp.path(),
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
}
