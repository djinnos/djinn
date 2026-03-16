use super::*;

#[tool_router(router = memory_writes_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Create or update a note. Type is required and determines storage folder.
    /// Singleton types (brief, roadmap) write a fixed file — one per project.
    #[tool(
        description = "Create or update a note. Type is required and determines storage folder (adr->decisions/, pattern->patterns/, research->research/, requirement->requirements/, reference->reference/, design->design/, persona->design/personas, journey->design/journeys, design_spec->design/specs, session->research/sessions, competitive->research/competitive, tech_spike->research/technical). Singleton types (brief, roadmap) write a fixed file at docs root — one per project, title is ignored. Use [[wikilinks]] in content to connect notes — any [[Note Title]] creates a link in the knowledge graph. Add a '## Relations' section at the bottom with '- [[Related Note]]' entries to make connections explicit. For large documents (>150 lines): create with initial content, then use memory_edit with operation=\"append\" to add remaining sections."
    )]
    pub async fn memory_write(
        &self,
        Parameters(p): Parameters<WriteParams>,
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

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());

        // For singletons, upsert: try create, fall back to update if already exists.
        use crate::db::is_singleton;
        if is_singleton(&p.note_type)
            && let Some(existing) = repo
                .get_by_permalink(&project_id, &p.note_type)
                .await
                .ok()
                .flatten()
        {
            match repo
                .update(&existing.id, &p.title, &p.content, &tags_json)
                .await
            {
                Ok(note) => return Json(MemoryNoteResponse::from_note(&note)),
                Err(e) => {
                    return Json(MemoryNoteResponse::error(e.to_string()));
                }
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
            Ok(note) => Json(MemoryNoteResponse::from_note(&note)),
            Err(e) => Json(MemoryNoteResponse::error(e.to_string())),
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
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryNoteResponse::error(format!(
                "project not found: {}",
                p.project
            )));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());

        // Resolve note by permalink or title.
        let note = match resolve_note_by_identifier(&repo, &project_id, &p.identifier).await {
            Some(n) => n,
            None => {
                return Json(MemoryNoteResponse::error(format!(
                    "note not found: {}",
                    p.identifier
                )));
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
                    Err(e) => {
                        return Json(MemoryNoteResponse::error(e.to_string()));
                    }
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
            Err(e) => return Json(MemoryNoteResponse::error(e)),
        };

        match repo
            .update(&note.id, &note.title, &new_content, &note.tags)
            .await
        {
            Ok(updated) => Json(MemoryNoteResponse::from_note(&updated)),
            Err(e) => Json(MemoryNoteResponse::error(e.to_string())),
        }
    }

    /// Delete a note. Removes file and index entry.
    #[tool(description = "Delete a note. Removes file and index entry.")]
    pub async fn memory_delete(
        &self,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Json<MemoryDeleteResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryDeleteResponse {
                ok: false,
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
            return Json(MemoryDeleteResponse {
                ok: false,
                error: Some(format!("note not found: {}", p.identifier)),
            });
        };

        match repo.delete(&note.id).await {
            Ok(()) => Json(MemoryDeleteResponse {
                ok: true,
                error: None,
            }),
            Err(e) => Json(MemoryDeleteResponse {
                ok: false,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Move a note to a new location. Updates permalink and resolves inbound links.
    #[tool(
        description = "Move a note to a new location. Updates permalink and resolves inbound links."
    )]
    pub async fn memory_move(
        &self,
        Parameters(p): Parameters<MoveParams>,
    ) -> Json<MemoryNoteResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryNoteResponse::error(format!(
                "project not found: {}",
                p.project
            )));
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
            return Json(MemoryNoteResponse::error(format!(
                "note not found: {}",
                p.identifier
            )));
        };

        let new_title = p.title.as_deref().unwrap_or(&note.title);
        let moved_title = p.title.as_deref().unwrap_or(&note.title);

        match repo
            .move_note(&note.id, Path::new(&p.project), moved_title, &p.note_type)
            .await
        {
            Ok(mut moved) => {
                if p.title.is_some() {
                    moved.title = new_title.to_string();
                }
                Json(MemoryNoteResponse::from_note(&moved))
            }
            Err(e) => Json(MemoryNoteResponse::error(e.to_string())),
        }
    }
}
