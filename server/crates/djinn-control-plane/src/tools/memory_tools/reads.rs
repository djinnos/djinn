use super::ops;
use super::revision_readers;
use super::*;

#[tool_router(router = memory_reads_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Read a note by permalink or title. Updates last_accessed timestamp.
    #[tool(description = "Read a note by permalink or title. Updates last_accessed timestamp.")]
    pub async fn memory_read(
        &self,
        Parameters(p): Parameters<ReadParams>,
    ) -> Json<MemoryNoteResponse> {
        Json(ops::memory_read(self, p).await)
    }

    /// Return a note's immutable revision history from the ledger,
    /// newest-first, with stable cursor pagination. Retained deleted-note
    /// history requires audit authorization; live pre-ledger notes return an
    /// empty `migration_cutover` history. Never consults git.
    #[tool(
        description = "Return a note's immutable ledger revision history, newest-first by note sequence, with bounded cursor pagination (default 50, max 200). Retained deleted-note history is available to audit-authorized callers; a live pre-ledger note returns an empty history with history_start=migration_cutover. Ledger-backed: no git fallback."
    )]
    pub async fn memory_history(
        &self,
        Parameters(p): Parameters<HistoryParams>,
    ) -> Json<MemoryHistoryResponse> {
        Json(revision_readers::note_history(self, p).await)
    }

    /// Render a deterministic unified text diff between two explicit
    /// content-bearing ledger revisions of one note. Both endpoints must be
    /// created/updated/deleted events for the same project and note, and the
    /// from-revision must be older than the to-revision. Intervening
    /// non-content events are returned alongside the diff.
    #[tool(
        description = "Render a deterministic unified text diff between two explicit ledger revisions of one note, from the from-revision's content_before snapshot to the to-revision's content_after snapshot. Endpoints must be content-bearing (created/updated/deleted) events in the same project and note, ordered oldest to newest; confidence_changed or extraction_skipped endpoints are rejected. Intervening non-content events are included in the response. Ledger-backed: no git fallback."
    )]
    pub async fn memory_diff(
        &self,
        Parameters(p): Parameters<DiffParams>,
    ) -> Json<MemoryDiffResponse> {
        Json(revision_readers::note_diff(self, p).await)
    }

    /// Return every ledger revision event attributed to one session or one
    /// task run, newest-first by (created_at, id), with bounded cursor
    /// pagination. Exactly one of session_id or task_run_id must be provided.
    /// Content bodies are redacted when the caller lacks note-read
    /// permission.
    #[tool(
        description = "Return all immutable ledger revision events attributed to exactly one session_id or task_run_id, newest-first by (created_at, id), with bounded cursor pagination (default 100, max 500). All event kinds are included. Event metadata is always visible; content bodies are redacted when the caller lacks note-read permission."
    )]
    pub async fn memory_session_diff(
        &self,
        Parameters(p): Parameters<SessionDiffParams>,
    ) -> Json<MemorySessionDiffResponse> {
        Json(revision_readers::session_diff(self, p).await)
    }

    /// List notes in a folder with depth control. Returns compact summaries
    /// without full content.
    #[tool(
        description = "List notes in a folder with depth control. Returns compact summaries without full content."
    )]
    pub async fn memory_list(
        &self,
        Parameters(p): Parameters<ListParams>,
    ) -> Json<MemoryListResponse> {
        Json(ops::memory_list(self, p).await)
    }

    /// Read the auto-generated knowledge base catalog. Returns the full catalog
    /// markdown content — the master table of contents for all notes in the KB.
    #[tool(
        description = "Read the auto-generated knowledge base catalog. Returns the full catalog markdown content — the master table of contents for all notes in the KB. Use this as the first tool call when orienting yourself in a new session: it tells you exactly what knowledge exists and where to find it. Read-only — does not modify any notes or the SQLite index."
    )]
    pub async fn memory_catalog(
        &self,
        Parameters(p): Parameters<CatalogParams>,
    ) -> Json<MemoryCatalogResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryCatalogResponse {
                catalog: String::new(),
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let catalog = repo.catalog(&project_id).await.unwrap_or_default();
        Json(MemoryCatalogResponse {
            catalog,
            error: None,
        })
    }

    /// Returns aggregate health report (total notes, broken links, orphan notes,
    /// duplicate clusters, low-confidence notes, stale note totals, stale notes by folder).
    #[tool(
        description = "Returns aggregate health report (total notes, broken links, orphan notes, low-confidence notes, stale note totals, stale notes by folder, lifecycle counts, and recent lifecycle sweep metrics)."
    )]
    pub async fn memory_health(
        &self,
        Parameters(p): Parameters<HealthParams>,
    ) -> Json<MemoryHealthResponse> {
        Json(ops::memory_health(self, p).await)
    }

    /// Audit existing extracted case/pattern/pitfall notes against ADR-054
    /// taxonomy and template expectations. Output is grouped into merge,
    /// strengthening, demotion, and archive backlogs so cleanup can be rerun.
    #[tool(
        description = "Audit existing extracted case/pattern/pitfall notes against ADR-054 taxonomy and template expectations. Returns grouped cleanup backlogs for merge candidates, underspecified notes, demotion-to-working-spec candidates, and archive candidates, plus rerun guidance."
    )]
    pub async fn memory_extracted_audit(
        &self,
        Parameters(params): Parameters<ExtractedAuditParams>,
    ) -> Json<MemoryExtractedAuditResponse> {
        Json(ops::memory_extracted_audit(self, params).await)
    }

    /// List recently updated notes by timeframe (e.g., '7d', '24h', 'today').
    /// Returns compact summaries.
    #[tool(
        description = "List recently updated notes by timeframe (e.g., '7d', '24h', 'today'). Returns compact summaries."
    )]
    pub async fn memory_recent(
        &self,
        Parameters(p): Parameters<RecentParams>,
    ) -> Json<MemoryRecentResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryRecentResponse {
                notes: vec![],
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let hours = parse_timeframe(p.timeframe.as_deref().unwrap_or("7d"));
        let limit = p.limit.unwrap_or(10).clamp(1, 100);

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let notes = repo
            .recent(&project_id, hours, limit)
            .await
            .unwrap_or_default();
        Json(MemoryRecentResponse { notes, error: None })
    }

    /// List task IDs that reference a memory note permalink (reverse lookup).
    #[tool(description = "List task IDs that reference a memory note permalink (reverse lookup).")]
    pub async fn memory_task_refs(
        &self,
        Parameters(p): Parameters<TaskRefsParams>,
    ) -> Json<MemoryTaskRefsResponse> {
        let Some(_project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryTaskRefsResponse {
                tasks: vec![],
                proposals: vec![],
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let tasks: Vec<MemoryTaskRefItem> = repo
            .task_refs(&p.permalink)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(parse_task_ref_item)
            .collect();
        let proposals: Vec<MemoryProposalRefItem> = repo
            .proposal_refs(&p.permalink)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(parse_proposal_ref_item)
            .collect();
        Json(MemoryTaskRefsResponse {
            tasks,
            proposals,
            error: None,
        })
    }

    /// Lists all broken wikilinks with source context (permalink, title, raw text,
    /// target permalink).
    #[tool(
        description = "Lists all broken wikilinks with source context (permalink, title, raw text, target permalink)."
    )]
    pub async fn memory_broken_links(
        &self,
        Parameters(params): Parameters<BrokenLinksParams>,
    ) -> Json<MemoryBrokenLinksResponse> {
        let Some(project_id) = self.project_id_for_path(&params.project).await else {
            return Json(MemoryBrokenLinksResponse {
                broken_links: vec![],
                error: Some(format!("project not found: {}", params.project)),
            });
        };
        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let folder = params.folder.filter(|value| !value.is_empty());
        let broken_links = repo
            .broken_links(&project_id, folder.as_deref())
            .await
            .unwrap_or_default();
        Json(MemoryBrokenLinksResponse {
            broken_links,
            error: None,
        })
    }

    /// Lists notes with zero inbound links. Excludes catalogs and singletons
    /// (brief, roadmap, catalog).
    #[tool(
        description = "Lists notes with zero inbound links. Excludes catalogs and singletons (brief, roadmap)."
    )]
    pub async fn memory_orphans(
        &self,
        Parameters(params): Parameters<OrphansParams>,
    ) -> Json<MemoryOrphansResponse> {
        let Some(project_id) = self.project_id_for_path(&params.project).await else {
            return Json(MemoryOrphansResponse {
                orphans: vec![],
                error: Some(format!("project not found: {}", params.project)),
            });
        };
        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let folder = params.folder.filter(|value| !value.is_empty());
        let orphans = repo
            .orphans(&project_id, folder.as_deref())
            .await
            .unwrap_or_default();
        Json(MemoryOrphansResponse {
            orphans,
            error: None,
        })
    }
}
