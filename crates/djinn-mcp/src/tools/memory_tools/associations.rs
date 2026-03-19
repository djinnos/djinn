use super::*;

#[tool_router(router = memory_associations_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// List implicit associations for a note, sorted by weight descending.
    /// Returns the connected notes with their co-access weight and count.
    /// Returns an empty array for notes that have no associations.
    #[tool(
        description = "List implicit associations for a note, sorted by weight descending. Returns connected notes with co-access weight and count. Returns [] for notes with no associations."
    )]
    pub async fn memory_associations(
        &self,
        Parameters(p): Parameters<AssociationsParams>,
    ) -> Json<MemoryAssociationsResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryAssociationsResponse {
                associations: vec![],
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(note) = resolve_note_by_identifier(&repo, &project_id, &p.identifier).await else {
            return Json(MemoryAssociationsResponse {
                associations: vec![],
                error: Some(format!("note not found: {}", p.identifier)),
            });
        };

        let min_weight = p.min_weight.unwrap_or(0.0).clamp(0.0, 1.0);
        let limit = p.limit.unwrap_or(20).clamp(0, 1000);

        match repo
            .list_associations_for_note(&note.id, min_weight, limit)
            .await
        {
            Ok(entries) => {
                let associations = entries
                    .into_iter()
                    .map(|e| MemoryAssociationEntry {
                        note_permalink: e.note_permalink,
                        note_title: e.note_title,
                        weight: e.weight,
                        co_access_count: e.co_access_count,
                        last_co_access: e.last_co_access,
                    })
                    .collect();
                Json(MemoryAssociationsResponse {
                    associations,
                    error: None,
                })
            }
            Err(e) => Json(MemoryAssociationsResponse {
                associations: vec![],
                error: Some(format!("failed to fetch associations: {e}")),
            }),
        }
    }
}
