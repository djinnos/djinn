use super::*;
use crate::extension::github_search;

pub(crate) async fn call_lsp(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: LspParams = parse_args(arguments)?;
    validate_symbol_only_params(p.operation.as_str(), &p)?;
    let path = resolve_path(&p.file_path, worktree_path);

    match p.operation.as_str() {
        "hover" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state.lsp.hover_symbol(worktree_path, &path, symbol).await?
                }
                (None, Some(line), Some(character)) => {
                    // LSP uses 0-based positions; accept 1-based from agents
                    state
                        .lsp
                        .hover(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "hover accepts either symbol or line+character, but not both".to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "hover requires both line and character when symbol is omitted".to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("hover requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "hover", "result": result }))
        }
        "definition" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state
                        .lsp
                        .go_to_definition_symbol(worktree_path, &path, symbol)
                        .await?
                }
                (None, Some(line), Some(character)) => {
                    state
                        .lsp
                        .go_to_definition(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "definition accepts either symbol or line+character, but not both"
                            .to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "definition requires both line and character when symbol is omitted"
                            .to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("definition requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "definition", "result": result }))
        }
        "references" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state
                        .lsp
                        .find_references_symbol(worktree_path, &path, symbol)
                        .await?
                }
                (None, Some(line), Some(character)) => {
                    state
                        .lsp
                        .find_references(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "references accepts either symbol or line+character, but not both"
                            .to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "references requires both line and character when symbol is omitted"
                            .to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("references requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "references", "result": result }))
        }
        "symbols" => {
            let query = SymbolQuery {
                depth: p.depth,
                kinds: p
                    .kind
                    .as_deref()
                    .map(parse_symbol_kind_filter)
                    .transpose()?,
                name_filter: p.name_filter,
            };
            let result = state
                .lsp
                .document_symbols(worktree_path, &path, query)
                .await?;
            Ok(serde_json::json!({ "operation": "symbols", "result": result }))
        }
        other => Err(format!(
            "unknown LSP operation: {other}. Use: hover, definition, references, or symbols"
        )),
    }
}

pub(crate) async fn call_code_graph(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: &str,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: CodeGraphParams = parse_args(arguments)?;
    let mcp_state = state.to_mcp_state();
    let graph_ops = mcp_state.repo_graph();
    // Build the resolved ProjectCtx once; pass by reference to each op.
    // We ignore any caller-supplied `project_path` in `p` — the task's
    // resolved project_id + its canonical clone path are authoritative.
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: project_id.to_string(),
        clone_path: project_path.to_string(),
    };

    let result: serde_json::Value = match p.operation.as_str() {
        "neighbors" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'neighbors'")?;
            let neighbors = graph_ops
                .neighbors(
                    &ctx,
                    key,
                    p.direction.as_deref(),
                    p.group_by.as_deref(),
                )
                .await?;
            serde_json::to_value(&neighbors).map_err(|e| format!("serialize error: {e}"))?
        }
        "ranked" => {
            let limit = p.limit.unwrap_or(20);
            let ranked = graph_ops
                .ranked(
                    &ctx,
                    p.kind_filter.as_deref(),
                    p.sort_by.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&ranked).map_err(|e| format!("serialize error: {e}"))?
        }
        "implementations" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'implementations'")?;
            let impls = graph_ops.implementations(&ctx, key).await?;
            serde_json::to_value(&impls).map_err(|e| format!("serialize error: {e}"))?
        }
        "impact" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'impact'")?;
            let depth = p.limit.unwrap_or(3);
            let impact = graph_ops
                .impact(&ctx, key, depth, p.group_by.as_deref())
                .await?;
            serde_json::to_value(&impact).map_err(|e| format!("serialize error: {e}"))?
        }
        "search" => {
            let query = p
                .query
                .as_deref()
                .filter(|q| !q.is_empty())
                .ok_or("'query' is required for 'search'")?;
            let limit = p.limit.unwrap_or(20);
            let hits = graph_ops
                .search(&ctx, query, p.kind_filter.as_deref(), limit)
                .await?;
            serde_json::to_value(&hits).map_err(|e| format!("serialize error: {e}"))?
        }
        "cycles" => {
            let min_size = p.min_size.unwrap_or(2);
            let cycles = graph_ops
                .cycles(&ctx, p.kind_filter.as_deref(), min_size)
                .await?;
            serde_json::to_value(&cycles).map_err(|e| format!("serialize error: {e}"))?
        }
        "orphans" => {
            let limit = p.limit.unwrap_or(50);
            let orphans = graph_ops
                .orphans(
                    &ctx,
                    p.kind_filter.as_deref(),
                    p.visibility.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&orphans).map_err(|e| format!("serialize error: {e}"))?
        }
        "path" => {
            let from = p
                .from
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'from' is required for 'path'")?;
            let to =
                p.to.as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or("'to' is required for 'path'")?;
            let path = graph_ops
                .path(&ctx, from, to, p.max_depth)
                .await?;
            serde_json::to_value(&path).map_err(|e| format!("serialize error: {e}"))?
        }
        "edges" => {
            let from_glob = p
                .from_glob
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'from_glob' is required for 'edges'")?;
            let to_glob = p
                .to_glob
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'to_glob' is required for 'edges'")?;
            let limit = p.limit.unwrap_or(100);
            let edges = graph_ops
                .edges(
                    &ctx,
                    from_glob,
                    to_glob,
                    p.edge_kind.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&edges).map_err(|e| format!("serialize error: {e}"))?
        }
        "describe" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'describe'")?;
            let description = graph_ops.describe(&ctx, key).await?;
            serde_json::to_value(&description).map_err(|e| format!("serialize error: {e}"))?
        }
        other => {
            return Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'cycles', 'orphans', 'path', 'edges', 'describe'"
            ));
        }
    };
    Ok(result)
}

// ---------------------------------------------------------------------------
// github_search — search GitHub code via the GitHub Code Search API
// ---------------------------------------------------------------------------

pub(crate) async fn call_github_search(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let params: GithubSearchParams = parse_args(arguments)?;
    let installation_id = resolve_installation_id(state, project_id).await?;
    github_search::search(
        installation_id,
        &params.query,
        params.language.as_deref(),
        params.repo.as_deref(),
        params.path.as_deref(),
    )
    .await
}

// ---------------------------------------------------------------------------
// github_fetch_file — fetch a file from a GitHub repository
// ---------------------------------------------------------------------------

pub(crate) async fn call_github_fetch_file(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let params: GithubFetchFileParams = parse_args(arguments)?;
    let installation_id = resolve_installation_id(state, project_id).await?;
    github_search::fetch_file(
        installation_id,
        &params.repo,
        &params.path,
        params.git_ref.as_deref(),
        params.start_line,
        params.end_line,
    )
    .await
}

/// Resolve a GitHub App installation id for an agent-dispatched GitHub tool.
///
/// Worker sessions run outside the MCP request scope, so we cannot read the
/// session-user token-local. The project-scoped installation is the only
/// credential available; fail closed with an actionable error when the
/// project lacks one.
async fn resolve_installation_id(
    state: &AgentContext,
    project_id: Option<&str>,
) -> Result<u64, String> {
    let project_id = project_id.ok_or(
        "github_* tools require an active project context; dispatcher could not resolve project_id",
    )?;
    let project_repo = djinn_db::ProjectRepository::new(state.db.clone(), state.event_bus.clone());
    match project_repo.get_installation_id(project_id).await {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(format!(
            "project {project_id} has no GitHub App installation; \
             re-register the project via the GitHub App flow to enable background GitHub tools"
        )),
        Err(e) => Err(format!(
            "failed to read installation_id for project {project_id}: {e}"
        )),
    }
}
