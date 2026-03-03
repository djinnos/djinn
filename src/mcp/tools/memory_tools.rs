// MCP tools for knowledge base operations: wikilink graph, orphan detection,
// and broken link detection.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::connection::OptionalExt;
use crate::db::repositories::note::NoteRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::models::note::{BrokenLink, GraphResponse, OrphanNote};

// ── Param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GraphParams {
    /// Absolute path to the project directory.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BrokenLinksParams {
    /// Absolute path to the project directory.
    pub project: String,
    /// Optional folder to restrict results to (e.g. "decisions").
    pub folder: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct OrphansParams {
    /// Absolute path to the project directory.
    pub project: String,
    /// Optional folder to restrict results to (e.g. "decisions").
    pub folder: Option<String>,
}

// ── Response wrappers ─────────────────────────────────────────────────────────

#[derive(Serialize, schemars::JsonSchema)]
pub struct BrokenLinksResponse {
    pub broken_links: Vec<BrokenLink>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct OrphansResponse {
    pub orphans: Vec<OrphanNote>,
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = memory_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Returns the full knowledge graph for visualization — all notes with
    /// connection counts and all resolved wikilink edges in a single query.
    #[tool(description = "Returns the full knowledge graph for visualization — all notes with connection counts and all resolved wikilink edges in a single query.")]
    pub async fn memory_graph(
        &self,
        Parameters(params): Parameters<GraphParams>,
    ) -> Json<GraphResponse> {
        let Some(project_id) = self.project_id_for_path(&params.project).await else {
            return Json(GraphResponse { nodes: vec![], edges: vec![] });
        };
        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        Json(
            repo.graph(&project_id)
                .await
                .unwrap_or(GraphResponse { nodes: vec![], edges: vec![] }),
        )
    }

    /// Lists all broken wikilinks with source context (permalink, title, raw text, target permalink).
    #[tool(description = "Lists all broken wikilinks with source context (permalink, title, raw text, target permalink).")]
    pub async fn memory_broken_links(
        &self,
        Parameters(params): Parameters<BrokenLinksParams>,
    ) -> Json<BrokenLinksResponse> {
        let Some(project_id) = self.project_id_for_path(&params.project).await else {
            return Json(BrokenLinksResponse { broken_links: vec![] });
        };
        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let broken_links = repo
            .broken_links(&project_id, params.folder.as_deref())
            .await
            .unwrap_or_default();
        Json(BrokenLinksResponse { broken_links })
    }

    /// Lists notes with zero inbound links. Excludes catalogs and singletons (brief, roadmap).
    #[tool(description = "Lists notes with zero inbound links. Excludes catalogs and singletons (brief, roadmap).")]
    pub async fn memory_orphans(
        &self,
        Parameters(params): Parameters<OrphansParams>,
    ) -> Json<OrphansResponse> {
        let Some(project_id) = self.project_id_for_path(&params.project).await else {
            return Json(OrphansResponse { orphans: vec![] });
        };
        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let orphans = repo
            .orphans(&project_id, params.folder.as_deref())
            .await
            .unwrap_or_default();
        Json(OrphansResponse { orphans })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Resolve an absolute project path to its DB project_id.
    pub(crate) async fn project_id_for_path(&self, project_path: &str) -> Option<String> {
        let path = project_path.to_owned();
        self.state
            .db()
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id FROM projects WHERE path = ?1",
                        [&path],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .await
            .ok()
            .flatten()
    }
}
