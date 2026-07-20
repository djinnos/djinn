use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use djinn_memory::{
    ExtractedNoteAuditAttribution, ExtractedNoteAuditCategory, ExtractedNoteAuditFinding,
    LifecycleHealth, RecentSweepMetrics,
};
use sqlx::Row;

use super::embedding_associations::EMBEDDING_ASSOCIATION_THRESHOLD;
use super::*;
use crate::repositories::note::{
    NoteConsolidationRepository, STALE_CITATION, assess_note_quality, looks_task_local,
    required_sections,
};

impl NoteRepository {
    // ── Wikilink graph ────────────────────────────────────────────────────────

    /// Full knowledge graph for a project: all notes as nodes and all resolved
    /// wikilink edges. `connection_count` = inbound + outbound resolved edges.
    pub async fn graph(&self, project_id: &str) -> Result<GraphResponse> {
        self.db.ensure_initialized().await?;

        let node_rows = sqlx::query!(
            r#"SELECT n.id AS "id!", n.permalink AS "permalink!",
                    n.title AS "title!", n.note_type AS "note_type!",
                    n.folder AS "folder!", n.status AS "status!",
                    n.lifecycle_changed_at AS "lifecycle_changed_at?", n.created_at AS "created_at!",
                    (SELECT COUNT(*) FROM note_links WHERE source_id = n.id
                       AND target_id IS NOT NULL)
                    + (SELECT COUNT(*) FROM note_links WHERE target_id = n.id)
                      AS "connection_count!: i64"
             FROM notes n
             WHERE n.project_id = $1 AND n.status = 'active'
             ORDER BY n.folder, n.title"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;

        let edge_rows = sqlx::query!(
            r#"SELECT l.source_id, l.target_id AS "target_id!", l.target_raw
             FROM note_links l
             JOIN notes src ON src.id = l.source_id AND src.project_id = $1
             WHERE l.target_id IS NOT NULL"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;

        let orphans = self
            .orphans(project_id, None)
            .await?
            .into_iter()
            .map(|orphan| orphan.id)
            .collect::<HashSet<_>>();
        let broken = self.broken_links(project_id, None).await?.into_iter().fold(
            HashMap::<String, Vec<String>>::new(),
            |mut acc, link| {
                acc.entry(link.source_id).or_default().push(link.raw_text);
                acc
            },
        );

        let mut nodes: Vec<GraphNode> = node_rows
            .into_iter()
            .map(|row| GraphNode {
                is_orphan: orphans.contains(&row.id),
                broken_targets: broken.get(&row.id).cloned().unwrap_or_default(),
                id: row.id,
                permalink: row.permalink,
                title: row.title,
                note_type: row.note_type,
                folder: row.folder,
                status: row.status,
                lifecycle_changed_at: row.lifecycle_changed_at,
                connection_count: row.connection_count,
                entity_type: "note".to_string(),
                created_at: Some(row.created_at),
            })
            .collect();

        // ── Proposal nodes ──────────────────────────────────────────────────────
        // Fetch proposals linked to this project via proposal_targets.
        let proposal_rows = sqlx::query(
            r#"SELECT p.id, p.short_id, p.title, p.created_at
               FROM proposals p
               JOIN proposal_targets pt ON pt.proposal_id = p.id
               WHERE pt.project_id = $1"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        for row in proposal_rows {
            let id: String = row.get("id");
            let short_id: String = row.get("short_id");
            let title: String = row.get("title");
            let created_at: String = row.get("created_at");
            // Count heterogeneous typed edges incident to this proposal.
            let connection_count: i64 = sqlx::query_scalar(
                r#"SELECT COUNT(*) FROM memory_entity_associations
                   WHERE (source_entity_type = 'proposal' AND source_id = $1)
                      OR (target_entity_type = 'proposal' AND target_id = $1)"#,
            )
            .bind(&id)
            .fetch_one(self.db.pool())
            .await?;

            nodes.push(GraphNode {
                id,
                permalink: short_id,
                title,
                note_type: "proposal".to_string(),
                folder: "".to_string(),
                status: "active".to_string(),
                lifecycle_changed_at: None,
                connection_count,
                is_orphan: false,
                broken_targets: Vec::new(),
                entity_type: "proposal".to_string(),
                created_at: Some(created_at),
            });
        }

        // Lifecycle selection has completed before edge loading. Keep the
        // rendered graph endpoint-closed even when an inactive note was not
        // selected by a bounded request.
        let returned_node_ids: HashSet<String> = nodes.iter().map(|node| node.id.clone()).collect();
        let edges = edge_rows
            .into_iter()
            .filter(|row| returned_node_ids.contains(&row.source_id) && returned_node_ids.contains(&row.target_id))
            .map(|row| GraphEdge {
                source_id: row.source_id,
                target_id: row.target_id,
                raw_text: row.target_raw,
            })
            .collect();

        // ── Typed semantic edges ──────────────────────────────────────────────
        // Fetch typed association edges (kind <> 'co_access') for notes in this
        // project. Uses a runtime (non-macro) query because the widened kind
        // CHECK constraint was added after the offline .sqlx cache was generated.
        //
        // The F5 substrate is undirected (canonical-pair ordering: note_a_id <
        // note_b_id). Outbound direction for directional kinds (e.g.
        // supersedes) is reconstructed at scoring/context time — here we
        // project both endpoints as (source_id = note_a_id, target_id = note_b_id)
        // so the canonical pair is preserved verbatim.
        let typed_edge_rows = sqlx::query(
            r#"SELECT na.note_a_id AS "source_id", na.note_b_id AS "target_id",
                      na.kind, na.weight
               FROM note_associations na
               JOIN notes n_a ON n_a.id = na.note_a_id AND n_a.project_id = $1
               JOIN notes n_b ON n_b.id = na.note_b_id AND n_b.project_id = $1
               WHERE na.kind <> 'co_access'"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut typed_edges: Vec<TypedEdge> = typed_edge_rows
            .into_iter()
            .filter(|row| {
                returned_node_ids.contains(row.get::<String, _>("source_id").as_str())
                    && returned_node_ids.contains(row.get::<String, _>("target_id").as_str())
            })
            .map(|row| TypedEdge {
                source_id: row.get("source_id"),
                target_id: row.get("target_id"),
                kind: row.get("kind"),
                weight: row.get("weight"),
                source_entity_type: "note".to_string(),
                target_entity_type: "note".to_string(),
            })
            .collect();

        // ── Heterogeneous typed edges ─────────────────────────────────────────
        // Fetch edges from memory_entity_associations where both endpoints are
        // in this project (note↔proposal, proposal↔note, proposal↔proposal).
        let mea_rows = sqlx::query(
            r#"SELECT
                   mea.source_entity_type,
                   mea.source_id,
                   mea.target_entity_type,
                   mea.target_id,
                   mea.kind,
                   mea.weight
               FROM memory_entity_associations mea
               WHERE (
                     (mea.source_entity_type = 'note'
                      AND EXISTS (SELECT 1 FROM notes n WHERE n.id = mea.source_id AND n.project_id = $1))
                  OR (mea.source_entity_type = 'proposal'
                      AND EXISTS (SELECT 1 FROM proposal_targets pt WHERE pt.proposal_id = mea.source_id AND pt.project_id = $1))
                 )
                 AND (
                     (mea.target_entity_type = 'note'
                      AND EXISTS (SELECT 1 FROM notes n WHERE n.id = mea.target_id AND n.project_id = $1))
                  OR (mea.target_entity_type = 'proposal'
                      AND EXISTS (SELECT 1 FROM proposal_targets pt WHERE pt.proposal_id = mea.target_id AND pt.project_id = $1))
                 )"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        for row in mea_rows {
            typed_edges.push(TypedEdge {
                source_id: row.get("source_id"),
                target_id: row.get("target_id"),
                kind: row.get("kind"),
                weight: row.get("weight"),
                source_entity_type: row.get("source_entity_type"),
                target_entity_type: row.get("target_entity_type"),
            });
        }

        Ok(GraphResponse {
            nodes,
            edges,
            typed_edges,
            lifecycle_summary: None,
        })
    }

    /// Additive lifecycle-aware graph entry point. The default wrapper above
    /// deliberately remains the active-only compatibility API.
    pub async fn graph_with_options(
        &self,
        project_id: &str,
        options: GraphOptions,
    ) -> Result<GraphResponse> {
        let Some(statuses) = options.statuses else {
            return self.graph(project_id).await;
        };
        let include_active = statuses.iter().any(|status| status == "active");
        let requested_inactive: HashSet<&str> = statuses
            .iter()
            .filter_map(|status| match status.as_str() {
                "archived" | "deprecated" => Some(status.as_str()),
                _ => None,
            })
            .collect();
        let limit = options.lifecycle_limit.unwrap_or(500).clamp(0, 1000);
        let mut graph = self.graph(project_id).await?;
        if !include_active {
            graph.nodes.retain(|node| node.entity_type == "proposal");
            graph.edges.clear();
            graph.typed_edges.retain(|edge| {
                edge.source_entity_type == "proposal" && edge.target_entity_type == "proposal"
            });
        }

        let mut inactive_rows = sqlx::query(
            r#"SELECT id, permalink, title, note_type, folder, status, created_at,
                      lifecycle_changed_at
               FROM notes
               WHERE project_id = $1 AND status IN ('archived', 'deprecated')
               ORDER BY lifecycle_changed_at DESC NULLS LAST, id"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?
        .into_iter()
        .filter(|row| requested_inactive.contains(row.get::<String, _>("status").as_str()))
        .collect::<Vec<_>>();
        let inactive_total = inactive_rows.len() as i64;
        inactive_rows.truncate(limit as usize);
        for row in inactive_rows {
            graph.nodes.push(GraphNode {
                id: row.get("id"),
                permalink: row.get("permalink"),
                title: row.get("title"),
                note_type: row.get("note_type"),
                folder: row.get("folder"),
                status: row.get("status"),
                lifecycle_changed_at: row.get("lifecycle_changed_at"),
                connection_count: 0,
                is_orphan: false,
                broken_targets: Vec::new(),
                entity_type: "note".to_string(),
                created_at: Some(row.get("created_at")),
            });
        }
        let inactive_returned = (graph.nodes.iter().filter(|node| {
            node.entity_type == "note" && node.status != "active"
        }).count()) as i64;
        graph.lifecycle_summary = Some(LifecycleGraphSummary {
            inactive_total,
            inactive_returned,
            inactive_omitted: inactive_total - inactive_returned,
        });
        Ok(graph)
    }

    /// All wikilinks in a project whose target note does not exist.
    /// Optionally filtered to links whose source note is in `folder`.
    pub async fn broken_links(
        &self,
        project_id: &str,
        folder: Option<&str>,
    ) -> Result<Vec<BrokenLink>> {
        self.db.ensure_initialized().await?;

        let rows = sqlx::query!(
            r#"SELECT src.id, src.permalink, src.title, l.target_raw
             FROM note_links l
             JOIN notes src ON src.id = l.source_id AND src.project_id = $1
             WHERE l.target_id IS NULL
               AND ($2::text IS NULL OR src.folder = $2)
             ORDER BY src.permalink, l.target_raw"#,
            project_id,
            folder,
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| BrokenLink {
                source_id: row.id,
                source_permalink: row.permalink,
                source_title: row.title,
                raw_text: row.target_raw,
            })
            .collect())
    }

    /// Notes with zero inbound authored edges (wikilinks or explicit
    /// `authored`/`manual` association edges).  Singleton types and generated
    /// catalog notes are excluded.  Machine-minted edges (`embedding_related`,
    /// `co_access`, etc.) do **not** hide authored-link debt.
    ///
    /// Optionally filtered by `folder`.
    pub async fn orphans(&self, project_id: &str, folder: Option<&str>) -> Result<Vec<OrphanNote>> {
        self.db.ensure_initialized().await?;

        // Runtime (non-macro) query: the added `note_associations` anti-join
        // for authored edges widens the statement beyond the offline `.sqlx`
        // cache.
        let rows = sqlx::query(
            r#"SELECT n.id, n.permalink, n.title, n.note_type, n.folder
             FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
               AND ($2::text IS NULL OR n.folder = $2)
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l WHERE l.target_id = n.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM note_associations na
                   WHERE (na.note_a_id = n.id OR na.note_b_id = n.id)
                     AND na.kind = 'authored'
               )
             ORDER BY n.folder, n.title"#,
        )
        .bind(project_id)
        .bind(folder)
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| OrphanNote {
                id: row.get("id"),
                permalink: row.get("permalink"),
                title: row.get("title"),
                note_type: row.get("note_type"),
                folder: row.get("folder"),
            })
            .collect())
    }

    /// Aggregate health report for a project's knowledge base.
    ///
    /// Stale threshold: notes not updated in more than 30 days.
    pub async fn health(&self, project_id: &str) -> Result<HealthReport> {
        self.db.ensure_initialized().await?;

        let total_notes: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM notes WHERE project_id = $1"#,
            project_id
        )
        .fetch_one(self.db.pool())
        .await?;

        let broken_link_count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM note_links l
             JOIN notes src ON src.id = l.source_id AND src.project_id = $1
             WHERE l.target_id IS NULL"#,
            project_id
        )
        .fetch_one(self.db.pool())
        .await?;

        // ── Authored-orphan count ──────────────────────────────────────────
        // A note is an authored orphan when it has no inbound wikilinks AND
        // no inbound explicit `authored` association edge.  Machine-minted
        // edges (embedding_related, co_access, etc.) do not hide debt.
        // Only active notes are counted.
        let authored_orphan_count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l WHERE l.target_id = n.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM note_associations na
                   WHERE (na.note_a_id = n.id OR na.note_b_id = n.id)
                     AND na.kind = 'authored'
               )"#,
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;

        // `orphan_note_count` is the backward-compatible alias.
        let orphan_note_count = authored_orphan_count;

        // ── Graph-isolation count ─────────────────────────────────────────
        // A note is graph-isolated when it has *no* retrieval-effective edges
        // in any direction: no resolved wikilinks, no authored/manual
        // associations, no co-access, and no threshold-qualified
        // `embedding_related` edges.  Only active notes are counted.
        let threshold = EMBEDDING_ASSOCIATION_THRESHOLD;
        let isolated_count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
               -- No inbound resolved wikilink
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l WHERE l.target_id = n.id
               )
               -- No outbound resolved wikilink
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l
                   WHERE l.source_id = n.id AND l.target_id IS NOT NULL
               )
               -- No non-embedding association of any kind (includes authored,
               -- co_access, builds_on, contradicts, supersedes, etc.)
               AND NOT EXISTS (
                   SELECT 1 FROM note_associations na
                   WHERE (na.note_a_id = n.id OR na.note_b_id = n.id)
                     AND na.kind <> 'embedding_related'
               )
               -- No threshold-qualified embedding_related edge
               AND NOT EXISTS (
                   SELECT 1 FROM note_associations na
                   WHERE (na.note_a_id = n.id OR na.note_b_id = n.id)
                     AND na.kind = 'embedding_related'
                     AND (na.confidence IS NULL OR na.confidence >= $2)
               )"#,
        )
        .bind(project_id)
        .bind(threshold)
        .fetch_one(self.db.pool())
        .await?;

        // Non-singleton note count for percentage computation.
        // Only active notes are counted.
        let non_singleton_count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')"#,
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;

        let isolated_pct = if non_singleton_count > 0 {
            (isolated_count as f64) / (non_singleton_count as f64) * 100.0
        } else {
            0.0
        };

        // Machine-connected orphans: authored orphans that have at least one
        // *non-authored* retrieval edge connecting them (e.g. co_access,
        // threshold-qualified embedding_related).  Outbound authored wikilinks
        // do NOT count — they are authored edges, not machine-minted.
        let machine_connected_orphan_count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
               -- Authored-orphan predicate
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l WHERE l.target_id = n.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM note_associations na
                   WHERE (na.note_a_id = n.id OR na.note_b_id = n.id)
                     AND na.kind = 'authored'
               )
               -- Has at least one non-authored retrieval edge
               AND (
                   EXISTS (
                       SELECT 1 FROM note_associations na
                       WHERE (na.note_a_id = n.id OR na.note_b_id = n.id)
                         AND na.kind <> 'embedding_related'
                         AND na.kind <> 'authored'
                   )
                   OR EXISTS (
                       SELECT 1 FROM note_associations na
                       WHERE (na.note_a_id = n.id OR na.note_b_id = n.id)
                         AND na.kind = 'embedding_related'
                         AND (na.confidence IS NULL OR na.confidence >= $2)
                   )
               )"#,
        )
        .bind(project_id)
        .bind(threshold)
        .fetch_one(self.db.pool())
        .await?;

        let stale_rows = sqlx::query!(
            r#"SELECT folder, COUNT(*) AS "count!: i64" FROM notes
             WHERE project_id = $1
               AND updated_at < to_char((now() at time zone 'utc') - interval '30 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
             GROUP BY folder ORDER BY folder"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;

        let stale_notes_by_folder = stale_rows
            .into_iter()
            .map(|row| StaleFolder {
                folder: row.folder,
                count: row.count,
            })
            .collect::<Vec<_>>();
        let stale_note_count = stale_notes_by_folder
            .iter()
            .map(|folder| folder.count)
            .sum();

        let low_confidence_note_count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM notes
             WHERE project_id = $1
               AND confidence < $2"#,
            project_id,
            STALE_CITATION
        )
        .fetch_one(self.db.pool())
        .await?;

        let active_notes: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM notes WHERE project_id = $1 AND status = 'active'"#,
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;
        let archived_notes: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM notes WHERE project_id = $1 AND status = 'archived'"#,
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;

        let deprecated_notes: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM notes WHERE project_id = $1 AND status = 'deprecated'"#,
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;

        let recent_sweep_row = sqlx::query(
            r#"SELECT completed_at,
                      CAST(decayed_note_count AS BIGINT) AS decayed_note_count,
                      CAST(archived_note_count AS BIGINT) AS archived_note_count,
                      CAST(superseded_source_note_count AS BIGINT) AS superseded_source_note_count
               FROM consolidation_run_metrics
               WHERE project_id = $1
                 AND note_type = 'lifecycle_sweep'
               ORDER BY COALESCE(completed_at, started_at) DESC, id DESC
               LIMIT 1"#,
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?;

        let recent_sweep = if let Some(row) = recent_sweep_row {
            RecentSweepMetrics {
                last_sweep_at: row.try_get("completed_at")?,
                last_decayed_count: row.try_get("decayed_note_count")?,
                last_archived_count: row.try_get("archived_note_count")?,
                last_superseded_source_count: row.try_get("superseded_source_note_count")?,
            }
        } else {
            RecentSweepMetrics {
                last_sweep_at: None,
                last_decayed_count: 0,
                last_archived_count: 0,
                last_superseded_source_count: 0,
            }
        };

        let admission_dropped_note_count: i64 = sqlx::query_scalar(
            r#"SELECT COALESCE(SUM(admission_dropped_note_count), 0)::BIGINT
               FROM consolidation_run_metrics
               WHERE project_id = $1"#,
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;

        Ok(HealthReport {
            total_notes,
            broken_link_count,
            orphan_note_count,
            authored_orphan_count,
            isolated_count,
            isolated_pct,
            machine_connected_orphan_count,
            low_confidence_note_count,
            stale_note_count,
            stale_notes_by_folder,
            admission_dropped_note_count,
            lifecycle: LifecycleHealth {
                active_notes,
                archived_notes,
                deprecated_notes,
            },
            recent_sweep,
        })
    }

    /// ADR-054 corpus audit for existing extracted case/pattern/pitfall notes.
    pub async fn extracted_note_audit(&self, project_id: &str) -> Result<ExtractedNoteAuditReport> {
        self.db.ensure_initialized().await?;

        let notes = sqlx::query_as::<_, Note>(
            r#"SELECT id, project_id, permalink, title, file_path,
                    storage, note_type, folder, status, tags::text AS tags, content,
                    retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                    access_count, confidence, abstract AS abstract_, overview,
                    scope_paths::text AS scope_paths
             FROM notes
             WHERE project_id = $1
               AND status = 'active'
               AND note_type IN ('case', 'pattern', 'pitfall')
             ORDER BY note_type, permalink"#,
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
            // Select the latest immutable event under the same project/note
            // scope as the audit. An empty history is expected for
            // pre-migration notes and deliberately remains unattributed.
            let attribution = self
                .note_revision_history(NoteHistoryRequest {
                    project_id,
                    note_id: &note.id,
                    limit: 1,
                    before: None,
                })
                .await?
                .events
                .into_iter()
                .next()
                .map(|event| ExtractedNoteAuditAttribution {
                    revision_id: event.id,
                    revision_kind: event.event_kind.as_str().to_owned(),
                    revision_seq: event.note_seq,
                    revision_created_at: event.created_at,
                    actor_kind: event.attribution.actor_kind().as_str().to_owned(),
                    actor_id: event.attribution.actor_id().map(ToOwned::to_owned),
                    subsystem: event.attribution.subsystem().map(ToOwned::to_owned),
                    session_id: event.provenance.session_id().map(ToOwned::to_owned),
                    task_id: event.provenance.task_id().map(ToOwned::to_owned),
                    task_run_id: event.provenance.task_run_id().map(ToOwned::to_owned),
                    reason: event.reason.into_inner(),
                });
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
                    attribution: attribution.clone(),
                });
            }

            let quality = assess_note_quality(&note.note_type, &note.content);
            let has_footer_only_shape = note.content.contains("*Extracted from session ")
                && quality.paragraph_count <= 2
                && quality.missing_sections.len() == required_sections(&note.note_type).len();
            let looks_task_local = looks_task_local(&note.title, &note.content);
            let is_orphan = !sqlx::query_scalar!(
                r#"SELECT EXISTS(SELECT 1 FROM note_links WHERE target_id = $1) AS "exists!: bool""#,
                note.id
            )
            .fetch_one(self.db.pool())
            .await?;

            if quality.is_underspecified && seen.insert((note.id.clone(), "underspecified")) {
                underspecified.push(ExtractedNoteAuditFinding {
                    note_id: note.id.clone(),
                    permalink: note.permalink.clone(),
                    title: note.title.clone(),
                    note_type: note.note_type.clone(),
                    folder: note.folder.clone(),
                    category: ExtractedNoteAuditCategory::Underspecified,
                    reasons: quality.reasons.clone(),
                    related_note_ids: cluster_by_note_id
                        .get(&note.id)
                        .into_iter()
                        .flatten()
                        .filter(|id| *id != &note.id)
                        .cloned()
                        .collect(),
                    attribution: attribution.clone(),
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
                    attribution: attribution.clone(),
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
                    attribution: attribution.clone(),
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

    /// ADR-054 archive candidates that are safe for an automated archive pass.
    ///
    /// This intersects the ADR-054 audit classifier with the lifecycle/access
    /// safety boundary required by the archive sweep: only `active` extracted
    /// note types (`case`, `pattern`, `pitfall`) are eligible, and only when the
    /// note has never been accessed (`access_count = 0`) or its last access is
    /// outside the configured archive window. Hand-written note types are
    /// excluded by the SQL predicate before any status mutation can consume this
    /// result.
    pub async fn extracted_archive_candidates(
        &self,
        project_id: &str,
        archive_window_days: u32,
    ) -> Result<Vec<ExtractedNoteAuditFinding>> {
        self.db.ensure_initialized().await?;

        let window = if archive_window_days > 0 {
            archive_window_days
        } else {
            crate::repositories::note::lifecycle::DEFAULT_ARCHIVE_WINDOW_DAYS
        };

        let eligible_rows = sqlx::query(
            r#"SELECT id
               FROM notes
               WHERE project_id = $1
                 AND status = 'active'
                 AND note_type IN ('case', 'pattern', 'pitfall')
                 AND (
                     access_count = 0
                     OR last_accessed IS NULL
                     OR last_accessed = ''
                     OR last_accessed < to_char(
                         (now() at time zone 'utc') - ($2 || ' days')::interval,
                         'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'
                     )
                 )"#,
        )
        .bind(project_id)
        .bind(window.to_string())
        .fetch_all(self.db.pool())
        .await?;

        let mut eligible_ids = HashSet::with_capacity(eligible_rows.len());
        for row in eligible_rows {
            let id: String = row.try_get("id")?;
            eligible_ids.insert(id);
        }

        let audit = self.extracted_note_audit(project_id).await?;
        Ok(audit
            .archive_candidates
            .into_iter()
            .filter(|finding| eligible_ids.contains(&finding.note_id))
            .collect())
    }

    async fn note_id_by_permalink(
        &self,
        project_id: &str,
        permalink: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar!(
            "SELECT id FROM notes WHERE project_id = $1 AND permalink = $2 LIMIT 1",
            project_id,
            permalink
        )
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
            let note_type: Option<String> = sqlx::query_scalar(
                "SELECT note_type FROM notes WHERE id = $1 AND project_id = $2 AND status = 'active' LIMIT 1",
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

        let task_refs: Option<String> = sqlx::query_scalar!(
            r#"SELECT memory_refs::text AS "memory_refs!" FROM tasks WHERE id = $1 AND project_id = $2"#,
            task_id,
            project_id
        )
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

        let epic_refs: Option<String> = sqlx::query_scalar!(
            r#"SELECT e.memory_refs::text AS "memory_refs!"
             FROM tasks t
             JOIN epics e ON e.id = t.epic_id
             WHERE t.id = $1 AND t.project_id = $2"#,
            task_id,
            project_id
        )
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

        let blocker_refs = sqlx::query!(
            r#"SELECT bt.memory_refs::text AS "memory_refs!"
             FROM blockers b
             JOIN tasks bt ON bt.id = b.blocking_task_id
             WHERE b.task_id = $1 AND bt.project_id = $2"#,
            task_id,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;

        for row in blocker_refs {
            let refs_json = row.memory_refs;
            if let Ok(note_ids) = serde_json::from_str::<Vec<String>>(&refs_json) {
                for note_id in note_ids {
                    scores
                        .entry(note_id)
                        .and_modify(|score| *score = score.max(0.5_f64))
                        .or_insert(0.5);
                }
            }
        }

        let candidate_ids: Vec<String> = scores.keys().cloned().collect();
        if candidate_ids.is_empty() {
            return Ok(vec![]);
        }

        let placeholders = crate::repositories::pg_placeholders(candidate_ids.len(), 2);
        let sql = format!(
            "SELECT id FROM notes WHERE project_id = $1 AND status = 'active' AND id IN ({placeholders})"
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql).bind(project_id);
        for note_id in &candidate_ids {
            query = query.bind(note_id);
        }
        let active_ids: std::collections::HashSet<String> =
            query.fetch_all(self.db.pool()).await?.into_iter().collect();

        let mut ranked: Vec<(String, f64)> = scores
            .into_iter()
            .filter(|(note_id, _)| active_ids.contains(note_id))
            .collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        Ok(ranked)
    }
}
