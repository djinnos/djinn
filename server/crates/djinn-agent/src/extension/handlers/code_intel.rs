// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use super::*;
use crate::extension::github_search;
// v10: canonical test-path classification, shared with the graph
// builder's `RepoGraphNode::is_test` stamping.
use djinn_control_plane::bridge::{ProjectCtx, RepoGraphOps, ResolveOutcome};
use djinn_core::test_paths::is_test_path;

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
    if !should_pre_resolve_chat_key(params) {
        return Ok(None);
    }

    let single_key_ops = [
        "neighbors",
        "impact",
        "implementations",
        "describe",
        "context",
    ];
    if single_key_ops.contains(&params.operation.as_str())
        && let Some(key) = params.key.as_deref().filter(|k| !k.is_empty())
    {
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

pub(in crate::extension) fn should_pre_resolve_chat_key(params: &CodeGraphParams) -> bool {
    // Workspace-aware traversal ops use `workspace` only while resolving the
    // seed/endpoint and then intentionally keep the graph walk cross-workspace.
    // The bridge methods for those ops own that seed-scoped resolution. If the
    // chat extension pre-resolves a short name here with no workspace argument,
    // it can turn a valid workspace-local seed into an ambiguous or wrong
    // global symbol before the backend sees the requested slug.
    !(params.workspace.is_some() && matches!(params.operation.as_str(), "impact" | "path"))
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
    p.normalize();
    p.normalize_resolver_inputs();
    let mcp_state = state.to_mcp_state();
    let graph_ops = mcp_state.repo_graph();
    // Build the resolved ProjectCtx once; pass by reference to each op.
    // We ignore any caller-supplied `project_path` in `p` — the task's
    // resolved project_id + its canonical clone path are authoritative.
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: project_id.to_string(),
        clone_path: project_path.to_string(),
        workspace: p.workspace.clone(),
        sub_path: None,
    };

    // wraw (jc47 mirror): capture the caller commit once so both the
    // pre-resolve short-circuit and the normal dispatch path can
    // attach the same `graph_staleness` object on success. Normalized
    // (blank → None) by `CodeGraphParams::normalize()` already.
    let caller_head = p.resolved_current_head();

    // PR C2: pre-resolve key-bearing ops so the chat tool surfaces
    // `Ambiguous` / `NotFound` as structured JSON the model can act on,
    // instead of failing the call with a generic "not found" string.
    if let Some(mut short_circuit) =
        pre_resolve_chat_key(graph_ops.as_ref(), &ctx, &mut p).await?
    {
        attach_chat_graph_staleness(graph_ops.as_ref(), &ctx, caller_head.as_deref(), &mut short_circuit).await;
        return Ok(short_circuit);
    }

    // Wrap the per-op dispatch in a tokio timeout + tracing span. The
    // chat handler does NOT impose its own per-tool timeout, so a slow
    // op (e.g. an unindexed coupling self-join hitting Dolt's planner
    // pathology) would otherwise wedge the chat stream forever — the
    // hang fix that motivated this whole change. Mirrors the MCP-side
    // timeout in `DjinnMcpServer::dispatch_code_graph`. Override with
    // `DJINN_CODE_GRAPH_DISPATCH_TIMEOUT_SECS`.
    let op = p.operation.clone();
    let project_id_for_log = ctx.id.clone();
    let started = std::time::Instant::now();
    let span = tracing::info_span!(
        "code_graph_chat",
        op = %op,
        project_id = %project_id_for_log,
    );
    let timeout = code_graph_chat_dispatch_timeout();
    let inner = call_code_graph_inner(state, &mut p, &ctx, graph_ops.as_ref());
    let mut result = {
        use tracing::Instrument;
        tokio::time::timeout(timeout, inner)
            .instrument(span)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "code_graph op '{op}' exceeded {}s — try a narrower call \
                     (lower limit, file_glob filter, since_days) or a different op",
                    timeout.as_secs()
                ))
            })
    };
    // wraw (jc47 mirror): on success, attach `graph_staleness` so the
    // chat caller can see whether the served graph blob matches its
    // current HEAD. Mirrors `attach_graph_staleness` on the
    // control-plane side without forcing the chat extension to round
    // trip through the typed `CodeGraphResponse` enum (the chat layer
    // always returns `serde_json::Value`).
    if let Ok(ref mut value) = result {
        attach_chat_graph_staleness(graph_ops.as_ref(), &ctx, caller_head.as_deref(), value).await;
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(_) => tracing::info!(
            target: "djinn_agent::extension::handlers::code_intel",
            op = %op,
            project_id = %project_id_for_log,
            elapsed_ms,
            status = "ok",
            "code_graph chat dispatch completed"
        ),
        Err(err) => tracing::warn!(
            target: "djinn_agent::extension::handlers::code_intel",
            op = %op,
            project_id = %project_id_for_log,
            elapsed_ms,
            status = "error",
            error = %err,
            "code_graph chat dispatch failed"
        ),
    }
    result
}

/// wraw: build and attach a `graph_staleness` object to a successful
/// chat-side `code_graph` response.
///
/// Mirrors the control-plane `attach_graph_staleness` helper — the
/// agent extension doesn't round-trip through the typed
/// `CodeGraphResponse` enum, so we read the same `RepoGraphOps::status`
/// peek (never warms) and write the same field shape directly into the
/// returned `serde_json::Value`. The field is added at the top level
/// alongside the op-specific wrapper (e.g. `{ "key": ..., "neighbors":
/// ..., "graph_staleness": {...} }`).
///
/// No-op when `caller_head` is `None` so existing callers that don't
/// track their current commit see the same response shape as before.
/// No-op on status-lookup failure so a transient error never blocks
/// the served graph result. This is a best-effort warning, never a
/// hard signal.
async fn attach_chat_graph_staleness(
    graph: &dyn RepoGraphOps,
    ctx: &ProjectCtx,
    caller_head: Option<&str>,
    response: &mut serde_json::Value,
) {
    let Some(caller_commit) = caller_head else {
        return;
    };
    let cached_commit = match graph.status(ctx).await {
        Ok(status) => status
            .pinned_commit
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "code_graph chat staleness: status lookup failed; omitting graph_staleness"
            );
            return;
        }
    };
    let trimmed_caller = caller_commit.trim();
    let staleness = match cached_commit.as_deref() {
        Some(cached) => {
            let is_stale = cached != trimmed_caller;
            serde_json::json!({
                "cached_commit": cached,
                "caller_commit": trimmed_caller,
                "is_stale": is_stale,
            })
        }
        None => {
            // Non-stale-safe: missing pinned commit means the graph
            // blob has no recorded commit (unwarmed or status lookup
            // returned an unindexed result). Per jc47's contract, a
            // missing cached commit never blocks the query; surface
            // `is_stale=false` so the agent knows the result is
            // ambiguous rather than known-stale.
            serde_json::json!({
                "caller_commit": trimmed_caller,
                "is_stale": false,
            })
        }
    };
    if let Some(object) = response.as_object_mut() {
        object.insert("graph_staleness".to_string(), staleness);
    }
}

/// Default per-op timeout for the chat-side `code_graph` dispatch.
/// Mirrors `DjinnMcpServer::dispatch_code_graph` — keep them aligned
/// so the same op behaves the same under either surface.
const CODE_GRAPH_CHAT_DISPATCH_TIMEOUT_DEFAULT_SECS: u64 = 60;

fn code_graph_chat_dispatch_timeout() -> std::time::Duration {
    let secs = std::env::var("DJINN_CODE_GRAPH_DISPATCH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(CODE_GRAPH_CHAT_DISPATCH_TIMEOUT_DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

struct AgentWorkspaceScope {
    workspace: Option<String>,
    hint: Option<Vec<String>>,
}

async fn resolve_agent_workspace_scope(
    graph_ops: &dyn RepoGraphOps,
    ctx: &djinn_control_plane::bridge::ProjectCtx,
) -> Result<AgentWorkspaceScope, String> {
    match graph_ops
        .workspace_hint(ctx, ctx.workspace.as_deref())
        .await?
    {
        Some(candidates) => Ok(AgentWorkspaceScope {
            workspace: None,
            hint: Some(candidates),
        }),
        None => Ok(AgentWorkspaceScope {
            workspace: ctx.workspace.clone(),
            hint: None,
        }),
    }
}

fn attach_workspace_hint(value: &mut serde_json::Value, hint: Option<Vec<String>>) {
    let Some(hint) = hint else {
        return;
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("workspace_hint".to_string(), serde_json::json!(hint));
    }
}

fn json_with_optional_workspace_hint<T: serde::Serialize>(
    field: &str,
    payload: T,
    hint: Option<Vec<String>>,
) -> Result<serde_json::Value, String> {
    match hint {
        Some(hint) => Ok(serde_json::json!({ field: payload, "workspace_hint": hint })),
        None => serde_json::to_value(payload).map_err(|e| format!("serialize error: {e}")),
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AgentPagination {
    pub(super) offset: usize,
    pub(super) limit: usize,
    pub(super) summary_only: bool,
    pub(super) by_depth_counts: bool,
}

impl AgentPagination {
    pub(super) fn resolve(p: &CodeGraphParams, default_limit: usize) -> Self {
        Self {
            offset: p.resolved_offset(),
            limit: p.resolved_page_limit(default_limit),
            summary_only: p.summary_only.unwrap_or(false),
            by_depth_counts: p.by_depth_counts.unwrap_or(false),
        }
    }

    pub(super) fn emit_metadata(self, total: usize) -> bool {
        self.offset > 0 || self.summary_only || total > self.limit
    }
}

pub(super) fn apply_agent_page_slice<T>(items: &mut Vec<T>, pagination: AgentPagination) -> bool {
    let total = items.len();
    if pagination.offset >= total {
        items.clear();
        return false;
    }
    if pagination.offset > 0 {
        items.drain(0..pagination.offset);
    }
    if items.len() > pagination.limit {
        items.truncate(pagination.limit);
        true
    } else {
        false
    }
}

pub(super) fn agent_by_depth_counts(
    entries: &[djinn_control_plane::bridge::ImpactEntry],
) -> std::collections::BTreeMap<String, usize> {
    let mut counts: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for entry in entries {
        *counts.entry(entry.depth).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(depth, count)| (depth.to_string(), count))
        .collect()
}

pub(crate) async fn call_code_graph_inner(
    state: &AgentContext,
    p: &mut CodeGraphParams,
    ctx: &djinn_control_plane::bridge::ProjectCtx,
    graph_ops: &dyn RepoGraphOps,
) -> Result<serde_json::Value, String> {
    let _ = state;
    let _ = (&p.edge_filters, p.token_budget, p.max_seeds, p.window_days);
    let result: serde_json::Value = match p.operation.as_str() {
        "neighbors" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'neighbors'")?;
            let neighbors = graph_ops
                .neighbors(
                    ctx,
                    key,
                    p.direction.as_deref(),
                    p.group_by.as_deref(),
                    p.kind_filter.as_deref(),
                )
                .await?;
            let default_cap = p.limit.unwrap_or(20);
            let pagination = AgentPagination::resolve(p, default_cap);
            match neighbors {
                djinn_control_plane::bridge::NeighborsResult::Detailed(mut neighbors) => {
                    let total = neighbors.len();
                    let has_more = apply_agent_page_slice(&mut neighbors, pagination);
                    let mut value = if pagination.summary_only {
                        serde_json::json!({ "key": key })
                    } else {
                        serde_json::json!({ "key": key, "neighbors": neighbors })
                    };
                    if pagination.summary_only {
                        value["summary_only"] = serde_json::json!(true);
                    }
                    if pagination.emit_metadata(total) {
                        value["total"] = serde_json::json!(total);
                        value["offset"] = serde_json::json!(pagination.offset);
                        value["limit"] = serde_json::json!(pagination.limit);
                        value["has_more"] = serde_json::json!(has_more);
                    }
                    value
                }
                djinn_control_plane::bridge::NeighborsResult::Grouped(mut file_groups) => {
                    let total = file_groups.len();
                    let has_more = apply_agent_page_slice(&mut file_groups, pagination);
                    let mut value = if pagination.summary_only {
                        serde_json::json!({ "key": key })
                    } else {
                        serde_json::json!({ "key": key, "file_groups": file_groups })
                    };
                    if pagination.summary_only {
                        value["summary_only"] = serde_json::json!(true);
                    }
                    if pagination.emit_metadata(total) {
                        value["total"] = serde_json::json!(total);
                        value["offset"] = serde_json::json!(pagination.offset);
                        value["limit"] = serde_json::json!(pagination.limit);
                        value["has_more"] = serde_json::json!(has_more);
                    }
                    value
                }
            }
        }
        "ranked" => {
            let limit = p.limit.unwrap_or(20);
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let ranked = graph_ops
                .ranked(
                    ctx,
                    scope.workspace.as_deref(),
                    p.kind_filter.as_deref(),
                    p.sort_by.as_deref(),
                    limit,
                )
                .await?;
            json_with_optional_workspace_hint("nodes", ranked, scope.hint)?
        }
        "implementations" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'implementations'")?;
            let impls = graph_ops.implementations(ctx, key).await?;
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
                return Err(format!("invalid min_confidence {c}: must be in [0.0, 1.0]"));
            }
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let impact = graph_ops
                .impact(
                    ctx,
                    scope.workspace.as_deref(),
                    key,
                    depth,
                    p.group_by.as_deref(),
                    p.min_confidence,
                )
                .await?;
            let pagination = AgentPagination::resolve(p, 100);
            let mut value = match impact {
                djinn_control_plane::bridge::ImpactResult::Detailed(mut impact) => {
                    let total = impact.len();
                    let by_depth_counts = if pagination.summary_only || pagination.by_depth_counts {
                        Some(agent_by_depth_counts(&impact))
                    } else {
                        None
                    };
                    let has_more = apply_agent_page_slice(&mut impact, pagination);
                    let mut value = if pagination.summary_only {
                        serde_json::json!({ "key": key })
                    } else {
                        serde_json::json!({ "key": key, "impact": impact })
                    };
                    if pagination.summary_only {
                        value["summary_only"] = serde_json::json!(true);
                    }
                    if let Some(by_depth_counts) = by_depth_counts {
                        value["by_depth_counts"] = serde_json::json!(by_depth_counts);
                    }
                    if pagination.emit_metadata(total) {
                        value["total"] = serde_json::json!(total);
                        value["offset"] = serde_json::json!(pagination.offset);
                        value["limit"] = serde_json::json!(pagination.limit);
                        value["has_more"] = serde_json::json!(has_more);
                    }
                    value
                }
                djinn_control_plane::bridge::ImpactResult::Grouped(mut file_groups) => {
                    let total = file_groups.len();
                    let has_more = apply_agent_page_slice(&mut file_groups, pagination);
                    let mut value = if pagination.summary_only {
                        serde_json::json!({ "key": key })
                    } else {
                        serde_json::json!({ "key": key, "file_groups": file_groups })
                    };
                    if pagination.summary_only {
                        value["summary_only"] = serde_json::json!(true);
                    }
                    if pagination.emit_metadata(total) {
                        value["total"] = serde_json::json!(total);
                        value["offset"] = serde_json::json!(pagination.offset);
                        value["limit"] = serde_json::json!(pagination.limit);
                        value["has_more"] = serde_json::json!(has_more);
                    }
                    value
                }
            };
            attach_workspace_hint(&mut value, scope.hint);
            value
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
                        .search(ctx, query, p.kind_filter.as_deref(), limit)
                        .await?
                }
                "hybrid" => {
                    graph_ops
                        .hybrid_search(ctx, query, p.kind_filter.as_deref(), limit)
                        .await?
                }
                other => {
                    return Err(format!(
                        "invalid search mode '{other}': expected 'name' or 'hybrid'"
                    ));
                }
            };
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            // v8: when hybrid returns nothing, wrap with a diagnostic
            // payload explaining WHY (semantic index unavailable, etc.)
            // instead of an opaque `[]`. Empty `name` results don't get
            // wrapped because the failure mode is just "no name match"
            // — clients understand that. The UI's `unwrapList(value,
            // 'hits')` handles both the array shape and the wrapped
            // `{ hits: [...] }` shape, so this is non-breaking.
            if hits.is_empty() && mode == "hybrid" {
                let mut value = serde_json::json!({
                    "hits": [],
                    "diagnostic": hybrid_search_diagnostic(query),
                });
                attach_workspace_hint(&mut value, scope.hint);
                value
            } else {
                json_with_optional_workspace_hint("hits", hits, scope.hint)?
            }
        }
        "query_subgraph" => {
            let query = p
                .query
                .as_deref()
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .ok_or("'query' is required for operation 'query_subgraph'")?;

            let token_budget =
                bounded_optional_usize(p.token_budget, "token_budget", 1_024, 32_000, false)?;
            let max_seeds = bounded_optional_usize(p.max_seeds, "max_seeds", 1, 32, false)?;
            let max_depth = p.max_depth.map(|d| d.clamp(0, 8));

            let edge_filter = p
                .edge_filters
                .clone()
                .unwrap_or_else(|| p.edge_kind.clone().into_iter().collect())
                .into_iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();

            let file_filter = p
                .file_filter
                .as_deref()
                .or(p.file_glob.as_deref())
                .or(p.file_path.as_deref())
                .or(p.from_glob.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let request = djinn_control_plane::bridge::QuerySubgraphRequest {
                query: query.to_string(),
                workspace: ctx
                    .workspace
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                context_filter: p
                    .context_filter
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                file_filter,
                kind_filter: p
                    .kind_filter
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                edge_filter,
                token_budget,
                max_depth,
                max_seeds,
            };
            let result = graph_ops.query_subgraph(ctx, request).await?;
            serde_json::json!({ "query_subgraph": result })
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
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let cycles = graph_ops.cycles(ctx, kind_filter, min_size).await?;
            json_with_optional_workspace_hint("cycles", cycles, scope.hint)?
        }
        "orphans" => {
            let limit = p.limit.unwrap_or(50);
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let orphans = graph_ops
                .orphans(
                    ctx,
                    scope.workspace.as_deref(),
                    p.kind_filter.as_deref(),
                    p.visibility.as_deref(),
                    limit,
                )
                .await?;
            json_with_optional_workspace_hint("orphans", orphans, scope.hint)?
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
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let path = graph_ops
                .path(ctx, scope.workspace.as_deref(), from, to, p.max_depth)
                .await?;
            let mut value = serde_json::json!({ "path": path });
            attach_workspace_hint(&mut value, scope.hint);
            value
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
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let edges = graph_ops
                .edges(ctx, from_glob, to_glob, p.edge_kind.as_deref(), limit)
                .await?;
            json_with_optional_workspace_hint("edges", edges, scope.hint)?
        }
        "describe" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'describe'")?;
            let description = graph_ops.describe(ctx, key).await?;
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
            match graph_ops.context(ctx, key, include_content).await? {
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
        "status" => {
            // v8: peek at the persisted canonical-graph cache. Cheap;
            // never warms.
            let result = graph_ops.status(ctx).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "snapshot" => {
            // v8: full graph snapshot for the /code-graph UI.
            // Default cap 2000 (Sigma WebGL ceiling); trait clamps
            // higher values.
            let node_cap = p.node_cap.or(p.limit).unwrap_or(2000);
            let exclusions = djinn_control_plane::tools::graph_exclusions::GraphExclusions::empty();
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let result = graph_ops
                .snapshot(
                    ctx,
                    scope.workspace.as_deref(),
                    djinn_control_plane::bridge::SnapshotLevel::parse(p.level.as_deref())?,
                    node_cap,
                    &exclusions,
                )
                .await?;
            let mut value = serde_json::json!({ "snapshot": result });
            attach_workspace_hint(&mut value, scope.hint);
            value
        }
        "workspaces" => {
            let result = graph_ops.workspaces(ctx).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "symbols_at" => {
            // v8: file/line → enclosing symbols.
            // Accepts EITHER the new dedicated `file_path` + `start_line`
            // fields (preferred, matches the schema doc) OR the
            // legacy `key` + `min_size` overload (kept so existing
            // callers don't break).
            let file_owned: String = p
                .file_path
                .clone()
                .or_else(|| {
                    p.key
                        .as_deref()
                        .map(|k| k.trim_start_matches("file:").to_string())
                })
                .filter(|f| !f.is_empty())
                .ok_or("'file_path' (or legacy 'key') is required for 'symbols_at'")?;
            let start_line = p
                .start_line
                .or_else(|| p.min_size.map(|n| n as u32))
                .ok_or("'start_line' (or legacy 'min_size') is required for 'symbols_at'")?;
            let result = graph_ops
                .symbols_at(ctx, &file_owned, start_line, p.end_line)
                .await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "diff_touches" => {
            // v8: changed line ranges → touched symbols. Caller
            // supplies `changed_ranges: [{file_path, start_line,
            // end_line}, ...]` parsed from `git diff --unified=0
            // base..head`.
            use djinn_control_plane::bridge::ChangedRange;
            let agents_ranges = p
                .changed_ranges
                .as_ref()
                .ok_or("'changed_ranges' is required for 'diff_touches'")?;
            if agents_ranges.is_empty() {
                return Err("'changed_ranges' must be a non-empty array".to_string());
            }
            let bridge_ranges: Vec<ChangedRange> = agents_ranges
                .iter()
                .map(|r| ChangedRange {
                    file: r.file.clone(),
                    start_line: r.start_line as i64,
                    end_line: r.end_line.map(|n| n as i64),
                })
                .collect();
            let result = graph_ops.diff_touches(ctx, &bridge_ranges).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "detect_changes" => {
            // v8: SHA range or explicit changed_files → touched
            // symbols + PageRank tier. Either {from_sha, to_sha}
            // (server shells out to git) or {changed_files} (caller
            // pre-resolved).
            let from_sha = p.from_sha.as_deref();
            let to_sha = p.to_sha.as_deref();
            let changed: Vec<String> = p.changed_files.clone().unwrap_or_default();
            if from_sha.is_none() && changed.is_empty() {
                return Err(
                    "detect_changes requires either {from_sha, to_sha} or non-empty \
                     changed_files"
                        .to_string(),
                );
            }
            let result = graph_ops
                .detect_changes(ctx, from_sha, to_sha, &changed)
                .await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "api_surface" => {
            // v8: list public symbols with fan-in / fan-out + a
            // used-outside-crate signal. Trait method already exists;
            // this is just dispatch wiring.
            let limit = p.limit.unwrap_or(50);
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let module_glob = p.module_glob.as_deref().or(p.from_glob.as_deref());
            let result = graph_ops
                .api_surface(
                    ctx,
                    scope.workspace.as_deref(),
                    module_glob,
                    p.visibility.as_deref(),
                    limit,
                )
                .await?;
            json_with_optional_workspace_hint("symbols", result, scope.hint)?
        }
        "metrics_at" => {
            // v8: scalar graph snapshot — node/edge counts, cycles,
            // god-object floor, orphan count, public-API and
            // documentation coverage. Cheap enough to call any time;
            // no graph load if cached.
            let result = graph_ops.metrics_at(ctx).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "dead_symbols" => {
            // v8: stricter sibling of `orphans`. Tiered by caller-
            // confidence (`high`/`med`/`low`); high = no incoming
            // refs from any entry-point reachable scope.
            //
            // Accepts the dedicated `confidence` field (preferred);
            // falls back to `kind_filter` for callers using the
            // pre-iter-21 contract.
            let confidence = p
                .confidence
                .as_deref()
                .or(p.kind_filter.as_deref())
                .unwrap_or("high");
            let limit = p.limit.unwrap_or(50);
            let result = graph_ops.dead_symbols(ctx, confidence, limit).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "deprecated_callers" => {
            // v8: surface symbols whose signature/documentation
            // carries `#[deprecated]` / `@deprecated`, plus their
            // callers — actionable removal target list.
            let limit = p.limit.unwrap_or(50);
            let result = graph_ops.deprecated_callers(ctx, limit).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "touches_hot_path" => {
            // v8: given entries (e.g. `[fn main]`) + sinks (e.g. db
            // writes) + queried symbols, returns which queried
            // symbols sit on any shortest path entry → sink. Useful
            // for "does my refactor touch the hot request path?"
            let entries: Vec<String> = p
                .from_glob
                .as_deref()
                .map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let sinks: Vec<String> = p
                .to_glob
                .as_deref()
                .map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let queried: Vec<String> = p
                .query
                .as_deref()
                .map(|s| {
                    s.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if entries.is_empty() || sinks.is_empty() || queried.is_empty() {
                return Err("touches_hot_path requires from_glob (entries, comma-sep), \
                     to_glob (sinks, comma-sep), and query (symbols, comma-sep)"
                    .to_string());
            }
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;
            let result = graph_ops
                .touches_hot_path(ctx, scope.workspace.as_deref(), &entries, &sinks, &queried)
                .await?;
            json_with_optional_workspace_hint("hits", result, scope.hint)?
        }
        "coupling_hubs" => {
            // v8: top files by cumulative coupling across all
            // partners (sum of co_edits). High value = touching this
            // file is more likely to require touching many others.
            // Accepts dedicated `since_days` (preferred); falls back
            // to parsing `query` as int.
            let limit = p.limit.unwrap_or(20);
            let since_days = p
                .since_days
                .or_else(|| p.query.as_deref().and_then(|s| s.parse::<u32>().ok()));
            let result = graph_ops.coupling_hubs(ctx, limit, since_days, 15).await?;
            serde_json::to_value(&result).map_err(|e| format!("serialize error: {e}"))?
        }
        "capabilities" => {
            // v8: introspection. Lets clients plan workflows without
            // trial-and-error against a deployment whose set of wired
            // ops, env-gated features, and indexed languages they
            // can't otherwise discover. Cheap — no graph load.
            code_graph_capabilities()
        }
        "cochange" => {
            // v8: files that change together. Routes through the
            // existing RepoGraphOps::coupling / coupling_hotspots
            // methods so the agent dispatch returns the same rich
            // shape the MCP server already exposes (with
            // supporting_commit_samples per coupled file).
            // - With `key`: top files co-edited with that one file.
            // - Without `key`: project-wide top coupled pairs.
            let limit = p.limit.unwrap_or(20);
            if let Some(key) = p.key.as_deref().filter(|k| !k.is_empty()) {
                let file_path = key.trim_start_matches("file:");
                let coupled = graph_ops.coupling(ctx, file_path, limit).await?;
                serde_json::json!({
                    "target": file_path,
                    "coupled": coupled,
                })
            } else {
                let pagination = AgentPagination::resolve(p, limit);
                // Guard pageLimit=0/unset coercions so the bridge never sees a zero fetch cap.
                let fetch_base = pagination.limit.max(1);
                let fetch_limit = fetch_base.saturating_mul(25).clamp(fetch_base, 500);
                let mut pairs = graph_ops
                    .coupling_hotspots(ctx, fetch_limit, None, 15)
                    .await?;
                let total = pairs.len();
                let has_more = apply_agent_page_slice(&mut pairs, pagination);
                if pagination.summary_only {
                    pairs.clear();
                }
                let mut value = serde_json::json!({ "pairs": pairs });
                if pagination.summary_only {
                    value["summary_only"] = serde_json::json!(true);
                }
                if pagination.emit_metadata(total) {
                    value["total"] = serde_json::json!(total);
                    value["offset"] = serde_json::json!(pagination.offset);
                    value["limit"] = serde_json::json!(pagination.limit);
                    value["has_more"] = serde_json::json!(has_more);
                }
                value
            }
        }
        "churn" => {
            // v8: top files by distinct-commit count.
            // Accepts the dedicated `since_days` field (preferred);
            // falls back to parsing `query` as an integer for the
            // pre-iter-21 contract.
            let limit = p.limit.unwrap_or(20);
            let since_days = p
                .since_days
                .or_else(|| p.query.as_deref().and_then(|s| s.parse::<u32>().ok()));
            let rows = graph_ops.churn(ctx, limit, since_days).await?;
            serde_json::json!({
                "since_days": since_days,
                "files": rows,
            })
        }
        "hotspots" => {
            // v8: churn × centrality, via the trait's existing
            // hotspots method. Returns HotspotEntry with `top_symbols`
            // (highest-pagerank symbol display names per file).
            // Accepts dedicated `since_days` + `file_glob` (preferred);
            // falls back to parsing `query` as int.
            let limit = p.limit.unwrap_or(20);
            let window_days = p
                .since_days
                .or_else(|| p.query.as_deref().and_then(|s| s.parse::<u32>().ok()))
                .unwrap_or(90);
            let hotspots = graph_ops
                .hotspots(ctx, window_days, p.file_glob.as_deref(), limit)
                .await?;
            serde_json::json!({
                "window_days": window_days,
                "file_glob": p.file_glob,
                "hotspots": hotspots,
                "scoring": "composite_score = churn × centrality (sum of pagerank over file's symbols)",
                "next_steps": [
                    "review top hotspots for refactoring candidates",
                    "high churn + high centrality = highest blast radius if it breaks",
                ],
            })
        }
        "boundary_check" => {
            // v8: enforce architectural layering rules. Routes through
            // `RepoGraphOps::boundary_check` (single graph walk over
            // all rules, vs. one per rule the iter-11 implementation
            // did). The trait's BoundaryRule is {from_glob, to_glob}
            // — we explode each user-supplied rule's `forbid_to` list
            // into multiple BoundaryRule entries + track a mapping
            // back to the original rule for output grouping.
            use djinn_control_plane::bridge::BoundaryRule as TraitRule;
            let rules = p.rules.as_deref().unwrap_or(&[]);
            if rules.is_empty() {
                return Err(
                    "'rules' is required for 'boundary_check': pass [{name, from_glob, \
                     forbid_to: [...]}]"
                        .to_string(),
                );
            }
            // Build the flat trait-rule list + the index → user-rule
            // mapping so we can regroup violations afterwards.
            let mut flat: Vec<TraitRule> = Vec::new();
            // Maps trait-rule index → (user-rule index, matched forbid_to glob).
            let mut origin: Vec<(usize, String)> = Vec::new();
            for (rule_i, rule) in rules.iter().enumerate() {
                for forbid in &rule.forbid_to {
                    flat.push(TraitRule {
                        from_glob: rule.from_glob.clone(),
                        to_glob: forbid.clone(),
                    });
                    origin.push((rule_i, forbid.clone()));
                }
            }
            let violations = graph_ops.boundary_check(ctx, &flat).await?;
            // Regroup violations by original user rule.
            const PER_RULE_LIMIT: usize = 100;
            let mut by_user_rule: Vec<(usize, bool, Vec<serde_json::Value>)> =
                rules.iter().map(|_| (0, false, Vec::new())).collect();
            for v in &violations {
                let (rule_i, ref forbid_glob) = origin[v.rule_index];
                let entry = &mut by_user_rule[rule_i];
                entry.0 += 1; // total count
                if entry.2.len() >= PER_RULE_LIMIT {
                    entry.1 = true; // truncated
                    continue;
                }
                entry.2.push(serde_json::json!({
                    "from": v.from_key,
                    "to": v.to_key,
                    "matched_forbid_glob": forbid_glob,
                    "edge_kind": v.edge_kind,
                    "from_file": v.from_file,
                    "to_file": v.to_file,
                }));
            }
            let mut total_violations: usize = 0;
            let report_rules: Vec<serde_json::Value> = rules
                .iter()
                .zip(by_user_rule.iter())
                .map(|(rule, (count, truncated, vs))| {
                    total_violations += count;
                    serde_json::json!({
                        "name": rule.name,
                        "from_glob": rule.from_glob,
                        "forbid_to": rule.forbid_to,
                        "violation_count": count,
                        "violations": vs,
                        "truncated": truncated,
                        "passed": *count == 0,
                    })
                })
                .collect();
            serde_json::json!({
                "rules_evaluated": rules.len(),
                "total_violations": total_violations,
                "passed": total_violations == 0,
                "rules": report_rules,
                "next_steps": if total_violations == 0 {
                    serde_json::json!(["all rules passed — no architectural violations detected"])
                } else {
                    serde_json::json!([
                        "for each violation, decide: refactor the dependency, or relax the rule",
                        "wire `boundary_check` into CI to fail on regressions",
                    ])
                },
            })
        }
        "complexity" => {
            // Iter 28: rank functions or files by complexity metric
            // (cognitive / cyclomatic / nloc / max_nesting / param_count).
            // Reuses the trait's `RepoGraphOps::complexity` method so the
            // agent dispatch returns the same shape the MCP server emits.
            let target = p.target.as_deref().unwrap_or("functions");
            let sort_by = p.sort_by.as_deref().unwrap_or("cognitive");
            let limit = p.limit.unwrap_or(30);
            let result = graph_ops
                .complexity(ctx, target, sort_by, p.file_glob.as_deref(), limit)
                .await?;
            // The bridge return is an untagged enum — serde just emits
            // either a `[FunctionComplexityEntry]` array or a
            // `[FileComplexityEntry]` array. Wrap in a `complexity` key
            // for the same reason the MCP-side response does (avoid
            // collision with the surrounding agent JSON shape).
            serde_json::json!({
                "target": target,
                "sort_by": sort_by,
                "complexity": result,
            })
        }
        "refactor_candidates" => {
            // Iter 29: composite refactor-priority ranking — fuses
            // cognitive × churn × pagerank into a single z-score and
            // returns the top function-level targets.
            let limit = p.limit.unwrap_or(30);
            let candidates = graph_ops
                .refactor_candidates(ctx, p.since_days, p.file_glob.as_deref(), limit)
                .await?;
            serde_json::json!({
                "since_days": p.since_days,
                "file_glob": p.file_glob,
                "refactor_candidates": candidates,
            })
        }
        "blast_radius" => {
            // v8: first-class "what breaks if I change this" op.
            // Bundles `neighbors(incoming, group_by=file)` for direct
            // dependents and `impact(group_by=file)` for transitive,
            // then categorises each file path into runtime / test /
            // e2e buckets. Defaults: depth=2 (matches the impact
            // default; depth-3 compounds through hub files into the
            // whole runtime), no kind/edge filters.
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'blast_radius'")?;
            let depth = p.max_depth.unwrap_or(2);
            let scope = resolve_agent_workspace_scope(graph_ops, ctx).await?;

            let (direct_result, transitive_result) = tokio::join!(
                graph_ops.neighbors(ctx, key, Some("incoming"), Some("file"), None),
                graph_ops.impact(
                    ctx,
                    scope.workspace.as_deref(),
                    key,
                    depth,
                    Some("file"),
                    None
                ),
            );
            let direct_groups = match direct_result? {
                djinn_control_plane::bridge::NeighborsResult::Grouped(g) => g,
                djinn_control_plane::bridge::NeighborsResult::Detailed(_) => {
                    // Unreachable: we passed group_by=file. Defensive
                    // fallback so a contract change doesn't panic.
                    Vec::new()
                }
            };
            let transitive_groups = match transitive_result? {
                djinn_control_plane::bridge::ImpactResult::Grouped(g) => g,
                djinn_control_plane::bridge::ImpactResult::Detailed(_) => Vec::new(),
            };

            // Hide the queried target itself from the transitive set —
            // depth=0 is the source node and shouldn't show up as its
            // own dependent. Also hide it from direct (defensive).
            let target_key_norm = key.trim_start_matches("file:").to_string();
            let direct_filtered: Vec<_> = direct_groups
                .into_iter()
                .filter(|g| g.file != target_key_norm)
                .collect();
            // Subtract direct dependents from transitive so the second
            // section is genuinely "deeper than depth-1".
            let direct_files: std::collections::HashSet<String> =
                direct_filtered.iter().map(|g| g.file.clone()).collect();
            let transitive_filtered: Vec<_> = transitive_groups
                .into_iter()
                .filter(|g| g.file != target_key_norm && !direct_files.contains(&g.file))
                .collect();

            let mut value = serde_json::json!({
                "target": key,
                "direct": categorize_blast_groups(direct_filtered),
                "transitive": categorize_blast_groups(transitive_filtered),
                "depth": depth,
                "next_steps": [
                    "run the tests listed in `direct.tests` and `direct.e2e_tests`",
                    "review `direct.runtime` for behavioural compatibility",
                    "treat `transitive.runtime` as a deeper-review hint, not a hard breakage list",
                ],
            });
            attach_workspace_hint(&mut value, scope.hint);
            value
        }
        other => {
            return Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'query_subgraph', 'cycles', 'orphans', 'path', 'edges', \
                 'describe', 'context', 'capabilities', 'blast_radius', \
                 'boundary_check', 'cochange', 'churn', 'hotspots', \
                 'complexity', 'refactor_candidates', 'api_surface', 'metrics_at', 'dead_symbols', \
                 'deprecated_callers', 'touches_hot_path', 'coupling_hubs', \
                 'status', 'snapshot', 'workspaces', 'symbols_at', 'diff_touches', \
                 'detect_changes'"
            ));
        }
    };
    Ok(result)
}

fn bounded_optional_usize(
    value: Option<i64>,
    name: &str,
    min: usize,
    max: usize,
    allow_zero: bool,
) -> Result<Option<usize>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value < 0 || (value == 0 && !allow_zero) {
        return Err(format!(
            "invalid {name} {value}: expected positive integer{}",
            if allow_zero { " or 0" } else { "" }
        ));
    }
    let value = value as usize;
    if value == 0 && allow_zero {
        return Ok(Some(0));
    }
    Ok(Some(value.clamp(min, max)))
}

/// v8: classify a [`djinn_control_plane::bridge::FileGroupEntry`] list
/// into runtime / test / e2e buckets for the `blast_radius` op.
///
/// Heuristics (file-path conventional, language-aware):
/// - **e2e_test**: file is a test (see below) AND path contains
///   `/e2e/`, `/integration/`, `/system/`, or matches `tests/e2e/**` /
///   `tests/integration/**`. Run e2e separately because they're slow.
/// - **test**: basename matches `*_test.{go,rs,py,kt,scala}` OR
///   `*.test.{ts,tsx,js,jsx}` OR `*_spec.{rb,ts,tsx,js,jsx}` OR path
///   contains `/tests/` segment OR `/test/` segment OR Rust
///   `#[cfg(test)] mod tests` symbols (already filtered to file
///   `tests.rs` here). Run before merge.
/// - **runtime**: everything else. Behavioural-compatibility review
///   target.
///
/// Returns a JSON object with three keys (`runtime`, `tests`,
/// `e2e_tests`) each holding an array of `{file, occurrence_count,
/// max_depth, sample_keys}`. Order within each bucket follows input
/// order — typically pagerank-ish from the upstream impact/neighbor
/// ranking.
fn categorize_blast_groups(
    groups: Vec<djinn_control_plane::bridge::FileGroupEntry>,
) -> serde_json::Value {
    let mut runtime = Vec::new();
    let mut tests = Vec::new();
    let mut e2e_tests = Vec::new();

    for g in groups {
        let path = g.file.as_str();
        let entry = serde_json::json!({
            "file": g.file,
            "occurrence_count": g.occurrence_count,
            "max_depth": g.max_depth,
            "sample_keys": g.sample_keys,
        });
        if is_e2e_test_path(path) {
            e2e_tests.push(entry);
        } else if is_test_path(path) {
            tests.push(entry);
        } else {
            runtime.push(entry);
        }
    }

    serde_json::json!({
        "runtime": runtime,
        "tests": tests,
        "e2e_tests": e2e_tests,
        "totals": {
            "runtime": runtime.len(),
            "tests": tests.len(),
            "e2e_tests": e2e_tests.len(),
        },
    })
}

/// True for tests likely to be slow / require external services. e2e
/// usually runs separately from unit; surfacing it as its own bucket
/// helps reviewers plan their verification (run unit first, e2e on a
/// real env). Always also passes [`is_test_path`].
fn is_e2e_test_path(path: &str) -> bool {
    if !is_test_path(path) {
        return false;
    }
    path.contains("/e2e/")
        || path.starts_with("e2e/")
        || path.contains("/integration/")
        || path.starts_with("integration/")
        || path.contains("/system/")
        || path.contains("tests/integration/")
        || path.contains("tests/e2e/")
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
/// - `query_subgraph`: natural-language subgraph contract, including required
///   query field, optional narrowing/budget fields, clamps, and response shape.
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
            "search", "query_subgraph", "cycles", "orphans", "path", "edges",
            "describe", "context", "capabilities", "blast_radius",
            "boundary_check", "cochange", "churn", "hotspots",
            "complexity", "refactor_candidates", "api_surface", "metrics_at", "dead_symbols",
            "deprecated_callers", "touches_hot_path", "coupling_hubs",
            "status", "snapshot", "workspaces", "symbols_at", "diff_touches",
            "detect_changes",
        ],
        "default_search_mode": std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE")
            .unwrap_or_else(|_| "name".to_string()),
        "available_search_modes": ["name", "hybrid"],
        "query_subgraph": {
            "operation": "query_subgraph",
            "required": ["query"],
            "query": "Natural-language question text; must be nonblank.",
            "optional_filters": {
                "workspace": "Scope seed search/traversal to a warmed workspace when provided.",
                "context_filter": "Coarse subsystem/API/type/concern substring for narrowing broad questions.",
                "file_filter": "Repository-relative path/file substring; file_glob/file_path/from_glob are compatibility aliases.",
                "kind_filter": "Node-kind narrowing for seeds/traversal (file or symbol).",
                "edge_filters": "Explicit traversal edge kinds such as calls, imports, returns, reads, writes, implements, extends; edge_kind is the single-kind alias.",
                "max_depth": "Traversal depth from selected seeds; 0 keeps seed nodes only. Values clamp to 0..=8.",
                "max_seeds": "Maximum selected seeds. Omit for backend default (~6); positive values clamp to 1..=32.",
                "token_budget": "Approximate response token budget. Omit for backend default (~2000); positive values clamp to 1024..=32000."
            },
            "invalid": "Blank query, zero/negative token_budget, and zero/negative max_seeds are rejected before graph dispatch.",
            "response": {
                "wrapper": "query_subgraph",
                "fields": [
                    "query", "nodes", "edges", "seeds", "inferred_edge_kinds",
                    "budget", "traversal", "narrowing_hints"
                ],
                "budget_fields": [
                    "requested_tokens", "estimated_tokens", "truncated",
                    "omitted_nodes", "omitted_edges"
                ],
                "retry_guidance": "If truncated or too broad, retry with context_filter, file_filter, edge_filters, lower max_depth/max_seeds, or a different token_budget."
            }
        },
        "staleness": {
            "field": "current_head",
            "aliases": ["caller_commit", "currentHead"],
            "behavior": "serve-stale-with-warning-only",
            "response_field": "graph_staleness",
            "description": "Pass the caller's current git commit SHA in `current_head` (or its `caller_commit` / `currentHead` aliases). Every successful response then carries an additive `graph_staleness` object comparing that commit against the cached graph blob's pinned commit. The flag is advisory: the query is never blocked and graph re-warming is never auto-triggered. Omit `current_head` to keep the previous response shape."
        },
        "env_features": {
            // Defaults match the on-by-default behavior in djinn-graph.
            "entry_point_detection": env_on("DJINN_ENTRY_POINT_DETECTION", true),
            "process_detection": env_on("DJINN_PROCESS_DETECTION", true),
            "community_detection": env_on("DJINN_COMMUNITY_DETECTION", true),
            // Opt-in by design.
            "db_access_detection": env_opt_in("DJINN_DB_ACCESS_DETECTION"),
        },
        "access_classifier_languages": ["rust", "go", "python", "typescript", "javascript"],
        "repo_graph_artifact_version": 10,
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

#[cfg(test)]
mod blast_radius_categorize_tests {
    use super::*;

    #[test]
    fn classifies_unit_tests_per_language_convention() {
        // Go.
        assert!(is_test_path("internal/worker/page_worker_test.go"));
        // Rust.
        assert!(is_test_path("crates/foo/src/lib_test.rs"));
        assert!(is_test_path("crates/foo/src/tests.rs"));
        // Python.
        assert!(is_test_path("backend/handlers_test.py"));
        // Kotlin / Scala.
        assert!(is_test_path("src/main/kotlin/foo_test.kt"));
        // TS / JS.
        assert!(is_test_path("ui/src/components/Button.test.tsx"));
        assert!(is_test_path("ui/src/utils/parser.test.ts"));
        assert!(is_test_path("ui/src/utils/parser.spec.tsx"));
        assert!(is_test_path("scripts/util.test.mjs"));
        // Ruby.
        assert!(is_test_path("app/models/user_spec.rb"));
        // Conventional test dirs.
        assert!(is_test_path("tests/integration/foo.go"));
        assert!(is_test_path("crates/foo/tests/integration.rs"));
        assert!(is_test_path("test/unit/foo.py"));
        assert!(is_test_path("ui/src/__tests__/parser.ts"));
    }

    #[test]
    fn does_not_misclassify_legitimate_source_as_tests() {
        // Words containing "test" that aren't tests.
        assert!(!is_test_path("internal/handler/protests_handler.go"));
        assert!(!is_test_path("crates/contest/src/lib.rs"));
        assert!(!is_test_path("internal/util/testify_helper.go"));
        // Files literally named like a test pattern but in a non-test dir.
        // (tests.rs is a Rust convention, so it IS a test — covered above.)
        assert!(!is_test_path("internal/handler/handler.go"));
        assert!(!is_test_path("crates/foo/src/lib.rs"));
    }

    #[test]
    fn separates_e2e_from_unit_tests() {
        // E2E directory variants.
        assert!(is_e2e_test_path(
            "tests/integration/e2e/cw_polling_e2e_test.go"
        ));
        assert!(is_e2e_test_path("tests/e2e/auth_flow_test.go"));
        assert!(is_e2e_test_path("e2e/page_lifecycle_test.go"));
        assert!(is_e2e_test_path("integration/billing_test.go"));
        assert!(is_e2e_test_path("backend/system/system_test.py"));
        // A test that's NOT in e2e dir → not e2e (would be `test`).
        assert!(!is_e2e_test_path("internal/worker/page_worker_test.go"));
        assert!(!is_e2e_test_path("ui/src/components/Button.test.tsx"));
        // Non-test files outside any tests/ dir → not e2e (would be runtime).
        assert!(!is_e2e_test_path("internal/worker/page_worker.go"));
        assert!(!is_e2e_test_path("cmd/dispatcher.go"));
        // Note: a fixture file like `tests/e2e/fixtures.go` IS classified
        // as e2e here — it lives under `tests/` so is_test_path fires,
        // and it lives under `/e2e/` so is_e2e_test_path fires too. That's
        // the intentional behavior: fixtures shipped alongside e2e tests
        // should be in the same bucket as the e2e tests for "what should
        // I re-run" purposes.
    }

    #[test]
    fn categorize_buckets_each_path_correctly() {
        use djinn_control_plane::bridge::FileGroupEntry;
        let groups = vec![
            FileGroupEntry {
                file: "cmd/worker.go".to_string(),
                occurrence_count: 3,
                max_depth: 1,
                sample_keys: vec!["scip-go . . . StartPageWorker().".to_string()],
            },
            FileGroupEntry {
                file: "internal/worker/page_worker_test.go".to_string(),
                occurrence_count: 5,
                max_depth: 1,
                sample_keys: vec![],
            },
            FileGroupEntry {
                file: "tests/integration/e2e/cw_polling_e2e_test.go".to_string(),
                occurrence_count: 2,
                max_depth: 2,
                sample_keys: vec![],
            },
        ];
        let result = categorize_blast_groups(groups);
        assert_eq!(result["totals"]["runtime"], 1);
        assert_eq!(result["totals"]["tests"], 1);
        assert_eq!(result["totals"]["e2e_tests"], 1);
        assert_eq!(
            result["runtime"][0]["file"].as_str().unwrap(),
            "cmd/worker.go"
        );
        assert_eq!(
            result["tests"][0]["file"].as_str().unwrap(),
            "internal/worker/page_worker_test.go"
        );
        assert_eq!(
            result["e2e_tests"][0]["file"].as_str().unwrap(),
            "tests/integration/e2e/cw_polling_e2e_test.go"
        );
    }
}
