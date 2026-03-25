use std::collections::{BTreeMap, BTreeSet, HashMap};

use djinn_core::models::{
    ConsolidationCandidateEdge, ConsolidationCluster, ConsolidationNoteGroup, ConsolidationNoteRef,
};

use super::*;

pub const CONSOLIDATION_DEDUP_THRESHOLD: f64 = -3.0;
pub const DEFAULT_CONSOLIDATION_CLUSTER_MIN_SIZE: usize = 3;

impl NoteRepository {
    /// Enumerate DB-backed knowledge notes grouped by `(project_id, note_type)`.
    pub async fn list_db_consolidation_groups(&self) -> Result<Vec<ConsolidationNoteGroup>> {
        self.db.ensure_initialized().await?;

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
            ),
        >(
            "SELECT id, project_id, note_type, permalink, title, abstract, overview
             FROM notes
             WHERE storage = 'db'
               AND note_type IN ('case', 'pattern', 'pitfall')
             ORDER BY project_id, note_type, permalink, id",
        )
        .fetch_all(self.db.pool())
        .await?;

        let mut grouped: BTreeMap<(String, String), Vec<ConsolidationNoteRef>> = BTreeMap::new();
        for (id, project_id, note_type, permalink, title, abstract_, overview) in rows {
            grouped
                .entry((project_id.clone(), note_type.clone()))
                .or_default()
                .push(ConsolidationNoteRef {
                    id,
                    project_id,
                    note_type,
                    permalink,
                    title,
                    abstract_,
                    overview,
                });
        }

        Ok(grouped
            .into_iter()
            .map(|((project_id, note_type), notes)| ConsolidationNoteGroup {
                project_id,
                note_type,
                notes,
            })
            .collect())
    }

    /// Build deterministic likely-duplicate connected components for one DB-only group.
    pub async fn likely_duplicate_clusters_for_group(
        &self,
        project_id: &str,
        note_type: &str,
    ) -> Result<Vec<ConsolidationCluster>> {
        self.likely_duplicate_clusters_for_group_with_min_size(
            project_id,
            note_type,
            DEFAULT_CONSOLIDATION_CLUSTER_MIN_SIZE,
        )
        .await
    }

    async fn likely_duplicate_clusters_for_group_with_min_size(
        &self,
        project_id: &str,
        note_type: &str,
        min_cluster_size: usize,
    ) -> Result<Vec<ConsolidationCluster>> {
        self.db.ensure_initialized().await?;

        let group = self
            .list_db_consolidation_groups()
            .await?
            .into_iter()
            .find(|group| group.project_id == project_id && group.note_type == note_type);

        let Some(group) = group else {
            return Ok(vec![]);
        };

        if group.notes.len() < min_cluster_size {
            return Ok(vec![]);
        }

        let note_by_id: BTreeMap<String, ConsolidationNoteRef> = group
            .notes
            .iter()
            .cloned()
            .map(|note| (note.id.clone(), note))
            .collect();
        let note_ids: BTreeSet<String> = note_by_id.keys().cloned().collect();

        let mut edge_map: BTreeMap<(String, String), f64> = BTreeMap::new();
        for note in group.notes.iter().cloned() {
            let query_text = note
                .abstract_
                .clone()
                .or_else(|| note.overview.clone())
                .unwrap_or_else(|| note.title.clone());
            if query_text.trim().is_empty() {
                continue;
            }

            let candidates = self
                .dedup_candidates(
                    project_id,
                    folder_for_type(note_type),
                    note_type,
                    &query_text,
                    group.notes.len(),
                )
                .await?;

            for candidate in candidates {
                if candidate.id == note.id || !note_ids.contains(&candidate.id) {
                    continue;
                }
                if candidate.score <= CONSOLIDATION_DEDUP_THRESHOLD {
                    continue;
                }

                let pair = if note.id < candidate.id {
                    (note.id.clone(), candidate.id)
                } else {
                    (candidate.id, note.id.clone())
                };

                edge_map
                    .entry(pair)
                    .and_modify(|score| {
                        if candidate.score > *score {
                            *score = candidate.score;
                        }
                    })
                    .or_insert(candidate.score);
            }
        }

        if edge_map.is_empty() {
            return Ok(vec![]);
        }

        let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
        for (left, right) in edge_map.keys() {
            adjacency
                .entry(left.clone())
                .or_default()
                .push(right.clone());
            adjacency
                .entry(right.clone())
                .or_default()
                .push(left.clone());
        }
        for neighbors in adjacency.values_mut() {
            neighbors.sort();
            neighbors.dedup();
        }

        let mut visited = BTreeSet::new();
        let mut clusters = Vec::new();
        for start_id in adjacency.keys().cloned().collect::<BTreeSet<_>>() {
            if visited.contains(&start_id) {
                continue;
            }

            let mut stack = vec![start_id.clone()];
            let mut component_ids = BTreeSet::new();
            while let Some(current) = stack.pop() {
                if !visited.insert(current.clone()) {
                    continue;
                }
                component_ids.insert(current.clone());
                if let Some(neighbors) = adjacency.get(&current) {
                    for neighbor in neighbors.iter().rev() {
                        if !visited.contains(neighbor) {
                            stack.push(neighbor.clone());
                        }
                    }
                }
            }

            if component_ids.len() < min_cluster_size {
                continue;
            }

            let component_notes: Vec<ConsolidationNoteRef> = component_ids
                .iter()
                .filter_map(|id| note_by_id.get(id).cloned())
                .collect();
            let component_edges: Vec<ConsolidationCandidateEdge> = edge_map
                .iter()
                .filter(|((left, right), _)| {
                    component_ids.contains(left) && component_ids.contains(right)
                })
                .map(|((left, right), score)| ConsolidationCandidateEdge {
                    left_note_id: left.clone(),
                    right_note_id: right.clone(),
                    score: *score,
                })
                .collect();

            if component_edges.is_empty() {
                continue;
            }

            clusters.push(ConsolidationCluster {
                project_id: project_id.to_string(),
                note_type: note_type.to_string(),
                notes: component_notes,
                edges: component_edges,
            });
        }

        clusters.sort_by(|left, right| {
            left.notes
                .first()
                .map(|note| &note.id)
                .cmp(&right.notes.first().map(|note| &note.id))
                .then_with(|| left.notes.len().cmp(&right.notes.len()))
        });

        Ok(clusters)
    }
}
