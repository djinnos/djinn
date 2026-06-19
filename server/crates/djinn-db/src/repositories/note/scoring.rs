use std::collections::{HashMap, HashSet, VecDeque};
use std::time::SystemTime;

use djinn_memory::ContradictionWarning;

use crate::error::DbResult as Result;

use super::NoteRepository;

pub(crate) const HOP_DECAY: f64 = 0.7;
const HOTNESS_ALPHA: f64 = 0.2;
const HALF_LIFE_DAYS: f64 = 7.0;
const MIN_ASSOCIATION_WEIGHT: f64 = 0.05;

pub const CONFIDENCE_FLOOR: f64 = 0.025;
pub const CONFIDENCE_CEILING: f64 = 0.975;

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "Used by intra-crate tests in note::mod and scoring::tests"
    )
)]
pub(crate) const TASK_SUCCESS: f64 = 0.65;
pub const CO_ACCESS_HIGH: f64 = 0.65;
pub const STALE_CITATION: f64 = 0.3;
pub const USER_CONFIRM: f64 = 0.95;
pub const CONTRADICTION: f64 = 0.1;

/// Bayesian signal applied per decay iteration to stale extracted notes.
///
/// Chosen between `0.1` (`CONTRADICTION`) and `0.3` (`STALE_CITATION`) so a
/// single `bayesian_update` step moves the posterior toward the floor without
/// collapsing it in one shot. Repeated application drives an unaccessed
/// extracted note below `STALE_CITATION` within the per-tick iteration cap.
pub const STALE_DECAY_SIGNAL: f64 = 0.15;

/// Scales the stale-decay signal with the number of days elapsed since the
/// note was last accessed. Notes just past the window get a gentler signal;
/// long-dormant notes get the full `STALE_DECAY_SIGNAL`. The result is clamped
/// to `[0.1, STALE_DECAY_SIGNAL]` so the signal never exceeds the configured
/// ceiling and never drops to a value that would be a no-op underflow.
///
/// `days` is expected to be `>= 0`; negative values are clamped to `0`.
pub fn decay_signal_for_elapsed_days(days: f64) -> f64 {
    let days = days.max(0.0);
    // Linear ramp from 0.1 at the window boundary to STALE_DECAY_SIGNAL once
    // the note is ~90 days past the window. The ramp keeps recent-window
    // boundary notes from being slammed while ensuring very stale notes hit
    // the full signal quickly.
    let ramp_span = 90.0_f64;
    let t = (days / ramp_span).clamp(0.0, 1.0);
    let scaled = CONTRADICTION + t * (STALE_DECAY_SIGNAL - CONTRADICTION);
    scaled.clamp(CONTRADICTION, STALE_DECAY_SIGNAL)
}

pub fn bayesian_update(prior: f64, signal: f64) -> f64 {
    let posterior = (prior * signal) / (prior * signal + (1.0 - prior) * (1.0 - signal));
    posterior.clamp(CONFIDENCE_FLOOR, CONFIDENCE_CEILING)
}

/// Per-kind multiplier for a single hop in spreading activation.
///
/// Returns the raw multiplier to be applied by the caller. For symmetric kinds
/// (`co_access`, `derived_from`, `builds_on`, `exemplifies`), this is the same
/// in both directions. For `contradicts`, returns 0.0 (no score contribution).
/// For `supersedes`, returns the source→target demotion (-0.5); the reverse
/// direction (target→source +0.2) is handled by [`supersedes_reverse_multiplier`].
pub(crate) fn multiplier_for_kind(kind: &str, weight: f64) -> f64 {
    match kind {
        "co_access" => HOP_DECAY * weight,
        "derived_from" => HOP_DECAY * 1.0 * weight,
        "builds_on" => HOP_DECAY * 0.8 * weight,
        "exemplifies" => HOP_DECAY * 0.7 * weight,
        "contradicts" => 0.0,
        "supersedes" => -0.5,    // source→target; asymmetry is at the caller
        _ => HOP_DECAY * weight, // unknown kinds default to co_access behavior
    }
}

/// The reverse-direction multiplier for a `supersedes` edge (target→source).
/// A single hop from the target through a `supersedes` edge gives the source
/// a `+0.2` boost (the canonical note is preferred).
pub(crate) fn supersedes_reverse_multiplier() -> f64 {
    0.2
}

#[derive(Clone)]
struct ProximityEdge {
    target: String,
    multiplier: f64,
}

impl NoteRepository {
    /// Directly set the confidence of a note to `value`, clamped to
    /// `[CONFIDENCE_FLOOR, CONFIDENCE_CEILING]`.
    ///
    /// Unlike `update_confidence` (which applies a Bayesian signal update),
    /// this sets the absolute value. Use this when the initial confidence is
    /// known at creation time rather than derived from a signal (e.g. session-
    /// extracted notes that start at 0.5 rather than the human-written default
    /// of 1.0).
    pub async fn set_confidence(&self, note_id: &str, value: f64) -> Result<()> {
        self.db.ensure_initialized().await?;

        let clamped = value.clamp(CONFIDENCE_FLOOR, CONFIDENCE_CEILING);

        sqlx::query!(
            "UPDATE notes SET confidence = $1 WHERE id = $2",
            clamped,
            note_id
        )
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    pub async fn update_confidence(&self, note_id: &str, signal: f64) -> Result<f64> {
        self.db.ensure_initialized().await?;

        let prior = sqlx::query_scalar!("SELECT confidence FROM notes WHERE id = $1", note_id)
            .fetch_one(self.db.pool())
            .await?;

        let posterior = bayesian_update(prior, signal);

        sqlx::query!(
            "UPDATE notes SET confidence = $1 WHERE id = $2",
            posterior,
            note_id
        )
        .execute(self.db.pool())
        .await?;

        Ok(posterior)
    }

    pub async fn note_confidence_map(&self, note_ids: &[String]) -> Result<HashMap<String, f64>> {
        if note_ids.is_empty() {
            return Ok(HashMap::new());
        }

        // Postgres $N binds; no fixed params precede the IN list.
        let sql = format!(
            "SELECT id, confidence FROM notes WHERE id IN ({})",
            crate::repositories::pg_placeholders(note_ids.len(), 1)
        );

        // NOTE: dynamic SQL — compile-time check not possible (runtime IN list)
        let mut query = sqlx::query_as::<_, (String, f64)>(&sql);
        for id in note_ids {
            query = query.bind(id);
        }

        Ok(query.fetch_all(self.db.pool()).await?.into_iter().collect())
    }

    pub async fn temporal_scores(
        &self,
        project_id: &str,
        candidate_ids: &[String],
    ) -> Result<Vec<(String, f64)>> {
        if candidate_ids.is_empty() {
            return Ok(Vec::new());
        }

        // project_id is bound as $1, so the IN list starts at $2.
        let query = format!(
            "SELECT id, access_count, created_at, updated_at
             FROM notes
             WHERE project_id = $1 AND id IN ({})",
            crate::repositories::pg_placeholders(candidate_ids.len(), 2)
        );

        // NOTE: dynamic SQL — compile-time check not possible (runtime IN list)
        let mut q = sqlx::query_as::<_, (String, i64, String, String)>(&query).bind(project_id);
        for id in candidate_ids {
            q = q.bind(id);
        }

        let rows = q.fetch_all(self.db.pool()).await?;
        let now = SystemTime::now();

        let mut scores: Vec<(String, f64)> = rows
            .into_iter()
            .map(|(id, access_count, created_at, updated_at)| {
                let created_age_days = age_days_from_timestamp(&created_at, now);
                let updated_age_days = age_days_from_timestamp(&updated_at, now);

                let safe_created_age = created_age_days.max(f64::EPSILON);
                let safe_updated_age = updated_age_days.max(f64::EPSILON);

                let base_actr = ((access_count.max(0) as f64) + 1.0).ln() - safe_created_age.ln();
                let recency_boost = 2f64.powf(-(safe_updated_age / HALF_LIFE_DAYS));
                let hotness = HOTNESS_ALPHA * recency_boost;
                let score = base_actr + hotness;

                (id, score)
            })
            .collect();

        scores.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(scores)
    }

    /// Backward-compatible wrapper: calls `graph_proximity_scores_with_edge_kinds`
    /// with `edge_kinds=None` and drops warnings.
    pub async fn graph_proximity_scores(
        &self,
        seed_ids: &[String],
        max_hops: usize,
    ) -> Result<Vec<(String, f64)>> {
        let (scores, _warnings) = self
            .graph_proximity_scores_with_edge_kinds(seed_ids, max_hops, None)
            .await?;
        Ok(scores)
    }

    /// Kind-aware graph proximity scoring with optional edge-kind filtering.
    ///
    /// When `edge_kinds` is `Some`, only edges whose `kind` appears in the list
    /// participate in spreading activation. `None` means all kinds.
    ///
    /// Returns `(scores, warnings)` where `warnings` contains `ContradictionWarning`
    /// entries for any `contradicts` edges encountered during traversal.
    pub async fn graph_proximity_scores_with_edge_kinds(
        &self,
        seed_ids: &[String],
        max_hops: usize,
        edge_kinds: Option<&[String]>,
    ) -> Result<(Vec<(String, f64)>, Vec<ContradictionWarning>)> {
        if seed_ids.is_empty() || max_hops == 0 {
            return Ok((Vec::new(), Vec::new()));
        }

        let seed0 = &seed_ids[0];
        let project_id =
            sqlx::query_scalar!("SELECT project_id FROM notes WHERE id = $1 LIMIT 1", seed0)
                .fetch_optional(self.db.pool())
                .await?
                .unwrap_or_default();

        if project_id.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let link_edges: Vec<(String, String)> = sqlx::query_as::<_, (String, String)>(
            r#"SELECT source_id, target_id AS "target_id!: String" FROM note_links WHERE target_id IS NOT NULL AND source_id IN (
                SELECT id FROM notes WHERE project_id = $1 AND status = 'active'
            ) AND target_id IN (
                SELECT id FROM notes WHERE project_id = $2 AND status = 'active'
            )"#,
        )
        .bind(&project_id)
        .bind(&project_id)
        .fetch_all(self.db.pool())
        .await?
        .into_iter()
        .collect();

        // Fetch association edges with kind projected.
        let association_edges: Vec<(String, String, f64, String)> =
            sqlx::query_as::<_, (String, String, f64, String)>(
                "SELECT note_a_id, note_b_id, weight, kind
             FROM note_associations
             WHERE weight >= $1
               AND note_a_id IN (SELECT id FROM notes WHERE project_id = $2 AND status = 'active')
               AND note_b_id IN (SELECT id FROM notes WHERE project_id = $3 AND status = 'active')",
            )
            .bind(MIN_ASSOCIATION_WEIGHT)
            .bind(&project_id)
            .bind(&project_id)
            .fetch_all(self.db.pool())
            .await?
            .into_iter()
            .collect();

        // Build edge-kind filter set.
        let kind_filter: Option<HashSet<&str>> =
            edge_kinds.map(|kinds| kinds.iter().map(|s| s.as_str()).collect());

        let mut warnings: Vec<ContradictionWarning> = Vec::new();

        let mut adjacency: HashMap<String, Vec<ProximityEdge>> = HashMap::new();

        // Wikilink edges: always participate, symmetric with HOP_DECAY.
        for (source, target) in link_edges {
            adjacency
                .entry(source.clone())
                .or_default()
                .push(ProximityEdge {
                    target: target.clone(),
                    multiplier: HOP_DECAY,
                });
            adjacency.entry(target).or_default().push(ProximityEdge {
                target: source,
                multiplier: HOP_DECAY,
            });
        }

        // Association edges: apply per-kind multipliers and filtering.
        for (note_a_id, note_b_id, weight, kind) in association_edges {
            // If a kind filter is active and this kind is not in the set, skip it
            // (except `contradicts` which always generates a warning).
            let dominated_by_filter = kind_filter
                .as_ref()
                .is_some_and(|filter| !filter.contains(kind.as_str()));

            if kind == "contradicts" {
                // Always record contradiction warnings regardless of filter.
                warnings.push(ContradictionWarning {
                    source_id: note_a_id.clone(),
                    target_id: note_b_id.clone(),
                    kind: kind.clone(),
                });
                // contradicts does NOT participate in scoring.
                continue;
            }

            if dominated_by_filter {
                continue;
            }

            if kind == "supersedes" {
                // Asymmetric: note_a (source) → note_b (target) gets -0.5
                //              note_b (target) → note_a (source) gets +0.2
                adjacency
                    .entry(note_a_id.clone())
                    .or_default()
                    .push(ProximityEdge {
                        target: note_b_id.clone(),
                        multiplier: multiplier_for_kind("supersedes", weight),
                    });
                adjacency
                    .entry(note_b_id.clone())
                    .or_default()
                    .push(ProximityEdge {
                        target: note_a_id.clone(),
                        multiplier: supersedes_reverse_multiplier(),
                    });
            } else {
                // Symmetric kinds.
                let multiplier = multiplier_for_kind(&kind, weight);
                adjacency
                    .entry(note_a_id.clone())
                    .or_default()
                    .push(ProximityEdge {
                        target: note_b_id.clone(),
                        multiplier,
                    });
                adjacency.entry(note_b_id).or_default().push(ProximityEdge {
                    target: note_a_id,
                    multiplier,
                });
            }
        }

        let seed_set: HashSet<String> = seed_ids.iter().cloned().collect();
        let mut best_scores: HashMap<String, f64> = HashMap::new();
        let mut queue: VecDeque<(String, usize, f64)> = VecDeque::new();

        for seed in seed_ids {
            queue.push_back((seed.clone(), 0, 1.0));
        }

        while let Some((node, depth, score)) = queue.pop_front() {
            if depth >= max_hops {
                continue;
            }

            if let Some(neighbors) = adjacency.get(&node) {
                for neighbor in neighbors {
                    let next_depth = depth + 1;
                    let next_score = score * neighbor.multiplier;

                    let current_best = best_scores.get(&neighbor.target).copied().unwrap_or(0.0);
                    if next_score > current_best {
                        best_scores.insert(neighbor.target.clone(), next_score);
                        queue.push_back((neighbor.target.clone(), next_depth, next_score));
                    }
                }
            }
        }

        let active_ids: HashSet<String> =
            sqlx::query_scalar("SELECT id FROM notes WHERE project_id = $1 AND status = 'active'")
                .bind(project_id)
                .fetch_all(self.db.pool())
                .await?
                .into_iter()
                .collect();

        let mut results: Vec<(String, f64)> = best_scores
            .into_iter()
            .filter(|(id, _)| !seed_set.contains(id) && active_ids.contains(id))
            .collect();

        results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok((results, warnings))
    }
}

fn age_days_from_timestamp(value: &str, now: SystemTime) -> f64 {
    let Ok(duration) = now.duration_since(SystemTime::UNIX_EPOCH) else {
        return f64::EPSILON;
    };
    let now_unix = duration.as_secs_f64();

    let value = value.trim().trim_end_matches('Z');
    let Some((date_part, time_part)) = value.split_once(' ').or_else(|| value.split_once('T'))
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
    use super::*;

    #[test]
    fn bayesian_update_low_signal_reduces_from_near_one() {
        let updated = bayesian_update(0.95, 0.1);
        assert!(
            updated < 0.7,
            "expected a significant decrease, got {updated}"
        );
        assert!(updated >= CONFIDENCE_FLOOR);
    }

    #[test]
    fn bayesian_update_medium_positive_signal_increases_from_half() {
        let updated = bayesian_update(0.5, TASK_SUCCESS);
        assert!(updated > 0.5);
    }

    #[test]
    fn repeated_low_signals_never_cross_floor() {
        let mut confidence = 0.5;
        for _ in 0..50 {
            confidence = bayesian_update(confidence, CONTRADICTION);
        }
        assert!(confidence >= CONFIDENCE_FLOOR);
        assert!((confidence - CONFIDENCE_FLOOR).abs() < 1e-9);
    }

    #[test]
    fn repeated_high_signals_never_cross_ceiling() {
        let mut confidence = 0.5;
        for _ in 0..50 {
            confidence = bayesian_update(confidence, USER_CONFIRM);
        }
        assert!(confidence <= CONFIDENCE_CEILING);
        assert!((confidence - CONFIDENCE_CEILING).abs() < 1e-9);
    }

    // ── Per-kind multiplier tests ────────────────────────────────────────────

    #[test]
    fn multiplier_co_access_uses_hop_decay_times_weight() {
        let weight = 0.8;
        let result = multiplier_for_kind("co_access", weight);
        let expected = HOP_DECAY * weight;
        assert!(
            (result - expected).abs() < 1e-12,
            "co_access: expected {expected}, got {result}"
        );
    }

    #[test]
    fn multiplier_derived_from_preserves_existing_behavior() {
        let weight = 1.0;
        let result = multiplier_for_kind("derived_from", weight);
        let expected = HOP_DECAY * 1.0 * weight;
        assert!(
            (result - expected).abs() < 1e-12,
            "derived_from: expected {expected}, got {result}"
        );
    }

    #[test]
    fn multiplier_builds_on_uses_0_8_factor() {
        let weight = 1.0;
        let result = multiplier_for_kind("builds_on", weight);
        let expected = HOP_DECAY * 0.8 * weight;
        assert!(
            (result - expected).abs() < 1e-12,
            "builds_on: expected {expected}, got {result}"
        );
    }

    #[test]
    fn multiplier_exemplifies_uses_0_7_factor() {
        let weight = 1.0;
        let result = multiplier_for_kind("exemplifies", weight);
        let expected = HOP_DECAY * 0.7 * weight;
        assert!(
            (result - expected).abs() < 1e-12,
            "exemplifies: expected {expected}, got {result}"
        );
    }

    #[test]
    fn multiplier_contradicts_returns_zero() {
        let result = multiplier_for_kind("contradicts", 0.9);
        assert!(
            result.abs() < 1e-12,
            "contradicts should return 0.0, got {result}"
        );
    }

    #[test]
    fn multiplier_supersedes_source_to_target_is_negative_half() {
        let result = multiplier_for_kind("supersedes", 1.0);
        let expected = -0.5;
        assert!(
            (result - expected).abs() < 1e-12,
            "supersedes source→target: expected {expected}, got {result}"
        );
    }

    #[test]
    fn multiplier_supersedes_reverse_is_positive_0_2() {
        let result = supersedes_reverse_multiplier();
        let expected = 0.2;
        assert!(
            (result - expected).abs() < 1e-12,
            "supersedes target→source: expected {expected}, got {result}"
        );
    }

    #[test]
    fn multiplier_unknown_kind_defaults_to_co_access() {
        let weight = 0.6;
        let result = multiplier_for_kind("some_future_kind", weight);
        let expected = HOP_DECAY * weight;
        assert!(
            (result - expected).abs() < 1e-12,
            "unknown kind: expected {expected}, got {result}"
        );
    }
}
