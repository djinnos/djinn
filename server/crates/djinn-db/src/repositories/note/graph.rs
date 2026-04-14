use std::collections::{BTreeMap, BTreeSet};

use djinn_core::models::{ExtractedNoteAuditCategory, ExtractedNoteAuditFinding};

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

    /// ADR-054 corpus audit for existing extracted case/pattern/pitfall notes.
    pub async fn extracted_note_audit(&self, project_id: &str) -> Result<ExtractedNoteAuditReport> {
        self.db.ensure_initialized().await?;

        let notes = sqlx::query_as::<_, Note>(
            "SELECT id, project_id, permalink, title, file_path,
                    storage, note_type, folder, tags, content,
                    created_at, updated_at, last_accessed,
                    access_count, confidence, abstract as abstract_, overview,
                    scope_paths
             FROM notes
             WHERE project_id = ?1
               AND note_type IN ('case', 'pattern', 'pitfall')
             ORDER BY note_type, permalink",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut merge_candidates = Vec::new();
        let mut underspecified = Vec::new();
        let mut demote_to_working_spec = Vec::new();
        let mut archive_candidates = Vec::new();
        let mut seen = BTreeSet::new();

        let consolidation_repo = NoteConsolidationRepository::new(self.db.clone());
        let mut cluster_by_note_id = BTreeMap::new();
        for note_type in ["case", "pattern", "pitfall"] {
            for cluster in consolidation_repo
                .likely_duplicate_clusters(project_id, note_type)
                .await?
            {
                let related = cluster
                    .notes
                    .iter()
                    .map(|note| note.id.clone())
                    .collect::<Vec<_>>();
                for note in &cluster.notes {
                    cluster_by_note_id.insert(note.id.clone(), related.clone());
                }
            }
        }

        for note in &notes {
            if let Some(related_ids) = cluster_by_note_id.get(&note.id)
                && seen.insert((note.id.clone(), "merge"))
            {
                merge_candidates.push(ExtractedNoteAuditFinding {
                    note_id: note.id.clone(),
                    permalink: note.permalink.clone(),
                    title: note.title.clone(),
                    note_type: note.note_type.clone(),
                    folder: note.folder.clone(),
                    category: ExtractedNoteAuditCategory::MergeCandidate,
                    reasons: vec![format!(
                        "Likely duplicate cluster with {} related notes; consolidate into a canonical {} note",
                        related_ids.len().saturating_sub(1),
                        note.note_type
                    )],
                    related_note_ids: related_ids
                        .iter()
                        .filter(|id| *id != &note.id)
                        .cloned()
                        .collect(),
                });
            }

            let required_sections = required_sections(&note.note_type);
            let missing_sections = missing_required_sections(&note.content, &required_sections);
            let paragraph_count = note
                .content
                .split("\n\n")
                .filter(|block| !block.trim().is_empty())
                .count();
            let content_len = note.content.trim().chars().count();
            let has_footer_only_shape = note.content.contains("*Extracted from session ")
                && paragraph_count <= 2
                && missing_sections.len() == required_sections.len();
            let looks_task_local = looks_task_local(&note.title, &note.content);
            let is_orphan = !sqlx::query_scalar::<_, i64>(
                "SELECT EXISTS(SELECT 1 FROM note_links WHERE target_id = ?1)",
            )
            .bind(&note.id)
            .fetch_one(self.db.pool())
            .await?
                > 0;

            if (!missing_sections.is_empty() || content_len < 220 || paragraph_count < 3)
                && seen.insert((note.id.clone(), "underspecified"))
            {
                let mut reasons = Vec::new();
                if !missing_sections.is_empty() {
                    reasons.push(format!(
                        "Missing ADR-054 required sections: {}",
                        missing_sections.join(", ")
                    ));
                }
                if content_len < 220 {
                    reasons.push(format!(
                        "Body is too short for durable memory ({content_len} chars)"
                    ));
                }
                if paragraph_count < 3 {
                    reasons.push(format!(
                        "Body has only {paragraph_count} paragraph(s); strengthen with explicit context, rationale, and transfer lesson"
                    ));
                }
                underspecified.push(ExtractedNoteAuditFinding {
                    note_id: note.id.clone(),
                    permalink: note.permalink.clone(),
                    title: note.title.clone(),
                    note_type: note.note_type.clone(),
                    folder: note.folder.clone(),
                    category: ExtractedNoteAuditCategory::Underspecified,
                    reasons,
                    related_note_ids: cluster_by_note_id
                        .get(&note.id)
                        .into_iter()
                        .flatten()
                        .filter(|id| *id != &note.id)
                        .cloned()
                        .collect(),
                });
            }

            if looks_task_local && seen.insert((note.id.clone(), "demote")) {
                demote_to_working_spec.push(ExtractedNoteAuditFinding {
                    note_id: note.id.clone(),
                    permalink: note.permalink.clone(),
                    title: note.title.clone(),
                    note_type: note.note_type.clone(),
                    folder: note.folder.clone(),
                    category: ExtractedNoteAuditCategory::DemoteToWorkingSpec,
                    reasons: vec![
                        "Content reads as task/session-local working context rather than durable reusable knowledge"
                            .to_string(),
                        "Promote only if rewritten to explain the reusable lesson beyond the originating task"
                            .to_string(),
                    ],
                    related_note_ids: cluster_by_note_id
                        .get(&note.id)
                        .into_iter()
                        .flatten()
                        .filter(|id| *id != &note.id)
                        .cloned()
                        .collect(),
                });
            }

            if (has_footer_only_shape || (is_orphan && note.confidence <= STALE_CITATION))
                && seen.insert((note.id.clone(), "archive"))
            {
                let mut reasons = Vec::new();
                if has_footer_only_shape {
                    reasons.push(
                        "Note is essentially a single extracted paragraph plus provenance footer; archive unless strengthened or merged"
                            .to_string(),
                    );
                }
                if is_orphan {
                    reasons.push("No inbound wikilinks; currently disconnected from the durable knowledge graph".to_string());
                }
                if note.confidence <= STALE_CITATION {
                    reasons.push(format!(
                        "Low confidence ({:.2}) suggests stale or weak durable value",
                        note.confidence
                    ));
                }
                archive_candidates.push(ExtractedNoteAuditFinding {
                    note_id: note.id.clone(),
                    permalink: note.permalink.clone(),
                    title: note.title.clone(),
                    note_type: note.note_type.clone(),
                    folder: note.folder.clone(),
                    category: ExtractedNoteAuditCategory::ArchiveCandidate,
                    reasons,
                    related_note_ids: cluster_by_note_id
                        .get(&note.id)
                        .into_iter()
                        .flatten()
                        .filter(|id| *id != &note.id)
                        .cloned()
                        .collect(),
                });
            }
        }

        Ok(ExtractedNoteAuditReport {
            scanned_note_count: notes.len() as i64,
            merge_candidates,
            underspecified,
            demote_to_working_spec,
            archive_candidates,
            rerun_hint: "Rerun `memory_extracted_audit()` after cleanup and compare category counts to measure the remaining ADR-054 migration backlog.".to_string(),
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

fn required_sections(note_type: &str) -> Vec<&'static str> {
    match note_type {
        "pattern" => vec![
            "Context",
            "Problem shape",
            "Recommended approach",
            "Why it works",
            "Tradeoffs / limits",
            "When to use",
            "When not to use",
            "Related",
        ],
        "pitfall" => vec![
            "Trigger / smell",
            "Failure mode",
            "Observable symptoms",
            "Prevention",
            "Recovery",
            "Related",
        ],
        "case" => vec![
            "Situation",
            "Constraint",
            "Approach taken",
            "Result",
            "Why it worked / failed",
            "Reusable lesson",
            "Related",
        ],
        _ => vec![],
    }
}

fn missing_required_sections(content: &str, required_sections: &[&str]) -> Vec<String> {
    required_sections
        .iter()
        .filter(|section| !content.contains(&format!("## {section}")))
        .map(|section| section.to_string())
        .collect()
}

fn looks_task_local(title: &str, content: &str) -> bool {
    let haystack = format!("{}\n{}", title.to_lowercase(), content.to_lowercase());
    [
        "this session",
        "current task",
        "working note",
        "working spec",
        "follow-up work",
        "could not be updated from this session",
        "drafted locally",
        "active follow-up work",
        "next session",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}
