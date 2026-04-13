use super::ops;
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
        Json(ops::memory_search(self, p, None).await)
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
        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus())
            .with_embedding_provider(self.state.embedding_provider())
            .with_vector_store(self.state.vector_store());
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

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus())
            .with_vector_store(self.state.vector_store());

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

        if note.storage != "file" {
            return Json(MemoryDiffResponse {
                diff: String::new(),
                error: Some(format!(
                    "note '{}' is stored in database only (storage='{}'); git diff is only available for file-backed notes",
                    p.permalink, note.storage
                )),
            });
        }

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

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus())
            .with_vector_store(self.state.vector_store());
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

    /// Build context from a seed note with progressive disclosure and token budget
    /// awareness. Returns full content for primary notes, overview for direct (L1)
    /// linked notes, and abstract for discovered (L0) related notes.
    #[tool(
        description = "Build context from a seed note with progressive disclosure. Returns full content for primary notes, overview for direct linked notes, and abstract for discovered related notes. Seed notes are never dropped by budget constraints."
    )]
    pub async fn memory_build_context(
        &self,
        Parameters(p): Parameters<BuildContextParams>,
    ) -> Json<MemoryBuildContextResponse> {
        let task_id = p.task_id.clone();
        Json(ops::memory_build_context(self, p, task_id.as_deref()).await)
    }
}
