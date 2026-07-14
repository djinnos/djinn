use super::lifecycle::schedule_summary_regeneration;
use super::ops::RetrievalObserver;
use super::write_dedup::{
    LlmMemoryWriteDedupDecider, WriteDedupOutcome, apply_created_note_supersede,
    maybe_apply_write_dedup,
};
use super::write_dedup_types::{MemoryWriteDedupDecider, PendingWriteDedup};
use super::write_services::{create_note, maybe_update_singleton_note, note_repository};
use super::{
    DeleteParams, EditParams, MemoryDeleteResponse, MemoryNoteResponse, MoveParams, WriteParams,
    revision_context,
};
use djinn_telemetry::memory_retrieval::RetrievalOutcome;

use crate::server::DjinnMcpServer;
use rmcp::{Json, handler::server::wrapper::Parameters, tool, tool_router};

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
        self.memory_write_with_decider(
            Parameters(p),
            &LlmMemoryWriteDedupDecider::new(
                self.state.db().clone(),
                djinn_core::auth_context::current_user_id(),
            ),
        )
        .await
    }

    pub(crate) async fn memory_write_with_decider(
        &self,
        Parameters(p): Parameters<WriteParams>,
        decider: &dyn MemoryWriteDedupDecider,
    ) -> Json<MemoryNoteResponse> {
        let (reason, attribution, provenance) = match revision_context(&p.reason) {
            Ok(context) => context,
            Err(error) => return Json(MemoryNoteResponse::error(error)),
        };
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(MemoryNoteResponse::error(e)),
        };

        let tags_json = p
            .tags
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|| "[]".to_string());

        let repo = note_repository(self);

        if let Some(response) = maybe_update_singleton_note(
            self,
            &repo,
            &project_id,
            &p,
            &tags_json,
            reason.clone(),
            attribution.clone(),
            provenance.clone(),
        )
        .await
        {
            return Json(response);
        }

        let observer = RetrievalObserver::new(
            self,
            djinn_telemetry::memory_retrieval::RetrievalEntryPoint::JitPitfalls,
        );

        match maybe_apply_write_dedup(
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
            WriteDedupOutcome::Respond(response) => {
                let response = *response;
                let outcome = if response.error.is_some() {
                    RetrievalOutcome::Error
                } else if response.id.is_some() {
                    RetrievalOutcome::Success
                } else {
                    RetrievalOutcome::Empty
                };
                observer.finish(outcome, response.id.is_some() as u64);
                if let Some(note_id) = response.id.as_deref()
                    && response.error.is_none()
                {
                    schedule_summary_regeneration(self, note_id);
                }
                Json(response)
            }
            WriteDedupOutcome::CreateNew => {
                observer.finish(RetrievalOutcome::Empty, 0);
                Json(
                    create_note(
                        self,
                        &repo,
                        &project_id,
                        &p,
                        &tags_json,
                        reason.clone(),
                        attribution.clone(),
                        provenance.clone(),
                    )
                    .await,
                )
            }
            WriteDedupOutcome::SupersedeExisting {
                candidate_id,
                reason: dedup_reason,
            } => {
                observer.finish(RetrievalOutcome::Success, 1);
                // Keep incoming creation exactly on the ordinary memory_write path.
                let response = create_note(
                    self,
                    &repo,
                    &project_id,
                    &p,
                    &tags_json,
                    reason.clone(),
                    attribution.clone(),
                    provenance.clone(),
                )
                .await;
                if let Some(new_note_id) = response.id.as_deref()
                    && response.error.is_none()
                    && let Err(error) = apply_created_note_supersede(
                        &repo,
                        &project_id,
                        new_note_id,
                        &candidate_id,
                        &dedup_reason,
                    )
                    .await
                {
                    // The new note has already been durably created. Keep its normal
                    // public creation response while making incomplete association
                    // work observable for operators.
                    tracing::warn!(
                        decision_kind = "supersede_existing",
                        new_note_id,
                        candidate_id,
                        error,
                        "memory_write supersede mutation failed after creation"
                    );
                }
                Json(response)
            }
        }
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
        super::edit_ops::memory_edit(self, Parameters(p)).await
    }

    /// Delete a note. Removes the row from Dolt.
    #[tool(description = "Delete a note. Removes the row from Dolt.")]
    pub async fn memory_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Json<MemoryDeleteResponse> {
        super::delete_ops::memory_delete(self, Parameters(p)).await
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
