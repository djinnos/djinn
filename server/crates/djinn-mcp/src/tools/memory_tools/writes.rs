use super::lifecycle::schedule_summary_regeneration;
use super::write_dedup::{LlmMemoryWriteDedupDecider, maybe_apply_write_dedup};
use super::write_dedup_types::{MemoryWriteDedupDecider, PendingWriteDedup};
use super::write_services::{create_note, maybe_update_singleton_note, note_repository};
use super::{
    DeleteParams, EditParams, MemoryDeleteResponse, MemoryNoteResponse, MoveParams, WriteParams,
};

use crate::server::DjinnMcpServer;
use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};
use std::path::PathBuf;

#[tool_router(router = memory_writes_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Create or update a note. Type is required and determines storage folder.
    /// Singleton types (brief, roadmap) write a fixed file — one per project.
    #[tool(
        description = "Create or update a note. Type is required and determines storage folder (adr->decisions/, pattern->patterns/, case->cases/, pitfall->pitfalls/, research->research/, requirement->requirements/, reference->reference/, design->design/, persona->design/personas, journey->design/journeys, design_spec->design/specs, session->research/sessions, competitive->research/competitive, tech_spike->research/technical). Singleton types (brief, roadmap) write a fixed file at docs root — one per project, title is ignored. Use [[wikilinks]] in content to connect notes — any [[Note Title]] creates a link in the knowledge graph. Add a '## Relations' section at the bottom with '- [[Related Note]]' entries to make connections explicit. For large documents (>150 lines): create with initial content, then use memory_edit with operation=\"append\" to add remaining sections."
    )]
    pub async fn memory_write(
        &self,
        Parameters(p): Parameters<WriteParams>,
    ) -> Json<MemoryNoteResponse> {
        self.memory_write_with_worktree(Parameters(p), None).await
    }

    pub async fn memory_write_with_worktree(
        &self,
        Parameters(p): Parameters<WriteParams>,
        worktree_root: Option<PathBuf>,
    ) -> Json<MemoryNoteResponse> {
        self.memory_write_with_worktree_and_decider(
            Parameters(p),
            worktree_root,
            &LlmMemoryWriteDedupDecider::new(self.state.db().clone()),
        )
        .await
    }

    pub(crate) async fn memory_write_with_worktree_and_decider(
        &self,
        Parameters(p): Parameters<WriteParams>,
        worktree_root: Option<PathBuf>,
        decider: &dyn MemoryWriteDedupDecider,
    ) -> Json<MemoryNoteResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(MemoryNoteResponse::error(e)),
        };

        let tags_json = p
            .tags
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|| "[]".to_string());

        let repo = note_repository(self, worktree_root);

        if let Some(response) =
            maybe_update_singleton_note(self, &repo, &project_id, &p, &tags_json).await
        {
            return Json(response);
        }

        if let Some(response) = maybe_apply_write_dedup(
            &repo,
            decider,
            PendingWriteDedup {
                project_path: &p.project,
                project_id: &project_id,
                title: &p.title,
                content: &p.content,
                note_type: &p.note_type,
                status: p.status.as_deref(),
                tags_json: &tags_json,
            },
        )
        .await
        {
            if let Some(note_id) = response.id.as_deref()
                && response.error.is_none()
            {
                schedule_summary_regeneration(self, note_id);
            }
            return Json(response);
        }

        Json(create_note(self, &repo, &project_id, &p, &tags_json).await)
    }

    /// Edit an existing note. Operations: "append" (add to end), "prepend" (add
    /// after frontmatter), "find_replace" (exact text replacement, requires
    /// find_text), "replace_section" (replace content under a markdown heading,
    /// requires section). Use append to build large notes incrementally after
    /// memory_write creates the initial note. When type is provided and differs
    /// from current type, the note is automatically moved to the correct folder
    /// for the new type.
    #[tool(
        description = "Edit an existing note. Operations: \"append\" (add to end), \"prepend\" (add after frontmatter), \"find_replace\" (exact text replacement, requires find_text), \"replace_section\" (replace content under a markdown heading, requires section). Use append to build large notes incrementally after memory_write creates the initial note. When type is provided and differs from current type, the note is automatically moved to the correct folder for the new type."
    )]
    pub async fn memory_edit(
        &self,
        Parameters(p): Parameters<EditParams>,
    ) -> Json<MemoryNoteResponse> {
        self.memory_edit_with_worktree(Parameters(p), None).await
    }

    pub async fn memory_edit_with_worktree(
        &self,
        Parameters(p): Parameters<EditParams>,
        worktree_root: Option<PathBuf>,
    ) -> Json<MemoryNoteResponse> {
        super::edit_ops::memory_edit_with_worktree(self, Parameters(p), worktree_root).await
    }

    /// Delete a note. Removes file and index entry.
    #[tool(description = "Delete a note. Removes file and index entry.")]
    pub async fn memory_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Json<MemoryDeleteResponse> {
        self.memory_delete_with_worktree(Parameters(p), None).await
    }

    pub(crate) async fn memory_delete_with_worktree(
        &self,
        Parameters(p): Parameters<DeleteParams>,
        worktree_root: Option<PathBuf>,
    ) -> Json<MemoryDeleteResponse> {
        super::delete_ops::memory_delete_with_worktree(self, Parameters(p), worktree_root).await
    }

    /// Move a note to a new location. Updates permalink and resolves inbound links.
    #[tool(
        description = "Move a note to a new location. Updates permalink and resolves inbound links. Use type=\"proposed_adr\" to recover a mis-routed ADR draft into .djinn/decisions/proposed/ without raw shell mkdir/cp."
    )]
    pub async fn memory_move(
        &self,
        Parameters(p): Parameters<MoveParams>,
    ) -> Json<MemoryNoteResponse> {
        super::move_ops::memory_move(self, Parameters(p)).await
    }
}
