use std::collections::{HashMap, HashSet, VecDeque};
use std::time::SystemTime;

use crate::error::DbResult as Result;

use super::NoteRepository;

const HOP_DECAY: f64 = 0.7;
const HOTNESS_ALPHA: f64 = 0.2;
const HALF_LIFE_DAYS: f64 = 7.0;
const MIN_ASSOCIATION_WEIGHT: f64 = 0.05;

#[allow(dead_code)]
pub const CONFIDENCE_FLOOR: f64 = 0.025;
#[allow(dead_code)]
pub const CONFIDENCE_CEILING: f64 = 0.975;

#[allow(dead_code)]
pub const TASK_SUCCESS: f64 = 0.65;
#[allow(dead_code)]
pub const CO_ACCESS_HIGH: f64 = 0.65;
#[allow(dead_code)]
pub const USER_CONFIRM: f64 = 0.95;
#[allow(dead_code)]
pub const CONTRADICTION: f64 = 0.1;
#[allow(dead_code)]
pub const TASK_FAILURE: f64 = 0.1;
#[allow(dead_code)]
pub const STALE_CITATION: f64 = 0.3;

pub fn bayesian_update(prior: f64, signal: f64) -> f64 {
    let posterior = (prior * signal) / (prior * signal + (1.0 - prior) * (1.0 - signal));
    posterior.clamp(CONFIDENCE_FLOOR, CONFIDENCE_CEILING)
}

#[derive(Clone)]
struct ProximityEdge {
    target: String,
    multiplier: f64,
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

        sqlx::query("UPDATE notes SET confidence = ?1 WHERE id = ?2")
            .bind(clamped)
            .bind(note_id)
            .execute(self.db.pool())
            .await?;

        Ok(())
    }

    pub async fn update_confidence(&self, note_id: &str, signal: f64) -> Result<f64> {
        self.db.ensure_initialized().await?;

        let prior = sqlx::query_scalar::<_, f64>("SELECT confidence FROM notes WHERE id = ?1")
            .bind(note_id)
            .fetch_one(self.db.pool())
            .await?;

        let posterior = bayesian_update(prior, signal);

        sqlx::query("UPDATE notes SET confidence = ?1 WHERE id = ?2")
            .bind(posterior)
            .bind(note_id)
            .execute(self.db.pool())
            .await?;

        Ok(posterior)
    }

    pub async fn note_confidence_map(&self, note_ids: &[String]) -> Result<HashMap<String, f64>> {
        if note_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let placeholders = std::iter::repeat_n("?", note_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, confidence FROM notes WHERE id IN ({})",
            placeholders
        );

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

        let placeholders = std::iter::repeat_n("?", candidate_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "SELECT id, access_count, created_at, updated_at
             FROM notes
             WHERE project_id = ? AND id IN ({})",
            placeholders
        );

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

    pub async fn graph_proximity_scores(
        &self,
        seed_ids: &[String],
        max_hops: usize,
    ) -> Result<Vec<(String, f64)>> {
        if seed_ids.is_empty() || max_hops == 0 {
            return Ok(Vec::new());
        }

        let project_id =
            sqlx::query_scalar::<_, String>("SELECT project_id FROM notes WHERE id = ?1 LIMIT 1")
                .bind(&seed_ids[0])
                .fetch_optional(self.db.pool())
                .await?
                .unwrap_or_default();

        if project_id.is_empty() {
            return Ok(Vec::new());
        }

        let link_edges: Vec<(String, String)> = sqlx::query_as(
            "SELECT source_id, target_id FROM note_links WHERE target_id IS NOT NULL AND source_id IN (
                SELECT id FROM notes WHERE project_id = ?1
            )",
        )
        .bind(&project_id)
        .fetch_all(self.db.pool())
        .await?;

        let association_edges: Vec<(String, String, f64)> = sqlx::query_as(
            "SELECT note_a_id, note_b_id, weight
             FROM note_associations
             WHERE weight >= ?1
               AND note_a_id IN (SELECT id FROM notes WHERE project_id = ?2)
               AND note_b_id IN (SELECT id FROM notes WHERE project_id = ?2)",
        )
        .bind(MIN_ASSOCIATION_WEIGHT)
        .bind(&project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut adjacency: HashMap<String, Vec<ProximityEdge>> = HashMap::new();
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

        for (note_a_id, note_b_id, weight) in association_edges {
            let multiplier = HOP_DECAY * weight;
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

        let mut results: Vec<(String, f64)> = best_scores
            .into_iter()
            .filter(|(id, _)| !seed_set.contains(id))
            .collect();

        results.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(results)
    }
}

fn age_days_from_timestamp(value: &str, now: SystemTime) -> f64 {
    let Ok(duration) = now.duration_since(SystemTime::UNIX_EPOCH) else {
        return f64::EPSILON;
    };
    let now_unix = duration.as_secs_f64();

    let value = value.trim();
    let Some((date_part, time_part)) = value.split_once(' ') else {
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
