use super::*;
use crate::repositories::note::{NoteConsolidationRepository, STALE_CITATION};

impl NoteRepository {
    // ── Wikilink graph ────────────────────────────────────────────────────────

    /// Full knowledge graph for a project: all notes as nodes and all resolved
    /// wikilink edges. `connection_count` = inbound + outbound resolved edges.
    pub async fn graph(&self, project_id: &str) -> Result<GraphResponse> {
        self.db.ensure_initialized().await?;

        let node_rows = sqlx::query_as::<_, (String, String, String, String, String, i64)>(
            "SELECT n.id, n.permalink, n.title, n.note_type, n.folder,
                    (SELECT COUNT(*) FROM note_links WHERE source_id = n.id
                       AND target_id IS NOT NULL)
                    + (SELECT COUNT(*) FROM note_links WHERE target_id = n.id)
                      AS connection_count
             FROM notes n
             WHERE n.project_id = ?1
             ORDER BY n.folder, n.title",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let edge_rows = sqlx::query_as::<_, (String, String, String)>(
            "SELECT l.source_id, l.target_id, l.target_raw
             FROM note_links l
             JOIN notes src ON src.id = l.source_id AND src.project_id = ?1
             WHERE l.target_id IS NOT NULL",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let nodes = node_rows
            .into_iter()
            .map(
                |(id, permalink, title, note_type, folder, connection_count)| GraphNode {
                    id,
                    permalink,
                    title,
                    note_type,
                    folder,
                    connection_count,
                },
            )
            .collect();

        let edges = edge_rows
            .into_iter()
            .map(|(source_id, target_id, raw_text)| GraphEdge {
                source_id,
                target_id,
                raw_text,
            })
            .collect();

        Ok(GraphResponse { nodes, edges })
    }

    /// All wikilinks in a project whose target note does not exist.
    /// Optionally filtered to links whose source note is in `folder`.
    pub async fn broken_links(
        &self,
        project_id: &str,
        folder: Option<&str>,
    ) -> Result<Vec<BrokenLink>> {
        self.db.ensure_initialized().await?;

        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT src.id, src.permalink, src.title, l.target_raw
             FROM note_links l
             JOIN notes src ON src.id = l.source_id AND src.project_id = ?1
             WHERE l.target_id IS NULL
               AND (?2 IS NULL OR src.folder = ?2)
             ORDER BY src.permalink, l.target_raw",
        )
        .bind(project_id)
        .bind(folder)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(source_id, source_permalink, source_title, raw_text)| BrokenLink {
                    source_id,
                    source_permalink,
                    source_title,
                    raw_text,
                },
            )
            .collect())
    }

    /// Notes with zero inbound wikilinks (potential dead-ends).
    /// Singleton types and generated catalog notes are excluded.
    /// Optionally filtered by `folder`.
    pub async fn orphans(&self, project_id: &str, folder: Option<&str>) -> Result<Vec<OrphanNote>> {
        self.db.ensure_initialized().await?;

        let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
            "SELECT n.id, n.permalink, n.title, n.note_type, n.folder
             FROM notes n
             WHERE n.project_id = ?1
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
               AND (?2 IS NULL OR n.folder = ?2)
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l WHERE l.target_id = n.id
               )
             ORDER BY n.folder, n.title",
        )
        .bind(project_id)
        .bind(folder)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|(id, permalink, title, note_type, folder)| OrphanNote {
                id,
                permalink,
                title,
                note_type,
                folder,
            })
            .collect())
    }

    /// Aggregate health report for a project's knowledge base.
    ///
    /// Stale threshold: notes not updated in more than 30 days.
    pub async fn health(&self, project_id: &str) -> Result<HealthReport> {
        self.db.ensure_initialized().await?;

        let total_notes: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE project_id = ?1")
                .bind(project_id)
                .fetch_one(self.db.pool())
                .await?;

        let broken_link_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM note_links l
             JOIN notes src ON src.id = l.source_id AND src.project_id = ?1
             WHERE l.target_id IS NULL",
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;

        let orphan_note_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notes n
             WHERE n.project_id = ?1
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l WHERE l.target_id = n.id
               )",
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;

        let stale_rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT folder, COUNT(*) FROM notes
             WHERE project_id = ?1
               AND updated_at < datetime('now', '-30 days')
             GROUP BY folder ORDER BY folder",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let stale_notes_by_folder = stale_rows
            .into_iter()
            .map(|(folder, count)| StaleFolder { folder, count })
            .collect::<Vec<_>>();
        let stale_note_count = stale_notes_by_folder
            .iter()
            .map(|folder| folder.count)
            .sum();

        let low_confidence_note_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM notes
             WHERE project_id = ?1
               AND confidence < ?2",
        )
        .bind(project_id)
        .bind(STALE_CITATION)
        .fetch_one(self.db.pool())
        .await?;

        let consolidation_repo = NoteConsolidationRepository::new(self.db.clone());
        let mut duplicate_cluster_count = 0_i64;
        for group in consolidation_repo.list_db_note_groups().await? {
            if group.project_id != project_id {
                continue;
            }

            let clusters = consolidation_repo
                .likely_duplicate_clusters(project_id, &group.note_type)
                .await?;
            duplicate_cluster_count += clusters.len() as i64;
        }

        Ok(HealthReport {
            total_notes,
            broken_link_count,
            orphan_note_count,
            duplicate_cluster_count,
            low_confidence_note_count,
            stale_note_count,
            stale_notes_by_folder,
        })
    }
    async fn note_id_by_permalink(
        &self,
        project_id: &str,
        permalink: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT id FROM notes WHERE project_id = ?1 AND permalink = ?2 LIMIT 1",
        )
        .bind(project_id)
        .bind(permalink)
        .fetch_optional(self.db.pool())
        .await?)
    }

    async fn repo_map_file_affinity_scores(
        &self,
        project_id: &str,
        memory_refs: &[String],
    ) -> Result<Vec<(String, f64)>> {
        if memory_refs.is_empty() {
            return Ok(Vec::new());
        }

        let mut seeds = Vec::new();
        for permalink in memory_refs {
            if let Some(id) = self.note_id_by_permalink(project_id, permalink).await? {
                seeds.push(id);
            }
        }

        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        let candidate_scores = self.graph_proximity_scores(&seeds, 1).await?;
        let mut filtered = Vec::new();

        for (id, score) in candidate_scores {
            let note_type = sqlx::query_scalar::<_, String>(
                "SELECT note_type FROM notes WHERE id = ?1 AND project_id = ?2 LIMIT 1",
            )
            .bind(&id)
            .bind(project_id)
            .fetch_optional(self.db.pool())
            .await?;

            if matches!(note_type.as_deref(), Some("repo_map")) {
                filtered.push((id, score * 0.35));
            }
        }

        filtered.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        Ok(filtered)
    }

    pub async fn task_affinity_scores(
        &self,
        project_id: &str,
        task_id: Option<&str>,
    ) -> Result<Vec<(String, f64)>> {
        self.db.ensure_initialized().await?;

        let Some(task_id) = task_id else {
            return Ok(vec![]);
        };

        use std::collections::HashMap;

        let mut scores: HashMap<String, f64> = HashMap::new();

        let task_refs: Option<String> =
            sqlx::query_scalar("SELECT memory_refs FROM tasks WHERE id = ?1 AND project_id = ?2")
                .bind(task_id)
                .bind(project_id)
                .fetch_optional(self.db.pool())
                .await?;

        if let Some(refs_json) = task_refs
            && let Ok(memory_refs) = serde_json::from_str::<Vec<String>>(&refs_json)
        {
            let mut direct_note_ids = Vec::new();
            for memory_ref in &memory_refs {
                let note_id = self
                    .note_id_by_permalink(project_id, memory_ref)
                    .await?
                    .unwrap_or_else(|| memory_ref.clone());
                direct_note_ids.push(note_id);
            }

            for note_id in &direct_note_ids {
                scores
                    .entry(note_id.clone())
                    .and_modify(|score| *score = score.max(1.0_f64))
                    .or_insert(1.0);
            }

            for (note_id, score) in self
                .repo_map_file_affinity_scores(project_id, &memory_refs)
                .await?
            {
                scores
                    .entry(note_id)
                    .and_modify(|existing| *existing = existing.max(score))
                    .or_insert(score);
            }
        }

        let epic_refs: Option<String> = sqlx::query_scalar(
            "SELECT e.memory_refs
             FROM tasks t
             JOIN epics e ON e.id = t.epic_id
             WHERE t.id = ?1 AND t.project_id = ?2",
        )
        .bind(task_id)
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;

        if let Some(refs_json) = epic_refs
            && let Ok(note_ids) = serde_json::from_str::<Vec<String>>(&refs_json)
        {
            for note_id in note_ids {
                scores
                    .entry(note_id)
                    .and_modify(|score| *score = score.max(0.7_f64))
                    .or_insert(0.7);
            }
        }

        let blocker_refs = sqlx::query_as::<_, (String,)>(
            "SELECT bt.memory_refs
             FROM blockers b
             JOIN tasks bt ON bt.id = b.blocking_task_id
             WHERE b.task_id = ?1 AND bt.project_id = ?2",
        )
        .bind(task_id)
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        for (refs_json,) in blocker_refs {
            if let Ok(note_ids) = serde_json::from_str::<Vec<String>>(&refs_json) {
                for note_id in note_ids {
                    scores
                        .entry(note_id)
                        .and_modify(|score| *score = score.max(0.5_f64))
                        .or_insert(0.5);
                }
            }
        }

        let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        Ok(ranked)
    }
}
