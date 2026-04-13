use djinn_db::NoteSearchParams;
use djinn_db::{NoteRepository, ProjectRepository};

use crate::server::DjinnMcpServer;

use super::{
    BrokenLinksParams, BuildContextParams, HealthParams, ListParams, MemoryBrokenLinksResponse,
    MemoryBuildContextResponse, MemoryHealthResponse, MemoryListResponse, MemoryNoteResponse,
    MemoryOrphansResponse, MemorySearchResponse, MemorySearchResultItem, OrphansParams, ReadParams,
    SearchParams, note_to_view,
};

fn normalize_folder_filter(folder: Option<String>) -> Option<String> {
    folder.filter(|value| !value.is_empty())
}

pub async fn resolve_project_id(server: &DjinnMcpServer, project: &str) -> Result<String, String> {
    let repo = ProjectRepository::new(server.state.db().clone(), server.state.event_bus());
    repo.resolve(project)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("project not found: {project}"))
}

pub async fn memory_read(server: &DjinnMcpServer, p: ReadParams) -> MemoryNoteResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => return MemoryNoteResponse::error(error),
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
    let note = match repo.get_by_permalink(&project_id, &p.identifier).await {
        Ok(Some(note)) => note,
        _ => match repo
            .search(NoteSearchParams {
                project_id: &project_id,
                query: &p.identifier,
                task_id: None,
                folder: None,
                note_type: None,
                limit: 1,
                semantic_scores: None,
            })
            .await
        {
            Ok(results) if !results.is_empty() => {
                match repo.get(&results[0].id).await.ok().flatten() {
                    Some(note) => note,
                    None => {
                        return MemoryNoteResponse::error(format!(
                            "note not found: {}",
                            p.identifier
                        ));
                    }
                }
            }
            _ => return MemoryNoteResponse::error(format!("note not found: {}", p.identifier)),
        },
    };

    let _ = repo.touch_accessed(&note.id).await;
    if note.abstract_.is_none() || note.overview.is_none() {
        server.enqueue_missing_summary_backfill(&note.id).await;
    }
    server.record_memory_read(&note.id).await;

    MemoryNoteResponse::from_note(&note)
}

pub async fn memory_search(
    server: &DjinnMcpServer,
    p: SearchParams,
    task_id: Option<&str>,
) -> MemorySearchResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemorySearchResponse {
                results: vec![],
                error: Some(error),
            };
        }
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
    let limit = p.limit.unwrap_or(10).clamp(1, 100) as usize;
    let semantic_scores = match server.state.embed_memory_query(&p.query).await {
        Ok(Some(embedding)) => repo
            .semantic_candidate_scores(
                &project_id,
                &embedding.values,
                p.folder.as_deref(),
                p.note_type.as_deref(),
                limit,
            )
            .await
            .ok(),
        Ok(None) | Err(_) => None,
    };

    match repo
        .search(NoteSearchParams {
            project_id: &project_id,
            query: &p.query,
            task_id,
            folder: p.folder.as_deref(),
            note_type: p.note_type.as_deref(),
            limit,
            semantic_scores,
        })
        .await
    {
        Ok(results) => MemorySearchResponse {
            results: results
                .into_iter()
                .map(|r| MemorySearchResultItem {
                    id: r.id,
                    permalink: r.permalink,
                    title: r.title,
                    folder: r.folder,
                    note_type: r.note_type,
                    snippet: r.snippet,
                    score: r.score,
                })
                .collect(),
            error: None,
        },
        Err(e) => MemorySearchResponse {
            results: vec![],
            error: Some(format!("search failed: {e}")),
        },
    }
}

pub async fn memory_list(server: &DjinnMcpServer, p: ListParams) -> MemoryListResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryListResponse {
                notes: vec![],
                error: Some(error),
            };
        }
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
    let depth = p.depth.unwrap_or(1);
    let notes = repo
        .list_compact(
            &project_id,
            p.folder.as_deref(),
            p.note_type.as_deref(),
            depth,
        )
        .await
        .unwrap_or_default();

    MemoryListResponse { notes, error: None }
}

pub async fn memory_build_context(
    server: &DjinnMcpServer,
    p: BuildContextParams,
    task_id: Option<&str>,
) -> MemoryBuildContextResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryBuildContextResponse {
                primary: vec![],
                related_l1: vec![],
                related_l0: vec![],
                error: Some(error),
            };
        }
    };

    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
    let max_related = p.max_related.unwrap_or(10).clamp(1, 50) as usize;
    let budget = p.budget.map(|b| b as usize);
    let url = p.url.strip_prefix("memory://").unwrap_or(&p.url);

    if url.ends_with("/*") {
        let folder = url.trim_end_matches("/*");
        let folder_filter = if folder.is_empty() {
            None
        } else {
            Some(folder)
        };
        let all = repo
            .list(&project_id, folder_filter)
            .await
            .unwrap_or_default();
        return MemoryBuildContextResponse {
            primary: all.into_iter().map(|n| note_to_view(&n)).collect(),
            related_l1: vec![],
            related_l0: vec![],
            error: None,
        };
    }

    match repo
        .build_context(
            &project_id,
            url,
            budget,
            task_id,
            max_related,
            p.min_confidence,
        )
        .await
    {
        Ok(response) => MemoryBuildContextResponse {
            primary: response.primary.iter().map(note_to_view).collect(),
            related_l1: response.related_l1,
            related_l0: response.related_l0,
            error: None,
        },
        Err(e) => MemoryBuildContextResponse {
            primary: vec![],
            related_l1: vec![],
            related_l0: vec![],
            error: Some(format!("build_context failed: {e}")),
        },
    }
}

pub async fn memory_health(server: &DjinnMcpServer, p: HealthParams) -> MemoryHealthResponse {
    let project = match &p.project {
        Some(path) => path.clone(),
        None => {
            return MemoryHealthResponse {
                total_notes: None,
                broken_link_count: None,
                orphan_note_count: None,
                stale_notes_by_folder: None,
                error: Some("project parameter required".to_string()),
            };
        }
    };
    let project_id = match resolve_project_id(server, &project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryHealthResponse {
                total_notes: None,
                broken_link_count: None,
                orphan_note_count: None,
                stale_notes_by_folder: None,
                error: Some(error),
            };
        }
    };
    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
    match repo.health(&project_id).await {
        Ok(h) => MemoryHealthResponse {
            total_notes: Some(h.total_notes),
            broken_link_count: Some(h.broken_link_count),
            orphan_note_count: Some(h.orphan_note_count),
            stale_notes_by_folder: Some(h.stale_notes_by_folder),
            error: None,
        },
        Err(e) => MemoryHealthResponse {
            total_notes: None,
            broken_link_count: None,
            orphan_note_count: None,
            stale_notes_by_folder: None,
            error: Some(e.to_string()),
        },
    }
}

pub async fn memory_broken_links(
    server: &DjinnMcpServer,
    p: BrokenLinksParams,
) -> MemoryBrokenLinksResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryBrokenLinksResponse {
                broken_links: vec![],
                error: Some(error),
            };
        }
    };
    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
    let folder = normalize_folder_filter(p.folder);
    let broken_links = repo
        .broken_links(&project_id, folder.as_deref())
        .await
        .unwrap_or_default();
    MemoryBrokenLinksResponse {
        broken_links,
        error: None,
    }
}

pub async fn memory_orphans(server: &DjinnMcpServer, p: OrphansParams) -> MemoryOrphansResponse {
    let project_id = match resolve_project_id(server, &p.project).await {
        Ok(id) => id,
        Err(error) => {
            return MemoryOrphansResponse {
                orphans: vec![],
                error: Some(error),
            };
        }
    };
    let repo = NoteRepository::new(server.state.db().clone(), server.state.event_bus());
    let folder = normalize_folder_filter(p.folder);
    let orphans = repo
        .orphans(&project_id, folder.as_deref())
        .await
        .unwrap_or_default();
    MemoryOrphansResponse {
        orphans,
        error: None,
    }
}
