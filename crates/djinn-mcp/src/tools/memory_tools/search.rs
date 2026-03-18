use super::*;

#[tool_router(router = memory_search_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Search notes using FTS5 full-text search with BM25 ranking. Returns compact
    /// results with snippets.
    #[tool(
        description = "Search notes using FTS5 full-text search with BM25 ranking. Returns compact results with snippets."
    )]
    pub async fn memory_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Json<MemorySearchResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemorySearchResponse {
                results: vec![],
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let limit = p.limit.unwrap_or(10).clamp(1, 100) as usize;

        let results = match repo
            .search(
                &project_id,
                &p.query,
                None,
                p.folder.as_deref(),
                p.note_type.as_deref(),
                limit,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Json(MemorySearchResponse {
                    results: vec![],
                    error: Some(format!("search failed: {e}")),
                });
            }
        };

        let items: Vec<MemorySearchResultItem> = results
            .into_iter()
            .map(|r| MemorySearchResultItem {
                id: r.id,
                permalink: r.permalink,
                title: r.title,
                folder: r.folder,
                note_type: r.note_type,
                snippet: r.snippet,
            })
            .collect();

        Json(MemorySearchResponse {
            results: items,
            error: None,
        })
    }

    /// Returns the full knowledge graph for visualization — all notes with
    /// connection counts and all resolved wikilink edges in a single query.
    #[tool(
        description = "Returns the full knowledge graph for visualization — all notes with connection counts and all resolved wikilink edges in a single query."
    )]
    pub async fn memory_graph(
        &self,
        Parameters(params): Parameters<GraphParams>,
    ) -> Json<MemoryGraphResponse> {
        let Some(project_id) = self.project_id_for_path(&params.project).await else {
            return Json(MemoryGraphResponse {
                nodes: vec![],
                edges: vec![],
                error: Some(format!("project not found: {}", params.project)),
            });
        };
        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let graph = repo.graph(&project_id).await.unwrap_or_default();
        Json(MemoryGraphResponse {
            nodes: graph.nodes,
            edges: graph.edges,
            error: None,
        })
    }

    /// Get unified diff for a specific commit of a .djinn/ file. No SHA = returns
    /// diff for most recent change.
    #[tool(
        description = "Get unified diff for a specific commit of a .djinn/ file. No SHA = returns diff for most recent change."
    )]
    pub async fn memory_diff(
        &self,
        Parameters(p): Parameters<DiffParams>,
    ) -> Json<MemoryDiffResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryDiffResponse {
                diff: String::new(),
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(note) = repo
            .get_by_permalink(&project_id, &p.permalink)
            .await
            .ok()
            .flatten()
        else {
            return Json(MemoryDiffResponse {
                diff: String::new(),
                error: Some(format!("note not found: {}", p.permalink)),
            });
        };

        let diff = git_diff_for_file(&note.file_path, p.sha.as_deref()).await;
        Json(MemoryDiffResponse { diff, error: None })
    }

    /// Re-index all memory notes for a project from disk on demand.
    #[tool(
        description = "Re-index memory notes for a project by scanning note files, comparing checksums to indexed content, and applying create/update/delete changes."
    )]
    pub async fn memory_reindex(
        &self,
        Parameters(params): Parameters<ReindexParams>,
    ) -> Json<MemoryReindexResponse> {
        let Some(project_id) = self.project_id_for_path(&params.project).await else {
            return Json(MemoryReindexResponse {
                updated: 0,
                created: 0,
                deleted: 0,
                unchanged: 0,
                error: Some(format!("project not found: {}", params.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let summary = repo
            .reindex_from_disk(&project_id, Path::new(&params.project))
            .await
            .unwrap_or_else(|_| ReindexSummary::default());

        Json(MemoryReindexResponse {
            updated: summary.updated,
            created: summary.created,
            deleted: summary.deleted,
            unchanged: summary.unchanged,
            error: None,
        })
    }

    /// Build context from a seed note by traversing links. Returns full content
    /// for primary notes and summaries for related notes.
    #[tool(
        description = "Build context from a seed note by traversing links. Returns full content for primary notes and summaries for related notes."
    )]
    pub async fn memory_build_context(
        &self,
        Parameters(p): Parameters<BuildContextParams>,
    ) -> Json<MemoryBuildContextResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryBuildContextResponse {
                primary: vec![],
                related_l1: vec![],
                related_l0: vec![],
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let depth = p.depth.unwrap_or(1).max(0);
        let max_related = p.max_related.unwrap_or(10).clamp(1, 50) as usize;

        // Strip memory:// prefix.
        let url = p.url.strip_prefix("memory://").unwrap_or(&p.url);

        // Wildcard: return all notes in folder as primary.
        if url.ends_with("/*") {
            let folder = url.trim_end_matches("/*");
            let all = repo
                .list(&project_id, Some(folder))
                .await
                .unwrap_or_default();
            return Json(MemoryBuildContextResponse {
                primary: all.into_iter().map(|n| note_to_view(&n)).collect(),
                related_l1: vec![],
                related_l0: vec![],
                error: None,
            });
        }

        // Single note lookup.
        let Some(seed) = repo.get_by_permalink(&project_id, url).await.ok().flatten() else {
            return Json(MemoryBuildContextResponse {
                primary: vec![],
                related_l1: vec![],
                related_l0: vec![],
                error: None,
            });
        };

        if depth == 0 {
            return Json(MemoryBuildContextResponse {
                primary: vec![note_to_view(&seed)],
                related_l1: vec![],
                related_l0: vec![],
                error: None,
            });
        }

        // Traverse outbound wikilinks up to `depth` levels.
        let graph = repo.graph(&project_id).await.unwrap_or_default();
        let mut related_ids = std::collections::HashSet::new();
        let mut frontier = vec![seed.id.clone()];

        for _ in 0..depth {
            let mut next_frontier = vec![];
            for src_id in &frontier {
                for edge in graph.edges.iter().filter(|e| &e.source_id == src_id) {
                    if edge.target_id != seed.id && !related_ids.contains(&edge.target_id) {
                        related_ids.insert(edge.target_id.clone());
                        next_frontier.push(edge.target_id.clone());
                    }
                }
            }
            frontier = next_frontier;
            if frontier.is_empty() {
                break;
            }
        }

        // Build tiered related lists using NoteOverview for L1 and NoteAbstract for L0
        let mut related_l1: Vec<djinn_core::models::NoteOverview> = Vec::new();
        let mut related_l0: Vec<djinn_core::models::NoteAbstract> = Vec::new();
        for (idx, id) in related_ids.into_iter().take(max_related).enumerate() {
            if let Ok(Some(n)) = repo.get(&id).await {
                // First items go to L1 (NoteOverview), rest to L0 (NoteAbstract)
                if idx < max_related / 2 {
                    related_l1.push(djinn_core::models::NoteOverview {
                        id: n.id,
                        permalink: n.permalink,
                        title: n.title,
                        note_type: n.note_type,
                        overview_text: n.overview.unwrap_or_default(),
                        score: None,
                    });
                } else {
                    related_l0.push(djinn_core::models::NoteAbstract {
                        id: n.id,
                        permalink: n.permalink,
                        title: n.title,
                        note_type: n.note_type,
                        abstract_text: n.abstract_.unwrap_or_default(),
                        score: None,
                    });
                }
            }
        }

        Json(MemoryBuildContextResponse {
            primary: vec![note_to_view(&seed)],
            related_l1,
            related_l0,
            error: None,
        })
    }
}
