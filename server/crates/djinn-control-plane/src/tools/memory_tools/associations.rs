use super::*;

use std::collections::HashSet;

use djinn_db::{MemoryEntityKind, MemoryEntityRef, MemoryEntityType, ProposalRepository};
use djinn_memory::Note;

/// The entity that the seed `identifier` resolved to.
enum ResolvedEntity {
    Note(Note),
    Proposal(djinn_core::models::Proposal),
}

#[tool_router(router = memory_associations_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// List associations for a note or proposal, sorted by weight descending.
    ///
    /// Accepts a note identifier (permalink/ID) or a proposal identifier
    /// (short_id/ID). Returns connected notes and/or proposals with their
    /// weight, kind, and stable identity metadata (entity_type, entity_id,
    /// entity_title, entity_permalink). Returns an empty array for entities
    /// that have no associations.
    #[tool(
        description = "List associations for a note or proposal, sorted by weight descending. Returns connected notes and/or proposals with weight, count, kind (e.g. co_access, builds_on, contradicts, supersedes, exemplifies, derived_from), and entity metadata. Accepts a note identifier (permalink/ID) or proposal identifier (short_id/ID). Returns [] for entities with no associations."
    )]
    pub async fn memory_associations(
        &self,
        Parameters(p): Parameters<AssociationsParams>,
    ) -> Json<MemoryAssociationsResponse> {
        let Some(project_id) = self.project_id_for_path(&p.project).await else {
            return Json(MemoryAssociationsResponse {
                seed_entity_type: "note".to_string(),
                associations: vec![],
                error: Some(format!("project not found: {}", p.project)),
            });
        };

        let repo = NoteRepository::new(self.state.db().clone(), self.state.event_bus());
        let proposal_repo =
            ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        // Resolve the seed identifier to either a note or a proposal. Notes
        // take priority (backward compatibility: callers that used to pass a
        // note permalink must keep resolving to the note, not to a proposal
        // that happens to share an ID prefix).
        let seed =
            resolve_entity_identifier(&repo, &proposal_repo, &project_id, &p.identifier).await;

        let Some(seed) = seed else {
            return Json(MemoryAssociationsResponse {
                seed_entity_type: "note".to_string(),
                associations: vec![],
                error: Some(format!("entity not found: {}", p.identifier)),
            });
        };

        let min_weight = p.min_weight.unwrap_or(0.0).clamp(0.0, 1.0);
        let limit = p.limit.unwrap_or(20).clamp(0, 1000);

        match &seed {
            ResolvedEntity::Note(note) => {
                associations_for_note_seed(&repo, &proposal_repo, note, min_weight, limit).await
            }
            ResolvedEntity::Proposal(proposal) => {
                associations_for_proposal_seed(&repo, &proposal_repo, proposal, min_weight, limit)
                    .await
            }
        }
    }
}

/// Resolve an opaque identifier to a note (preferred) or proposal.
///
/// Notes are resolved first via the existing `resolve_note_by_identifier`
/// helper (permalink → title fallback). If that yields nothing, the
/// identifier is tried against the proposal repository by ID or short_id.
async fn resolve_entity_identifier(
    repo: &NoteRepository,
    proposal_repo: &ProposalRepository,
    project_id: &str,
    identifier: &str,
) -> Option<ResolvedEntity> {
    if let Some(note) = resolve_note_by_identifier(repo, project_id, identifier).await {
        return Some(ResolvedEntity::Note(note));
    }

    if let Ok(Some(proposal)) = proposal_repo.resolve(identifier).await {
        return Some(ResolvedEntity::Proposal(proposal));
    }

    None
}

/// Build the association response for a note seed.
///
/// Combines legacy note↔note associations (from `note_associations`, which
/// carry Hebbian `co_access` and F5 typed edges) with heterogeneous typed
/// entity associations (from `memory_entity_associations`, which carry
/// note↔proposal and proposal↔proposal typed edges). The legacy list is the
/// backward-compatible base; the heterogeneous list adds any proposal-typed
/// neighbors discovered through the new substrate.
async fn associations_for_note_seed(
    repo: &NoteRepository,
    proposal_repo: &ProposalRepository,
    note: &Note,
    min_weight: f64,
    limit: i64,
) -> Json<MemoryAssociationsResponse> {
    // Legacy note↔note associations (co_access + F5 typed edges).
    let legacy = repo
        .list_associations_for_note(&note.id, min_weight, limit)
        .await;

    let legacy_entries = match legacy {
        Ok(entries) => entries,
        Err(e) => {
            return Json(MemoryAssociationsResponse {
                seed_entity_type: "note".to_string(),
                associations: vec![],
                error: Some(format!("failed to fetch associations: {e}")),
            });
        }
    };

    let mut associations: Vec<MemoryAssociationEntry> = legacy_entries
        .into_iter()
        .map(|e| {
            note_association_entry(
                &e.note_permalink,
                &e.note_title,
                e.weight,
                &e.kind,
                e.co_access_count,
                &e.last_co_access,
            )
        })
        .collect();

    // Track the set of note permalinks already present (for dedup when we
    // fold in the heterogeneous list).
    let mut seen_note_permalinks: HashSet<String> = associations
        .iter()
        .map(|a| a.entity_permalink.clone())
        .collect();

    // Heterogeneous typed associations (note↔proposal, note↔note typed edges
    // that live on the new substrate). These complement the legacy list; a
    // note may have both legacy co_access edges and new typed edges to
    // proposals.
    if let Ok(typed) = repo
        .list_typed_entity_associations_for(MemoryEntityRef::note(&note.id), min_weight, limit)
        .await
    {
        for edge in typed {
            // Determine the "other" endpoint of the edge relative to the
            // seed note.
            let other = if edge.source.id == note.id {
                edge.target.clone()
            } else {
                edge.source.clone()
            };

            match other.entity_type {
                MemoryEntityType::Note => {
                    // Skip note↔note typed edges already covered by the
                    // legacy substrate to avoid duplicates.
                    if let Ok(Some(title_note)) = repo.get(&other.id).await {
                        if seen_note_permalinks.contains(&title_note.permalink) {
                            continue;
                        }
                        seen_note_permalinks.insert(title_note.permalink.clone());
                        associations.push(note_association_entry(
                            &title_note.permalink,
                            &title_note.title,
                            edge.weight,
                            edge.kind.as_str(),
                            0,
                            "",
                        ));
                    }
                }
                MemoryEntityType::Proposal => {
                    if let Some(entry) =
                        proposal_association_entry(proposal_repo, &other.id, edge.weight, edge.kind)
                            .await
                    {
                        associations.push(entry);
                    }
                }
            }
        }
    }

    // Sort by weight descending (stable for equal weights) and apply limit.
    associations.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let limit_usize = if limit <= 0 {
        usize::MAX
    } else {
        limit as usize
    };
    associations.truncate(limit_usize);

    Json(MemoryAssociationsResponse {
        seed_entity_type: "note".to_string(),
        associations,
        error: None,
    })
}

/// Build the association response for a proposal seed.
///
/// Proposals only participate in the heterogeneous typed substrate
/// (`memory_entity_associations`), so this path queries only that table.
async fn associations_for_proposal_seed(
    repo: &NoteRepository,
    proposal_repo: &ProposalRepository,
    proposal: &djinn_core::models::Proposal,
    min_weight: f64,
    limit: i64,
) -> Json<MemoryAssociationsResponse> {
    let typed = repo
        .list_typed_entity_associations_for(
            MemoryEntityRef::proposal(&proposal.id),
            min_weight,
            limit,
        )
        .await;

    let edges = match typed {
        Ok(edges) => edges,
        Err(e) => {
            return Json(MemoryAssociationsResponse {
                seed_entity_type: "proposal".to_string(),
                associations: vec![],
                error: Some(format!("failed to fetch associations: {e}")),
            });
        }
    };

    let mut associations = Vec::new();
    for edge in edges {
        let other = if edge.source.id == proposal.id {
            edge.target.clone()
        } else {
            edge.source.clone()
        };

        match other.entity_type {
            MemoryEntityType::Note => {
                if let Ok(Some(title_note)) = repo.get(&other.id).await {
                    associations.push(note_association_entry(
                        &title_note.permalink,
                        &title_note.title,
                        edge.weight,
                        edge.kind.as_str(),
                        0,
                        "",
                    ));
                }
            }
            MemoryEntityType::Proposal => {
                if let Some(entry) =
                    proposal_association_entry(proposal_repo, &other.id, edge.weight, edge.kind)
                        .await
                {
                    associations.push(entry);
                }
            }
        }
    }

    associations.sort_by(|a, b| {
        b.weight
            .partial_cmp(&a.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let limit_usize = if limit <= 0 {
        usize::MAX
    } else {
        limit as usize
    };
    associations.truncate(limit_usize);

    Json(MemoryAssociationsResponse {
        seed_entity_type: "proposal".to_string(),
        associations,
        error: None,
    })
}

/// Construct a `MemoryAssociationEntry` for a note target, populating both
/// the new entity fields and the legacy note-only fields for backward
/// compatibility.
fn note_association_entry(
    permalink: &str,
    title: &str,
    weight: f64,
    kind: &str,
    co_access_count: i64,
    last_co_access: &str,
) -> MemoryAssociationEntry {
    MemoryAssociationEntry {
        entity_type: "note".to_string(),
        entity_id: String::new(),
        entity_title: title.to_string(),
        entity_permalink: permalink.to_string(),
        note_permalink: permalink.to_string(),
        note_title: title.to_string(),
        weight,
        kind: kind.to_string(),
        co_access_count,
        last_co_access: last_co_access.to_string(),
    }
}

/// Construct a `MemoryAssociationEntry` for a proposal target by resolving
/// its title and short_id from the proposals table. Returns None if the
/// proposal can't be loaded.
async fn proposal_association_entry(
    proposal_repo: &ProposalRepository,
    proposal_id: &str,
    weight: f64,
    kind: MemoryEntityKind,
) -> Option<MemoryAssociationEntry> {
    let proposal = proposal_repo.get(proposal_id).await.ok().flatten()?;
    Some(MemoryAssociationEntry {
        entity_type: "proposal".to_string(),
        entity_id: proposal.id,
        entity_title: proposal.title,
        entity_permalink: proposal.short_id,
        note_permalink: String::new(),
        note_title: String::new(),
        weight,
        kind: kind.as_str().to_string(),
        co_access_count: 0,
        last_co_access: String::new(),
    })
}
