use super::ops;
use super::*;

#[tool_router(router = memory_search_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Search notes (and proposals) using FTS full-text search with BM25
    /// ranking. Returns unified results with snippets and an `entity`
    /// discriminator (`"note"` or `"proposal"`). Optionally filter graph
    /// traversal by edge kinds, or scope to a subset of entity types.
    #[tool(
        description = "Unified memory search across notes and proposals with BM25 ranking. Returns compact results with snippets. Each result carries an `entity` field (\"note\" or \"proposal\"). The optional `entity_types` parameter scopes results (omit/null = both, [\"note\"] = notes-only, [\"proposal\"] = proposals-only, [] = no results). The optional edge_kinds parameter filters which association edge kinds participate in graph proximity scoring during retrieval."
    )]
    pub async fn memory_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Json<MemorySearchResponse> {
        Json(ops::memory_search(self, p, None).await)
    }

    /// Returns the full knowledge graph for visualization — all notes with
    /// connection counts, all resolved wikilink edges, and typed semantic edges
    /// (builds_on, contradicts, supersedes, exemplifies, derived_from) in a
    /// single query.
    #[tool(
        description = "Returns the full knowledge graph for visualization — all notes with connection counts, all resolved wikilink edges, and typed semantic edges (builds_on, contradicts, supersedes, exemplifies, derived_from) in a single query."
    )]
    pub async fn memory_graph(
        &self,
        Parameters(params): Parameters<GraphParams>,
    ) -> Json<MemoryGraphResponse> {
        let Some(project_id) = self.project_id_for_path(&params.project).await else {
            return Json(MemoryGraphResponse {
                nodes: vec![],
                edges: vec![],
                typed_edges: vec![],
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
            typed_edges: graph.typed_edges,
            error: None,
        })
    }

    /// Build context from a seed note with progressive disclosure and token budget
    /// awareness. Returns full content for primary notes, overview for direct (L1)
    /// linked notes, abstract for discovered (L0) related notes, plus supersedes
    /// and contradicts annotations. Relevant proposals surface as related entities.
    #[tool(
        description = "Build context from a seed note with progressive disclosure. Returns full content for primary notes, overview for direct linked notes, abstract for discovered related notes, plus supersedes/contradicts annotations. Relevant proposals surface as related entities (title + acceptance criteria as the overview; call proposal_show for the full body). Seed notes are never dropped by budget constraints. The optional edge_kinds parameter filters which association edge kinds participate in graph proximity scoring."
    )]
    pub async fn memory_build_context(
        &self,
        Parameters(p): Parameters<BuildContextParams>,
    ) -> Json<MemoryBuildContextResponse> {
        let task_id = p.task_id.clone();
        Json(ops::memory_build_context(self, p, task_id.as_deref()).await)
    }
}
