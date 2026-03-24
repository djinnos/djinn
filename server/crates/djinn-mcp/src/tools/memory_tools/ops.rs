use djinn_db::{NoteRepository, ProjectRepository};

use crate::server::DjinnMcpServer;

use super::{
    BuildContextParams, ListParams, MemoryBuildContextResponse, MemoryListResponse,
    MemoryNoteResponse, MemorySearchResponse, MemorySearchResultItem, ReadParams, SearchParams,
    note_to_view,
};

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
            .search(&project_id, &p.identifier, None, None, None, 1)
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

    match repo
        .search(
            &project_id,
            &p.query,
            task_id,
            p.folder.as_deref(),
            p.note_type.as_deref(),
            limit,
        )
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
        let all = repo
            .list(&project_id, Some(folder))
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
        .build_context(&project_id, url, budget, task_id, max_related)
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
