use std::collections::{HashMap, HashSet, VecDeque};

use crate::error::DbResult as Result;

use super::NoteRepository;

const HOP_DECAY: f64 = 0.7;

impl NoteRepository {
    pub async fn graph_proximity_scores(
        &self,
        seed_ids: &[String],
        max_hops: usize,
    ) -> Result<Vec<(String, f64)>> {
        if seed_ids.is_empty() || max_hops == 0 {
            return Ok(Vec::new());
        }

        let project_id = sqlx::query_scalar::<_, String>(
            "SELECT project_id FROM notes WHERE id = ?1 LIMIT 1",
        )
        .bind(&seed_ids[0])
        .fetch_optional(self.db.pool())
        .await?
        .unwrap_or_default();

        if project_id.is_empty() {
            return Ok(Vec::new());
        }

        let edges: Vec<(String, String)> = sqlx::query_as(
            "SELECT source_id, target_id FROM note_links WHERE target_id IS NOT NULL AND source_id IN (
                SELECT id FROM notes WHERE project_id = ?1
            )",
        )
        .bind(&project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
        for (source, target) in edges {
            adjacency.entry(source.clone()).or_default().push(target.clone());
            adjacency.entry(target).or_default().push(source);
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
                    let next_score = score * HOP_DECAY;

                    let current_best = best_scores.get(neighbor).copied().unwrap_or(0.0);
                    if next_score > current_best {
                        best_scores.insert(neighbor.clone(), next_score);
                        queue.push_back((neighbor.clone(), next_depth, next_score));
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
