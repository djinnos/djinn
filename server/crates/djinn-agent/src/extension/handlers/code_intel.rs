use super::*;
use crate::extension::github_search;
use djinn_control_plane::bridge::{ProjectCtx, RepoGraphOps, ResolveOutcome};

/// PR C2 mirror of the MCP-side dispatcher's pre-resolve. When the chat
/// tool's caller passes a short identifier (`User`, `helper`) we go
/// through the bridge's `resolve` op so we can return a structured JSON
/// payload describing the ambiguity / hard miss instead of failing the
/// tool call.
///
/// On `Found(uid)`, mutate `params.key` (or `from`/`to`) so the
/// downstream op sees the canonical RepoNodeKey. Return `Ok(None)` to
/// continue dispatch as usual.
async fn pre_resolve_chat_key(
    graph: &dyn RepoGraphOps,
    ctx: &ProjectCtx,
    params: &mut CodeGraphParams,
) -> Result<Option<serde_json::Value>, String> {
    let single_key_ops = ["neighbors", "impact", "implementations", "describe", "context"];
    if single_key_ops.contains(&params.operation.as_str()) {
        if let Some(key) = params.key.as_deref().filter(|k| !k.is_empty()) {
            let kind_hint = params.kind_hint.as_deref();
            match graph.resolve(ctx, key, kind_hint).await? {
                ResolveOutcome::Found(uid) => {
                    params.key = Some(uid);
                }
                ResolveOutcome::Ambiguous(candidates) => {
                    return Ok(Some(serde_json::json!({ "candidates": candidates })));
                }
                ResolveOutcome::NotFound => {
                    return Ok(Some(serde_json::json!({
                        "not_found": {
                            "query": key,
                            "kind_hint": kind_hint,
                        }
                    })));
                }
            }
        }
    }

    if params.operation == "path" {
        // Validate required args BEFORE the resolve loop so a missing
        // `to` (or `from`) returns the user-facing arg-validation error,
        // not whatever the bridge stub happened to say. The dispatch
        // arm at `match params.operation` would also catch this — but
        // by the time we get there, `graph.resolve` has already been
        // called for whichever field IS present, propagating any
        // bridge error and masking the real problem.
        if params.from.as_deref().filter(|s| !s.is_empty()).is_none() {
            return Err("'from' is required for 'path'".to_string());
        }
        if params.to.as_deref().filter(|s| !s.is_empty()).is_none() {
            return Err("'to' is required for 'path'".to_string());
        }
        for which in ["from", "to"] {
            // After the validation above both are guaranteed Some/non-empty.
            let key = match which {
                "from" => params.from.as_deref().expect("validated above"),
                _ => params.to.as_deref().expect("validated above"),
            };
            let kind_hint = params.kind_hint.as_deref();
            match graph.resolve(ctx, key, kind_hint).await? {
                ResolveOutcome::Found(uid) => {
                    if which == "from" {
                        params.from = Some(uid);
                    } else {
                        params.to = Some(uid);
                    }
                }
                ResolveOutcome::Ambiguous(candidates) => {
                    return Ok(Some(serde_json::json!({ "candidates": candidates })));
                }
                ResolveOutcome::NotFound => {
                    return Ok(Some(serde_json::json!({
                        "not_found": {
                            "query": key,
                            "kind_hint": kind_hint,
                        }
                    })));
                }
            }
        }
    }

    Ok(None)
}

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
    let mut p: CodeGraphParams = parse_args(arguments)?;
    let mcp_state = state.to_mcp_state();
    let graph_ops = mcp_state.repo_graph();
    // Build the resolved ProjectCtx once; pass by reference to each op.
    // We ignore any caller-supplied `project_path` in `p` — the task's
    // resolved project_id + its canonical clone path are authoritative.
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: project_id.to_string(),
        clone_path: project_path.to_string(),
    };

    // PR C2: pre-resolve key-bearing ops so the chat tool surfaces
    // `Ambiguous` / `NotFound` as structured JSON the model can act on,
    // instead of failing the call with a generic "not found" string.
    if let Some(short_circuit) =
        pre_resolve_chat_key(graph_ops.as_ref(), &ctx, &mut p).await?
    {
        return Ok(short_circuit);
    }

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
                    p.kind_filter.as_deref(),
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
            // v8: lowered default depth from 3 to 2. At depth=3, FileReference
            // compounds through hub files (cmd/deps.go, cmd/dispatcher.go in
            // a typical Go service; mod.rs / lib.rs in Rust) and the impact
            // set effectively becomes "the whole runtime". Depth 2 still
            // catches the dependency-of-a-dependency that matters for "what
            // breaks if I change this", without the third hop's noise. Power
            // users can still pass `limit: 3+` explicitly.
            let depth = p.limit.unwrap_or(2);
            // PR A2: validate `min_confidence` in `[0, 1]` before forwarding
            // so chat-tool callers get a clear error instead of silent zero
            // results.
            if let Some(c) = p.min_confidence
                && !(0.0..=1.0).contains(&c)
            {
                return Err(format!(
                    "invalid min_confidence {c}: must be in [0.0, 1.0]"
                ));
            }
            let impact = graph_ops
                .impact(
                    &ctx,
                    key,
                    depth,
                    p.group_by.as_deref(),
                    p.min_confidence,
                )
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
            // PR B4: dispatch on `mode`. The default lives in
            // `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE` (env var), which
            // ships as `"name"` until the hybrid soak window closes.
            let mode = match p.mode.as_deref() {
                Some(value) => value.to_string(),
                None => std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE")
                    .unwrap_or_else(|_| "name".to_string()),
            };
            let hits = match mode.as_str() {
                "name" => {
                    graph_ops
                        .search(&ctx, query, p.kind_filter.as_deref(), limit)
                        .await?
                }
                "hybrid" => {
                    graph_ops
                        .hybrid_search(&ctx, query, p.kind_filter.as_deref(), limit)
                        .await?
                }
                other => {
                    return Err(format!(
                        "invalid search mode '{other}': expected 'name' or 'hybrid'"
                    ));
                }
            };
            // v8: when hybrid returns nothing, wrap with a diagnostic
            // payload explaining WHY (semantic index unavailable, etc.)
            // instead of an opaque `[]`. Empty `name` results don't get
            // wrapped because the failure mode is just "no name match"
            // — clients understand that. The UI's `unwrapList(value,
            // 'hits')` handles both the array shape and the wrapped
            // `{ hits: [...] }` shape, so this is non-breaking.
            if hits.is_empty() && mode == "hybrid" {
                serde_json::json!({
                    "hits": [],
                    "diagnostic": hybrid_search_diagnostic(query),
                })
            } else {
                serde_json::to_value(&hits).map_err(|e| format!("serialize error: {e}"))?
            }
        }
        "cycles" => {
            let min_size = p.min_size.unwrap_or(2);
            // v8: default kind_filter to "symbol" when unspecified.
            // The raw graph always contains tautological file↔symbol
            // 2-cycles (every symbol forms one with its containing file
            // via ContainsDefinition + DeclaredInFile), which drown
            // out real dependency cycles. Power users can pass
            // kind_filter="file" for file-level import cycles, or
            // kind_filter=null explicitly via the underlying bridge
            // for the mixed view.
            let kind_filter = p.kind_filter.as_deref().or(Some("symbol"));
            let cycles = graph_ops
                .cycles(&ctx, kind_filter, min_size)
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
        "context" => {
            // PR C1: 360° symbol view. Default include_content=false to
            // keep wire size bounded — chat callers rarely need the body
            // on the first hop.
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'context'")?;
            let include_content = p.include_content.unwrap_or(false);
            match graph_ops.context(&ctx, key, include_content).await? {
                Some(symbol_context) => {
                    // Wrap in the same `symbol_context` discriminator the
                    // MCP dispatcher emits so downstream parsers stay
                    // consistent.
                    serde_json::json!({ "symbol_context": symbol_context })
                }
                None => serde_json::json!({
                    "not_found": { "query": key, "kind_hint": p.kind_hint }
                }),
            }
        }
        "capabilities" => {
            // v8: introspection. Lets clients plan workflows without
            // trial-and-error against a deployment whose set of wired
            // ops, env-gated features, and indexed languages they
            // can't otherwise discover. Cheap — no graph load.
            code_graph_capabilities()
        }
        other => {
            return Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'cycles', 'orphans', 'path', 'edges', \
                 'describe', 'context', 'capabilities'"
            ));
        }
    };
    Ok(result)
}

/// v8: explain why a hybrid search returned no hits. Hybrid fans out
/// to lexical (Dolt LIKE), semantic (Qdrant vector cosine), and
/// structural (canonical-graph name index) signals — when all three
/// are empty, the user otherwise sees `[]` with no signal about
/// whether the codebase is mis-indexed, the query is just unmatched,
/// or a backend (typically Qdrant) is unreachable.
///
/// We can't easily distinguish the failure modes from this layer
/// without re-running the signals, so the diagnostic is a structured
/// hint rather than a definitive cause. Surfaces:
/// - the resolved query string
/// - the hybrid-mode fan-out the search uses
/// - the most common reasons each signal returns nothing
/// - actionable next steps
fn hybrid_search_diagnostic(query: &str) -> serde_json::Value {
    serde_json::json!({
        "reason": "no hits across lexical + semantic + structural signals",
        "query": query,
        "fan_out": ["lexical (LIKE on code_chunks)", "semantic (Qdrant cosine)", "structural (canonical-graph name index)"],
        "common_causes": [
            "semantic index not built — code_chunk_embeddings warm pass hasn't run for this project",
            "Qdrant unreachable or empty for this project",
            "embedding service degraded — query embedding failed",
            "canonical graph not warmed for this project (call code_graph status to check)",
            "query genuinely has no matches",
        ],
        "next_steps": [
            "fall back to mode=name with the same query",
            "check code_graph status for warmed=true",
            "broaden the query (single keyword instead of natural language)",
        ],
    })
}

/// v8 capability-introspection payload. Returns JSON describing what
/// THIS binary actually supports — distinct from what the tool schema
/// might advertise. Cheap (no DB / graph load); safe to call from any
/// agent at any time.
///
/// Fields:
/// - `operations`: list of `operation` strings the dispatcher accepts.
/// - `default_search_mode`: the `mode` that bare `search` calls use.
/// - `available_search_modes`: every `mode` value the dispatcher routes.
/// - `env_features`: env-flag-controlled passes and their on/off state.
/// - `access_classifier_languages`: tree-sitter languages the read/write
///   classifier (v8 PR) can resolve when SCIP roles are absent.
/// - `repo_graph_artifact_version`: bincode schema stamp; mismatches
///   force a re-warm.
fn code_graph_capabilities() -> serde_json::Value {
    // env-flag readers — kept inline so this crate doesn't take a
    // dep on djinn-graph just for capability introspection.
    fn env_on(var: &str, default: bool) -> bool {
        match std::env::var(var) {
            Ok(v) => !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            ),
            Err(_) => default,
        }
    }
    fn env_opt_in(var: &str) -> bool {
        matches!(
            std::env::var(var).ok().as_deref().map(str::trim).map(str::to_ascii_lowercase),
            Some(ref v) if matches!(v.as_str(), "1" | "true" | "on" | "yes")
        )
    }

    serde_json::json!({
        "operations": [
            "neighbors", "ranked", "impact", "implementations",
            "search", "cycles", "orphans", "path", "edges",
            "describe", "context", "capabilities",
        ],
        "default_search_mode": std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE")
            .unwrap_or_else(|_| "name".to_string()),
        "available_search_modes": ["name", "hybrid"],
        "env_features": {
            // Defaults match the on-by-default behavior in djinn-graph.
            "entry_point_detection": env_on("DJINN_ENTRY_POINT_DETECTION", true),
            "process_detection": env_on("DJINN_PROCESS_DETECTION", true),
            "community_detection": env_on("DJINN_COMMUNITY_DETECTION", true),
            // Opt-in by design.
            "db_access_detection": env_opt_in("DJINN_DB_ACCESS_DETECTION"),
        },
        "access_classifier_languages": ["rust", "go", "python", "typescript", "javascript"],
        "repo_graph_artifact_version": 8,
        "filter_tiers": {
            "tier_1_module_artifacts": "always-on; SCIP module-tree synthetic nodes (`crate/`, `…/MODULE.`)",
            "tier_1_5_generated_and_mocks": "always-on; *.pb.go, *.gen.*, *_mock.go, mock_*.go, **/__mocks__/**, *.snap",
            "tier_2_project_globs": "from project config: graph_excluded_paths + graph_orphan_ignore",
        },
        "default_filters": {
            "ranked_excludes_externals": true,
            "neighbors_excludes_externals": true,
            "implementations_excludes_externals": true,
            "context_excludes_externals": true,
            "snapshot_excludes_externals": true,
            "impact_excludes_externals": true,
            "impact_default_max_depth": 2,
            "impact_default_min_confidence": 0.85,
            "impact_behavioral_edge_whitelist": [
                "Reads", "Writes", "SymbolReference", "FileReference",
                "Implements", "Extends", "TypeDefines", "Defines"
            ],
            "cycles_default_kind_filter": "symbol",
            "ranked_default_sort_by": "fused"
        },
    })
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
