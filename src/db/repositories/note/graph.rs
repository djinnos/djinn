use super::*;

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
    /// Singleton types (`brief`, `roadmap`) are excluded.
    /// Optionally filtered by `folder`.
    pub async fn orphans(&self, project_id: &str, folder: Option<&str>) -> Result<Vec<OrphanNote>> {
        self.db.ensure_initialized().await?;

        let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
            "SELECT n.id, n.permalink, n.title, n.note_type, n.folder
             FROM notes n
             WHERE n.project_id = ?1
               AND n.note_type NOT IN ('brief', 'roadmap')
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
            .collect();

        Ok(HealthReport {
            total_notes,
            broken_link_count,
            orphan_note_count,
            stale_notes_by_folder,
        })
    }
}
