use super::*;

// ── Validation ──────────────────────────────────────────────────────────────────

pub(crate) fn validate_direction(direction: Option<&str>) -> Result<(), String> {
    if let Some(d) = direction {
        match d {
            "incoming" | "outgoing" => Ok(()),
            _ => Err(format!(
                "invalid direction '{d}': expected 'incoming' or 'outgoing'"
            )),
        }
    } else {
        Ok(())
    }
}

pub(crate) fn validate_kind_filter(kind_filter: Option<&str>) -> Result<(), String> {
    if let Some(k) = kind_filter {
        match k {
            "file" | "symbol" => Ok(()),
            _ => Err(format!(
                "invalid kind_filter '{k}': expected 'file' or 'symbol'"
            )),
        }
    } else {
        Ok(())
    }
}

/// PR B4: resolved mode for the `search` op. The wire-level vocabulary
/// is two strings (`"name"` / `"hybrid"`); this enum keeps the dispatch
/// site honest about the closed set of options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchMode {
    Name,
    Hybrid,
}

/// PR B4: resolve the effective search mode by layering caller intent
/// on top of the `DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE` env var. The
/// env-var default is `"name"` per the engineering-practices section
/// of the plan; flip to `"hybrid"` after the soak window. Caller wins
/// when both are set so an explicit `mode=name` always pins the fast
/// path even in a hybrid-default deployment.
pub(crate) fn resolve_search_mode(caller: Option<&str>) -> Result<SearchMode, String> {
    let raw = match caller {
        Some(value) => value.to_string(),
        None => std::env::var("DJINN_CODE_GRAPH_SEARCH_DEFAULT_MODE")
            .unwrap_or_else(|_| "name".to_string()),
    };
    match raw.as_str() {
        "name" => Ok(SearchMode::Name),
        "hybrid" => Ok(SearchMode::Hybrid),
        other => Err(format!(
            "invalid search mode '{other}': expected 'name' or 'hybrid'"
        )),
    }
}

/// PR A3: validator for the `neighbors` op's edge-kind filter. Currently
/// accepts `reads` / `writes`; future PRs may extend this with `calls` etc.
/// once the `EdgeCategory` enum lands (PR C1).
pub(crate) fn validate_edge_kind_filter(kind_filter: Option<&str>) -> Result<(), String> {
    if let Some(k) = kind_filter {
        match k {
            "reads" | "writes" => Ok(()),
            _ => Err(format!(
                "invalid kind_filter '{k}' for neighbors: expected 'reads' or 'writes'"
            )),
        }
    } else {
        Ok(())
    }
}

pub(crate) fn validate_flow_kind_filter(kind_filter: Option<&str>) -> Result<(), String> {
    if let Some(k) = kind_filter {
        match k {
            "process" | "step" => Ok(()),
            _ => Err(format!(
                "invalid kind_filter '{k}' for flow: expected 'process' or 'step'"
            )),
        }
    } else {
        Ok(())
    }
}

/// Tuple of (route_id, method, path) selectors that identify a single
/// route. Factored out of [`require_route_selector`]'s return type so
/// the per-op handlers can destructure it without tripping
/// `clippy::type_complexity` on the 3-tuple of `Option<&str>`s.
pub(crate) type RouteSelector<'a> = (Option<&'a str>, Option<&'a str>, Option<&'a str>);

pub(crate) fn require_route_selector(
    params: &CodeGraphParams,
) -> Result<RouteSelector<'_>, String> {
    let route_id = params.route_id.as_deref().filter(|s| !s.is_empty());
    let method = params.method.as_deref().filter(|s| !s.is_empty());
    let path = params.path.as_deref().filter(|s| !s.is_empty());
    if route_id.is_some() || (method.is_some() && path.is_some()) {
        Ok((route_id, method, path))
    } else {
        Err(format!(
            "'route_id' or both 'method' and 'path' are required for operation '{}'",
            params.operation
        ))
    }
}

pub(crate) fn validate_min_confidence_value(value: f64) -> Result<(), String> {
    if (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "invalid min_confidence {value}: must be in [0.0, 1.0]"
        ))
    }
}

pub(crate) fn bounded_required_limit(
    value: Option<i64>,
    default: usize,
    op: &str,
) -> Result<usize, String> {
    let Some(raw) = value else {
        return Ok(default);
    };
    if raw < 0 {
        return Err(format!(
            "invalid limit {raw} for operation '{op}': expected a non-negative integer"
        ));
    }
    Ok(raw as usize)
}

pub(crate) fn require_key(params: &CodeGraphParams) -> Result<&str, String> {
    params
        .key
        .as_deref()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| format!("'key' is required for operation '{}'", params.operation))
}

pub(crate) fn require_query(params: &CodeGraphParams) -> Result<&str, String> {
    params
        .query
        .as_deref()
        .filter(|q| !q.is_empty())
        .ok_or_else(|| format!("'query' is required for operation '{}'", params.operation))
}

pub(crate) fn require_from_to(params: &CodeGraphParams) -> Result<(&str, &str), String> {
    let from = params
        .from
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'from' is required for operation '{}'", params.operation))?;
    let to = params
        .to
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'to' is required for operation '{}'", params.operation))?;
    Ok((from, to))
}

pub(crate) fn require_globs(params: &CodeGraphParams) -> Result<(&str, &str), String> {
    let from = params
        .from_glob
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            format!(
                "'from_glob' is required for operation '{}'",
                params.operation
            )
        })?;
    let to = params
        .to_glob
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("'to_glob' is required for operation '{}'", params.operation))?;
    Ok((from, to))
}

/// Resolve the effective workspace scope for a `code_graph` op that
/// consumes the optional `workspace` parameter (pb94 epic).
///
/// Centralises the three-outcome contract the epic commits to:
///
/// 1. **Empty / unscoped** — caller omitted `workspace` (or sent `""`,
///    already normalized to `None` upstream). Returns
///    `WorkspaceScope { workspace: None, hint: None }` and the bridge
///    runs over the full graph. This matches the pre-pb94 behaviour
///    and the "omit == `None`" shape locked in by
///    `CodeGraphParams::normalize()`.
///
/// 2. **Valid / known** — requested slug is non-empty and resolves to at
///    least one node in the graph. Returns
///    `WorkspaceScope { workspace: Some(slug), hint: None }` and the
///    bridge hard-filters for listing/bounded ops. **Single-workspace
///    graphs land here too** — the workspace filter is structurally a
///    no-op (every node matches) rather than producing a hard-empty
///    result, per the epic "scoping is a no-op for single-workspace
///    graphs" requirement.
///
/// 3. **Unknown non-empty slug** — requested slug is non-empty and the
///    graph contains no node in that workspace. Returns
///    `WorkspaceScope { workspace: None, hint: Some(candidates) }` so
///    the bridge returns the full/unscoped result AND the caller gets
///    the candidate list to recover from the typo. Single-candidate
///    hint lists are suppressed (a "no choice" project never hints).
///
/// This helper is the only call site that needs to call
/// `RepoGraphOps::workspace_hint` — every per-op handler that uses
/// `workspace` should call this once and feed the result into both the
/// bridge call (`scope.workspace.as_deref()`) and the response
/// (`scope.hint`).
///
/// `query_subgraph` and a few other ops take `workspace` via their own
/// request type; they call `workspace_hint` directly to keep their
/// request DTO independent of the dispatcher.
pub(crate) async fn resolve_workspace_scope(
    ops: &Arc<dyn crate::bridge::RepoGraphOps>,
    ctx: &crate::bridge::ProjectCtx,
) -> Result<crate::bridge::WorkspaceScope, String> {
    let requested = ctx.workspace.as_deref();
    let hint = ops.workspace_hint(ctx, requested).await?;
    let scope = match hint {
        // Unknown non-empty slug AND the bridge confirms the project
        // has multiple workspaces to choose from: degrade to unscoped
        // and surface the candidate list. `workspace_hint_from_graph`
        // (in mcp_bridge.rs) gates the `Some(_)` return on
        // `slugs.len() > 1` so single-workspace graphs land in the
        // `None` arm below and the workspace is a structural no-op.
        Some(candidates) => crate::bridge::WorkspaceScope {
            workspace: None,
            hint: Some(candidates),
        },
        // Known, valid, or no-choice: use the caller's workspace as-is
        // (or `None` for unscoped callers). The bridge handles
        // single-workspace graphs transparently — every node matches
        // the only slug, so the filter is a no-op.
        None => crate::bridge::WorkspaceScope {
            workspace: requested.map(str::to_string),
            hint: None,
        },
    };
    Ok(scope)
}

pub(crate) fn bounded_optional_usize(
    value: Option<i64>,
    name: &str,
    min: usize,
    max: usize,
    allow_zero: bool,
) -> Result<Option<usize>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    if raw < 0 || (!allow_zero && raw == 0) {
        return Err(format!("invalid {name} {raw}: expected a positive integer"));
    }
    Ok(Some((raw as usize).clamp(min, max)))
}

pub(crate) fn validate_visibility(visibility: Option<&str>) -> Result<(), String> {
    if let Some(v) = visibility {
        match v {
            "public" | "private" | "any" => Ok(()),
            _ => Err(format!(
                "invalid visibility '{v}': expected 'public', 'private', or 'any'"
            )),
        }
    } else {
        Ok(())
    }
}

pub(crate) fn validate_sort_by(sort_by: Option<&str>) -> Result<(), String> {
    if let Some(s) = sort_by {
        match s {
            "pagerank" | "in_degree" | "out_degree" | "total_degree" => Ok(()),
            _ => Err(format!(
                "invalid sort_by '{s}': expected 'pagerank', 'in_degree', 'out_degree', or 'total_degree'"
            )),
        }
    } else {
        Ok(())
    }
}

pub(crate) fn validate_group_by(group_by: Option<&str>) -> Result<(), String> {
    if let Some(g) = group_by {
        match g {
            "file" => Ok(()),
            _ => Err(format!("invalid group_by '{g}': only 'file' is supported")),
        }
    } else {
        Ok(())
    }
}
