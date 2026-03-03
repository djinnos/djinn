// MCP tools for knowledge base operations: CRUD, search, graph, git history,
// health reporting, and memory↔task reference tracking.

use std::path::Path;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::note::NoteRepository;
use crate::mcp::server::DjinnMcpServer;
use crate::models::note::{
    BrokenLink, BuildContextResponse, GitLogEntry, GraphResponse, Note, NoteCompact, OrphanNote,
};

// ── Param structs ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteParams {
    /// Absolute path to the project directory.
    pub project: String,
    pub title: String,
    pub content: String,
    /// Note type: adr, pattern, research, requirement, reference, design,
    /// session, persona, journey, design_spec, competitive, tech_spike,
    /// brief (singleton), roadmap (singleton).
    #[schemars(rename = "type")]
    pub note_type: String,
    pub tags: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadParams {
    pub project: String,
    /// Note permalink (e.g. "decisions/my-adr") or title.
    pub identifier: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EditParams {
    pub project: String,
    /// Note permalink or title.
    pub identifier: String,
    /// Operation: "append", "prepend", "find_replace", "replace_section".
    pub operation: String,
    pub content: String,
    /// Required for find_replace: exact text to search for.
    pub find_text: Option<String>,
    /// Required for replace_section: heading text identifying the section.
    pub section: Option<String>,
    /// If provided and different from current type, move the note to the new
    /// type's folder. Allowed values same as memory_write type.
    #[schemars(rename = "type")]
    pub note_type: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    pub project: String,
    pub query: String,
    pub folder: Option<String>,
    #[schemars(rename = "type")]
    pub note_type: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListParams {
    pub project: String,
    pub folder: String,
    /// Depth control: 0 = unlimited, 1 = exact folder (default), N = N levels.
    pub depth: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DeleteParams {
    pub project: String,
    pub identifier: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct MoveParams {
    pub project: String,
    pub identifier: String,
    /// New note type to move the note to.
    #[schemars(rename = "type")]
    pub note_type: String,
    /// Optional new title; keep current title if omitted.
    pub title: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HealthParams {
    /// Absolute path to the project directory.
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CatalogParams {
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecentParams {
    pub project: String,
    /// Timeframe string, e.g. "7d", "24h", "today", "last week". Default: "7d".
    pub timeframe: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    pub project: String,
    pub permalink: String,
    pub limit: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DiffParams {
    pub project: String,
    pub permalink: String,
    /// Specific commit SHA. Omit to get the diff for the most recent change.
    pub sha: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BuildContextParams {
    pub project: String,
    /// Memory URI: "memory://folder/note", "folder/note", or "folder/*" for all
    /// notes in a folder.
    pub url: String,
    /// Link traversal depth (default 1).
    pub depth: Option<i64>,
    /// Maximum related notes to return (default 10).
    pub max_related: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskRefsParams {
    pub project: String,
    pub permalink: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GraphParams {
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BrokenLinksParams {
    pub project: String,
    pub folder: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct OrphansParams {
    pub project: String,
    pub folder: Option<String>,
}

// ── Response wrappers ─────────────────────────────────────────────────────────

#[derive(Serialize, schemars::JsonSchema)]
pub struct NoteListResponse {
    pub notes: Vec<NoteCompact>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SearchResponse {
    pub results: Vec<serde_json::Value>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct DeleteResponse {
    pub ok: bool,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct BrokenLinksResponse {
    pub broken_links: Vec<BrokenLink>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct OrphansResponse {
    pub orphans: Vec<OrphanNote>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct CatalogResponse {
    pub catalog: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct HistoryResponse {
    pub history: Vec<GitLogEntry>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct DiffResponse {
    pub diff: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskRefsResponse {
    pub tasks: Vec<serde_json::Value>,
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = memory_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create or update a note. Type is required and determines storage folder.
    /// Singleton types (brief, roadmap) write a fixed file — one per project.
    #[tool(description = "Create or update a note. Type is required and determines storage folder (adr->decisions/, pattern->patterns/, research->research/, requirement->requirements/, reference->reference/, design->design/, persona->design/personas, journey->design/journeys, design_spec->design/specs, session->research/sessions, competitive->research/competitive, tech_spike->research/technical). Singleton types (brief, roadmap) write a fixed file at docs root — one per project, title is ignored. Use [[wikilinks]] in content to connect notes — any [[Note Title]] creates a link in the knowledge graph. Add a '## Relations' section at the bottom with '- [[Related Note]]' entries to make connections explicit. For large documents (>150 lines): create with initial content, then use memory_edit with operation=\"append\" to add remaining sections.")]
    pub async fn memory_write(
        &self,
        Parameters(p): Parameters<WriteParams>,
    ) -> Json<serde_json::Value> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(serde_json::json!({ "error": format!("project not found: {}", p.project) }));
        };

        let tags_json = p
            .tags
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|| "[]".to_string());

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        // For singletons, upsert: try create, fall back to update if already exists.
        use crate::db::repositories::note::is_singleton;
        if is_singleton(&p.note_type) {
            match repo
                .get_by_permalink(&project_id, &p.note_type)
                .await
                .ok()
                .flatten()
            {
                Some(existing) => {
                    match repo
                        .update(&existing.id, &p.title, &p.content, &tags_json)
                        .await
                    {
                        Ok(note) => return Json(note_to_value(&note)),
                        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
                    }
                }
                None => {}
            }
        }

        match repo
            .create(
                &project_id,
                Path::new(&p.project),
                &p.title,
                &p.content,
                &p.note_type,
                &tags_json,
            )
            .await
        {
            Ok(note) => Json(note_to_value(&note)),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Read a note by permalink or title. Updates last_accessed timestamp.
    #[tool(description = "Read a note by permalink or title. Updates last_accessed timestamp.")]
    pub async fn memory_read(
        &self,
        Parameters(p): Parameters<ReadParams>,
    ) -> Json<serde_json::Value> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(serde_json::json!({ "error": format!("project not found: {}", p.project) }));
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
                                return Json(serde_json::json!({
                                    "error": format!("note not found: {}", p.identifier)
                                }))
                            }
                        }
                    }
                    _ => {
                        return Json(
                            serde_json::json!({ "error": format!("note not found: {}", p.identifier) }),
                        )
                    }
                }
            }
        };

        // Update last_accessed in the background (best-effort).
        let _ = repo.touch_accessed(&note.id).await;

        Json(note_to_value(&note))
    }

    /// Edit an existing note. Operations: "append" (add to end), "prepend" (add
    /// after frontmatter), "find_replace" (exact text replacement, requires
    /// find_text), "replace_section" (replace content under a markdown heading,
    /// requires section). Use append to build large notes incrementally after
    /// memory_write creates the initial note. When type is provided and differs
    /// from current type, the note is automatically moved to the correct folder
    /// for the new type.
    #[tool(description = "Edit an existing note. Operations: \"append\" (add to end), \"prepend\" (add after frontmatter), \"find_replace\" (exact text replacement, requires find_text), \"replace_section\" (replace content under a markdown heading, requires section). Use append to build large notes incrementally after memory_write creates the initial note. When type is provided and differs from current type, the note is automatically moved to the correct folder for the new type.")]
    pub async fn memory_edit(
        &self,
        Parameters(p): Parameters<EditParams>,
    ) -> Json<serde_json::Value> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(serde_json::json!({ "error": format!("project not found: {}", p.project) }));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        // Resolve note by permalink or title.
        let note = match resolve_note_by_identifier(&repo, &project_id, &p.identifier).await {
            Some(n) => n,
            None => {
                return Json(
                    serde_json::json!({ "error": format!("note not found: {}", p.identifier) }),
                )
            }
        };

        // If type changed, move first.
        let note = if let Some(ref new_type) = p.note_type {
            if new_type != &note.note_type {
                match repo
                    .move_note(&note.id, Path::new(&p.project), &note.title, new_type)
                    .await
                {
                    Ok(moved) => moved,
                    Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
                }
            } else {
                note
            }
        } else {
            note
        };

        // Apply edit operation.
        let new_content = match apply_edit_operation(
            &note.content,
            &p.operation,
            &p.content,
            p.find_text.as_deref(),
            p.section.as_deref(),
        ) {
            Ok(c) => c,
            Err(e) => return Json(serde_json::json!({ "error": e })),
        };

        match repo.update(&note.id, &note.title, &new_content, &note.tags).await {
            Ok(updated) => Json(note_to_value(&updated)),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Search notes using FTS5 full-text search with BM25 ranking. Returns compact
    /// results with snippets.
    #[tool(description = "Search notes using FTS5 full-text search with BM25 ranking. Returns compact results with snippets.")]
    pub async fn memory_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Json<SearchResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(SearchResponse { results: vec![] });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let limit = p.limit.unwrap_or(10).clamp(1, 100) as usize;

        let results = repo
            .search(
                &project_id,
                &p.query,
                p.folder.as_deref(),
                p.note_type.as_deref(),
                limit,
            )
            .await
            .unwrap_or_default();

        let items = results
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "id":        r.id,
                    "permalink": r.permalink,
                    "title":     r.title,
                    "folder":    r.folder,
                    "note_type": r.note_type,
                    "snippet":   r.snippet,
                })
            })
            .collect();

        Json(SearchResponse { results: items })
    }

    /// List notes in a folder with depth control. Returns compact summaries
    /// without full content.
    #[tool(description = "List notes in a folder with depth control. Returns compact summaries without full content.")]
    pub async fn memory_list(
        &self,
        Parameters(p): Parameters<ListParams>,
    ) -> Json<NoteListResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(NoteListResponse { notes: vec![] });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let depth = p.depth.unwrap_or(1);

        let notes = repo
            .list_compact(&project_id, &p.folder, depth)
            .await
            .unwrap_or_default();

        Json(NoteListResponse { notes })
    }

    /// Delete a note. Removes file and index entry.
    #[tool(description = "Delete a note. Removes file and index entry.")]
    pub async fn memory_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Json<serde_json::Value> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(serde_json::json!({ "error": format!("project not found: {}", p.project) }));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(note) =
            resolve_note_by_identifier(&repo, &project_id, &p.identifier).await
        else {
            return Json(
                serde_json::json!({ "error": format!("note not found: {}", p.identifier) }),
            );
        };

        match repo.delete(&note.id).await {
            Ok(()) => Json(serde_json::json!({ "ok": true })),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Move a note to a new location. Updates permalink and resolves inbound links.
    #[tool(description = "Move a note to a new location. Updates permalink and resolves inbound links.")]
    pub async fn memory_move(
        &self,
        Parameters(p): Parameters<MoveParams>,
    ) -> Json<serde_json::Value> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(serde_json::json!({ "error": format!("project not found: {}", p.project) }));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(note) =
            resolve_note_by_identifier(&repo, &project_id, &p.identifier).await
        else {
            return Json(
                serde_json::json!({ "error": format!("note not found: {}", p.identifier) }),
            );
        };

        let new_title = p.title.as_deref().unwrap_or(&note.title);

        match repo
            .move_note(&note.id, Path::new(&p.project), new_title, &p.note_type)
            .await
        {
            Ok(moved) => Json(note_to_value(&moved)),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Returns aggregate health report (total notes, broken links, orphan notes,
    /// stale notes by folder).
    #[tool(description = "Returns aggregate health report (total notes, broken links, orphan notes, stale notes by folder).")]
    pub async fn memory_health(
        &self,
        Parameters(p): Parameters<HealthParams>,
    ) -> Json<serde_json::Value> {
        let project_path = match &p.project {
            Some(path) => path.clone(),
            None => return Json(serde_json::json!({ "error": "project parameter required" })),
        };

        let Some(project_id) = self.project_id_for_path(&project_path).await else {
            return Json(serde_json::json!({ "error": format!("project not found: {project_path}") }));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        match repo.health(&project_id).await {
            Ok(h) => Json(serde_json::to_value(&h).unwrap_or(serde_json::json!({}))),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Read the auto-generated knowledge base catalog. Returns the full catalog
    /// markdown content — the master table of contents for all notes in the KB.
    #[tool(description = "Read the auto-generated knowledge base catalog. Returns the full catalog markdown content — the master table of contents for all notes in the KB. Use this as the first tool call when orienting yourself in a new session: it tells you exactly what knowledge exists and where to find it. Read-only — does not modify any notes or the SQLite index.")]
    pub async fn memory_catalog(
        &self,
        Parameters(p): Parameters<CatalogParams>,
    ) -> Json<CatalogResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(CatalogResponse { catalog: String::new() });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let catalog = repo.catalog(&project_id).await.unwrap_or_default();
        Json(CatalogResponse { catalog })
    }

    /// List recently updated notes by timeframe (e.g., '7d', '24h', 'today').
    /// Returns compact summaries.
    #[tool(description = "List recently updated notes by timeframe (e.g., '7d', '24h', 'today'). Returns compact summaries.")]
    pub async fn memory_recent(
        &self,
        Parameters(p): Parameters<RecentParams>,
    ) -> Json<NoteListResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(NoteListResponse { notes: vec![] });
        };

        let hours = parse_timeframe(p.timeframe.as_deref().unwrap_or("7d"));
        let limit = p.limit.unwrap_or(10).clamp(1, 100);

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let notes = repo.recent(&project_id, hours, limit).await.unwrap_or_default();
        Json(NoteListResponse { notes })
    }

    /// Get git log entries for a docs/ file. Returns chronological history with
    /// commit messages, timestamps, authors, and stats.
    #[tool(description = "Get git log entries for a docs/ file. Returns chronological history with commit messages, timestamps, authors, and stats.")]
    pub async fn memory_history(
        &self,
        Parameters(p): Parameters<HistoryParams>,
    ) -> Json<serde_json::Value> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(serde_json::json!({ "error": format!("project not found: {}", p.project) }));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(note) = repo.get_by_permalink(&project_id, &p.permalink).await.ok().flatten()
        else {
            return Json(serde_json::json!({ "error": format!("note not found: {}", p.permalink) }));
        };

        let limit = p.limit.unwrap_or(20).clamp(1, 100);
        let history = git_log_for_file(&note.file_path, limit).await;
        Json(serde_json::json!({ "history": history }))
    }

    /// Get unified diff for a specific commit of a docs/ file. No SHA = returns
    /// diff for most recent change.
    #[tool(description = "Get unified diff for a specific commit of a docs/ file. No SHA = returns diff for most recent change.")]
    pub async fn memory_diff(
        &self,
        Parameters(p): Parameters<DiffParams>,
    ) -> Json<DiffResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(DiffResponse { diff: String::new() });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(note) = repo.get_by_permalink(&project_id, &p.permalink).await.ok().flatten()
        else {
            return Json(DiffResponse { diff: String::new() });
        };

        let diff = git_diff_for_file(&note.file_path, p.sha.as_deref()).await;
        Json(DiffResponse { diff })
    }

    /// Build context from a seed note by traversing links. Returns full content
    /// for primary notes and summaries for related notes.
    #[tool(description = "Build context from a seed note by traversing links. Returns full content for primary notes and summaries for related notes.")]
    pub async fn memory_build_context(
        &self,
        Parameters(p): Parameters<BuildContextParams>,
    ) -> Json<BuildContextResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(BuildContextResponse { primary: vec![], related: vec![] });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let depth = p.depth.unwrap_or(1).max(0);
        let max_related = p.max_related.unwrap_or(10).clamp(1, 50) as usize;

        // Strip memory:// prefix.
        let url = p.url.strip_prefix("memory://").unwrap_or(&p.url);

        // Wildcard: return all notes in folder as primary.
        if url.ends_with("/*") {
            let folder = url.trim_end_matches("/*");
            let all = repo.list(&project_id, Some(folder)).await.unwrap_or_default();
            return Json(BuildContextResponse { primary: all, related: vec![] });
        }

        // Single note lookup.
        let Some(seed) = repo.get_by_permalink(&project_id, url).await.ok().flatten() else {
            return Json(BuildContextResponse { primary: vec![], related: vec![] });
        };

        if depth == 0 {
            return Json(BuildContextResponse { primary: vec![seed], related: vec![] });
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

        let mut related = Vec::new();
        for id in related_ids.into_iter().take(max_related) {
            if let Ok(Some(n)) = repo.get(&id).await {
                use crate::models::note::NoteCompact;
                related.push(NoteCompact {
                    id: n.id,
                    permalink: n.permalink,
                    title: n.title,
                    note_type: n.note_type,
                    folder: n.folder,
                    updated_at: n.updated_at,
                });
            }
        }

        Json(BuildContextResponse { primary: vec![seed], related })
    }

    /// List task IDs that reference a memory note permalink (reverse lookup).
    #[tool(description = "List task IDs that reference a memory note permalink (reverse lookup).")]
    pub async fn memory_task_refs(
        &self,
        Parameters(p): Parameters<TaskRefsParams>,
    ) -> Json<TaskRefsResponse> {
        let repo = NoteRepository::new(self.state.db().clone(), self.state.events().clone());
        let tasks = repo.task_refs(&p.permalink).await.unwrap_or_default();
        Json(TaskRefsResponse { tasks })
    }

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

    /// Lists all broken wikilinks with source context (permalink, title, raw text,
    /// target permalink).
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

    /// Lists notes with zero inbound links. Excludes catalogs and singletons
    /// (brief, roadmap, catalog).
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
        let db = self.state.db();
        db.ensure_initialized().await.ok()?;
        sqlx::query_scalar::<_, String>("SELECT id FROM projects WHERE path = ?1")
            .bind(project_path)
            .fetch_optional(db.pool())
            .await
            .ok()
            .flatten()
    }
}

/// Resolve a note by permalink (primary) or title search (fallback).
async fn resolve_note_by_identifier(
    repo: &NoteRepository,
    project_id: &str,
    identifier: &str,
) -> Option<Note> {
    if let Ok(Some(n)) = repo.get_by_permalink(project_id, identifier).await {
        return Some(n);
    }
    // Fallback: search by title
    if let Ok(results) = repo.search(project_id, identifier, None, None, 1).await {
        if let Some(r) = results.into_iter().next() {
            return repo.get(&r.id).await.ok().flatten();
        }
    }
    None
}

/// Serialize a `Note` to a JSON value with tags parsed as an array.
fn note_to_value(note: &Note) -> serde_json::Value {
    let tags: serde_json::Value =
        serde_json::from_str(&note.tags).unwrap_or(serde_json::json!([]));
    serde_json::json!({
        "id":            note.id,
        "project_id":    note.project_id,
        "permalink":     note.permalink,
        "title":         note.title,
        "file_path":     note.file_path,
        "note_type":     note.note_type,
        "folder":        note.folder,
        "tags":          tags,
        "content":       note.content,
        "created_at":    note.created_at,
        "updated_at":    note.updated_at,
        "last_accessed": note.last_accessed,
    })
}

/// Apply an edit operation to the current content, returning the new content.
fn apply_edit_operation(
    content: &str,
    operation: &str,
    new_content: &str,
    find_text: Option<&str>,
    section: Option<&str>,
) -> Result<String, String> {
    match operation {
        "append" => Ok(if content.is_empty() {
            new_content.to_string()
        } else {
            format!("{content}\n\n{new_content}")
        }),
        "prepend" => Ok(if content.is_empty() {
            new_content.to_string()
        } else {
            format!("{new_content}\n\n{content}")
        }),
        "find_replace" => {
            let find = find_text.ok_or("find_replace requires find_text")?;
            if !content.contains(find) {
                return Err(format!("text not found: '{find}'"));
            }
            Ok(content.replacen(find, new_content, 1))
        }
        "replace_section" => {
            let heading = section.ok_or("replace_section requires section")?;
            replace_section_in_content(content, heading, new_content)
        }
        other => Err(format!("unknown operation: '{other}'")),
    }
}

/// Replace the body under a markdown heading with `new_body`.
///
/// The heading line itself is preserved; content from the line after the heading
/// to the next heading at the same or higher level (or EOF) is replaced.
fn replace_section_in_content(
    content: &str,
    section: &str,
    new_body: &str,
) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();

    let heading_idx = lines.iter().position(|l| {
        let stripped = l.trim_start_matches('#');
        l.starts_with('#') && stripped.trim().eq_ignore_ascii_case(section)
    });

    let start = heading_idx.ok_or_else(|| format!("section '{section}' not found"))?;
    let heading_level = lines[start].chars().take_while(|&c| c == '#').count();

    let end = lines[start + 1..]
        .iter()
        .position(|l| {
            let lvl = l.chars().take_while(|&c| c == '#').count();
            lvl > 0 && lvl <= heading_level
        })
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());

    let mut result = lines[..=start].join("\n");
    result.push('\n');
    result.push_str(new_body);
    if !new_body.is_empty() && !new_body.ends_with('\n') && end < lines.len() {
        result.push('\n');
    }
    if end < lines.len() {
        result.push('\n');
        result.push_str(&lines[end..].join("\n"));
    }

    Ok(result)
}

/// Parse a human-readable timeframe string into hours.
///
/// Supports: "Xd", "Xh", "today", "last week", and raw integers (hours).
fn parse_timeframe(s: &str) -> i64 {
    let s = s.trim().to_lowercase();
    if s == "today" {
        return 24;
    }
    if s == "last week" || s == "lastweek" {
        return 168;
    }
    if let Some(n) = s.strip_suffix('d') {
        return n.trim().parse::<i64>().unwrap_or(7) * 24;
    }
    if let Some(n) = s.strip_suffix('h') {
        return n.trim().parse::<i64>().unwrap_or(24);
    }
    s.parse::<i64>().unwrap_or(168)
}

/// Run `git log --format="%H|||%s|||%an|||%ai" -n N -- file` and parse entries.
async fn git_log_for_file(file_path: &str, limit: i64) -> Vec<GitLogEntry> {
    let output = tokio::process::Command::new("git")
        .args([
            "log",
            &format!("--format=%H|||%s|||%an|||%ai"),
            &format!("-n{limit}"),
            "--",
            file_path,
        ])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .filter_map(|line| {
                    let parts: Vec<&str> = line.splitn(4, "|||").collect();
                    if parts.len() == 4 {
                        Some(GitLogEntry {
                            sha: parts[0].to_string(),
                            message: parts[1].to_string(),
                            author: parts[2].to_string(),
                            date: parts[3].to_string(),
                        })
                    } else {
                        None
                    }
                })
                .collect()
        }
        _ => vec![],
    }
}

/// Get the unified diff for a note file at a specific commit (or the latest change).
async fn git_diff_for_file(file_path: &str, sha: Option<&str>) -> String {
    let sha = match sha {
        Some(s) => s.to_owned(),
        None => {
            // Find the most recent commit that touched this file.
            let out = tokio::process::Command::new("git")
                .args(["log", "-n1", "--format=%H", "--", file_path])
                .output()
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_owned()
                }
                _ => return String::new(),
            }
        }
    };

    if sha.is_empty() {
        return String::new();
    }

    let out = tokio::process::Command::new("git")
        .args(["diff", &format!("{sha}^"), &sha, "--", file_path])
        .output()
        .await;

    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => String::new(),
    }
}
