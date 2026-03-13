use super::*;

#[tool_router(router = memory_reads_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Read a note by permalink or title. Updates last_accessed timestamp.
    #[tool(description = "Read a note by permalink or title. Updates last_accessed timestamp.")]
    pub async fn memory_read(
        &self,
        Parameters(p): Parameters<ReadParams>,
    ) -> Json<MemoryNoteResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryNoteResponse::error(format!(
                "project not found: {}",
                p.project
            )));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        // Try permalink first, then title search.
        let note = match repo.get_by_permalink(&project_id, &p.identifier).await {
            Ok(Some(n)) => n,
            _ => {
                // Fallback: search by title
                match repo.search(&project_id, &p.identifier, None, None, 1).await {
                    Ok(results) if !results.is_empty() => {
                        match repo.get(&results[0].id).await.ok().flatten() {
                            Some(n) => n,
                            None => {
                                return Json(MemoryNoteResponse::error(format!(
                                    "note not found: {}",
                                    p.identifier
                                )));
                            }
                        }
                    }
                    _ => {
                        return Json(MemoryNoteResponse::error(format!(
                            "note not found: {}",
                            p.identifier
                        )));
                    }
                }
            }
        };

        // Update last_accessed in the background (best-effort).
        let _ = repo.touch_accessed(&note.id).await;

        Json(MemoryNoteResponse::from_note(&note))
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
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryListResponse {
                notes: vec![],
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let depth = p.depth.unwrap_or(1);

        let notes = repo
            .list_compact(
                &project_id,
                p.folder.as_deref(),
                p.note_type.as_deref(),
                depth,
            )
            .await
            .unwrap_or_default();

        Json(MemoryListResponse { notes, error: None })
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

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let catalog = repo.catalog(&project_id).await.unwrap_or_default();
        Json(MemoryCatalogResponse {
            catalog,
            error: None,
        })
    }

    /// Returns aggregate health report (total notes, broken links, orphan notes,
    /// stale notes by folder).
    #[tool(
        description = "Returns aggregate health report (total notes, broken links, orphan notes, stale notes by folder)."
    )]
    pub async fn memory_health(
        &self,
        Parameters(p): Parameters<HealthParams>,
    ) -> Json<MemoryHealthResponse> {
        let project_path = match &p.project {
            Some(path) => path.clone(),
            None => {
                return Json(MemoryHealthResponse {
                    total_notes: None,
                    broken_link_count: None,
                    orphan_note_count: None,
                    stale_notes_by_folder: None,
                    error: Some("project parameter required".to_string()),
                });
            }
        };

        let Some(project_id) = self.project_id_for_path(&project_path).await else {
            return Json(MemoryHealthResponse {
                total_notes: None,
                broken_link_count: None,
                orphan_note_count: None,
                stale_notes_by_folder: None,
                error: Some(format!("project not found: {project_path}")),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        match repo.health(&project_id).await {
            Ok(h) => Json(MemoryHealthResponse {
                total_notes: Some(h.total_notes),
                broken_link_count: Some(h.broken_link_count),
                orphan_note_count: Some(h.orphan_note_count),
                stale_notes_by_folder: Some(h.stale_notes_by_folder),
                error: None,
            }),
            Err(e) => Json(MemoryHealthResponse {
                total_notes: None,
                broken_link_count: None,
                orphan_note_count: None,
                stale_notes_by_folder: None,
                error: Some(e.to_string()),
            }),
        }
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

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let notes = repo
            .recent(&project_id, hours, limit)
            .await
            .unwrap_or_default();
        Json(MemoryRecentResponse { notes, error: None })
    }

    /// Get git log entries for a .djinn/ file. Returns chronological history with
    /// commit messages, timestamps, authors, and stats.
    #[tool(
        description = "Get git log entries for a .djinn/ file. Returns chronological history with commit messages, timestamps, authors, and stats."
    )]
    pub async fn memory_history(
        &self,
        Parameters(p): Parameters<HistoryParams>,
    ) -> Json<MemoryHistoryResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryHistoryResponse {
                history: vec![],
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(note) = repo
            .get_by_permalink(&project_id, &p.permalink)
            .await
            .ok()
            .flatten()
        else {
            return Json(MemoryHistoryResponse {
                history: vec![],
                error: Some(format!("note not found: {}", p.permalink)),
            });
        };

        let limit = p.limit.unwrap_or(20).clamp(1, 100);
        let history = git_log_for_file(&note.file_path, limit).await;
        Json(MemoryHistoryResponse {
            history,
            error: None,
        })
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
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let tasks: Vec<MemoryTaskRefItem> = repo
            .task_refs(&p.permalink)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(parse_task_ref_item)
            .collect();
        Json(MemoryTaskRefsResponse { tasks, error: None })
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
        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let broken_links = repo
            .broken_links(&project_id, params.folder.as_deref())
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
        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let orphans = repo
            .orphans(&project_id, params.folder.as_deref())
            .await
            .unwrap_or_default();
        Json(MemoryOrphansResponse {
            orphans,
            error: None,
        })
    }
}
