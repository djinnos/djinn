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
    ///
    /// As of the db-only knowledge-base cut-over, notes no longer have a
    /// physical .md file under git, so this tool now always returns an empty
    /// diff with an error message. Kept on the surface for backward-compat;
    /// callers should switch to `memory_history` against db row updates.
    #[tool(
        description = "Get unified diff for a specific commit of a .djinn/ file. No SHA = returns diff for most recent change. Note: with db-only KB storage, this tool no longer returns a meaningful diff."
    )]
    pub async fn memory_diff(
        &self,
        Parameters(p): Parameters<DiffParams>,
    ) -> Json<MemoryDiffResponse> {
        let _ = p;
        Json(MemoryDiffResponse {
            diff: String::new(),
            error: Some(
                "memory_diff: notes are now stored db-only; on-disk git history is unavailable"
                    .to_string(),
            ),
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
