use super::*;

// ── Next-step hints ─────────────────────────────────────────────────────────────

pub(crate) const FALLBACK_NEXT_STEP: &str =
    "Use `code_graph status` to inspect the current graph state.";

/// PR C3: emitted when an `impact` query lands on a HIGH or CRITICAL
/// risk bucket. Steers the caller toward the cleanup ops they should
/// run before the change ships.
pub(crate) const HIGH_IMPACT_NEXT_STEP: &str =
    "Consider running `dead_symbols` and `deprecated_callers` before the change.";

/// Returns whether next-step hints should be appended. Toggled via the
/// `DJINN_CODE_GRAPH_NEXT_STEP_HINTS` env var; default is `true` (only
/// `0` / `false` / `off` / `no` suppress).
pub(crate) fn next_step_hints_enabled() -> bool {
    match std::env::var("DJINN_CODE_GRAPH_NEXT_STEP_HINTS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => true,
    }
}

/// Pick the right hint for the operation and write it into the response
/// envelope. Returns the populated hint string for telemetry.
///
/// The 5 op-specific hints come from the PR A4 plan; everything else
/// gets [`FALLBACK_NEXT_STEP`] so the contract "every response carries
/// a non-empty `next_step`" holds.
pub(crate) fn attach_next_step_hint(op: &str, response: &mut CodeGraphResponse) -> String {
    let hint = compute_next_step_hint(op, response);
    set_next_step_hint(response, hint.clone());
    tracing::debug!(
        op,
        hint = hint.as_str(),
        "code_graph: next_step_hint emitted"
    );
    hint
}

pub(crate) fn compute_next_step_hint(op: &str, response: &CodeGraphResponse) -> String {
    match (op, response) {
        ("search", CodeGraphResponse::Search(s)) => match s.hits.first() {
            Some(hit) => format!(
                "Call `code_graph context name={}` to see full incoming/outgoing.",
                hit.display_name
            ),
            None => FALLBACK_NEXT_STEP.to_string(),
        },
        // PR C1 introduces a dedicated `context` op; until then map onto
        // the existing `describe` arm so the hint chain still nudges the
        // agent toward the impact view.
        ("context" | "describe", _) => {
            "Call `code_graph impact target=<symbol>` to see blast radius.".to_string()
        }
        ("ranked", CodeGraphResponse::Ranked(_)) => {
            "Files at top of PageRank are likely entry points; explore with `context`.".to_string()
        }
        ("cycles", CodeGraphResponse::Cycles(_)) => {
            "Each cycle entry is a tuple of mutually-reaching nodes; resolve with `path`."
                .to_string()
        }
        // PR C3: gate on the freshly-computed `risk` bucket so HIGH /
        // CRITICAL blast radii steer reviewers toward the cleanup ops
        // (`dead_symbols` + `deprecated_callers`). LOW / MEDIUM (and
        // legacy responses without classification) keep the generic
        // fallback hint.
        ("impact", CodeGraphResponse::Impact(r)) => match r.risk {
            Some(risk) if risk.is_high_or_critical() => HIGH_IMPACT_NEXT_STEP.to_string(),
            _ => FALLBACK_NEXT_STEP.to_string(),
        },
        // Iter 28: complexity → nudge the agent to drill into the
        // worst-offender function with `context` before refactoring.
        // Files-target keeps the generic fallback because the file
        // entry doesn't carry a single SCIP key the next call could
        // hand directly to `context`.
        ("complexity", CodeGraphResponse::Complexity(r)) => match &r.complexity {
            crate::bridge::ComplexityResult::Functions(entries) if !entries.is_empty() => {
                "Top entries are refactor candidates. Call code_graph context \
                 name=<key> to see the function in detail before changing."
                    .to_string()
            }
            _ => FALLBACK_NEXT_STEP.to_string(),
        },
        // Iter 29: refactor_candidates → top entries are highest-priority
        // refactor targets (high cognitive + high churn + high pagerank).
        // Steer the caller into `context` on the worst offender.
        ("refactor_candidates", CodeGraphResponse::RefactorCandidates(r))
            if !r.refactor_candidates.is_empty() =>
        {
            "Top entries are highest-priority refactor targets (high cognitive + \
             high churn + high pagerank). Call code_graph context name=<key> \
             to inspect before changing."
                .to_string()
        }
        // PR D2: nudge the caller toward `context` on a top-PageRank
        // node. Truncation is the common case for medium repos, so the
        // hint focuses on drilling into the cap rather than expanding
        // it.
        ("query_subgraph", CodeGraphResponse::QuerySubgraph(r)) => r
            .query_subgraph
            .narrowing_hints
            .first()
            .cloned()
            .unwrap_or_else(|| FALLBACK_NEXT_STEP.to_string()),
        ("snapshot", CodeGraphResponse::Snapshot(r)) => match r.snapshot.nodes.first() {
            Some(node) => format!(
                "Snapshot capped at {} of {} nodes; call `code_graph context name={}` to drill in.",
                r.snapshot.nodes.len(),
                r.snapshot.total_nodes,
                node.label,
            ),
            None => FALLBACK_NEXT_STEP.to_string(),
        },
        _ => FALLBACK_NEXT_STEP.to_string(),
    }
}

pub(crate) fn set_next_step_hint(response: &mut CodeGraphResponse, hint: String) {
    let slot = next_step_slot(response);
    *slot = Some(hint);
}

pub(crate) fn next_step_slot(response: &mut CodeGraphResponse) -> &mut Option<String> {
    match response {
        CodeGraphResponse::Neighbors(r) => &mut r.next_step,
        CodeGraphResponse::Ranked(r) => &mut r.next_step,
        CodeGraphResponse::Implementations(r) => &mut r.next_step,
        CodeGraphResponse::Impact(r) => &mut r.next_step,
        CodeGraphResponse::Search(r) => &mut r.next_step,
        CodeGraphResponse::Cycles(r) => &mut r.next_step,
        CodeGraphResponse::Orphans(r) => &mut r.next_step,
        CodeGraphResponse::Path(r) => &mut r.next_step,
        CodeGraphResponse::Edges(r) => &mut r.next_step,
        CodeGraphResponse::Describe(r) => &mut r.next_step,
        CodeGraphResponse::Context(r) => &mut r.next_step,
        CodeGraphResponse::Status(r) => &mut r.next_step,
        CodeGraphResponse::Workspaces(r) => &mut r.next_step,
        CodeGraphResponse::SymbolsAt(r) => &mut r.next_step,
        CodeGraphResponse::DiffTouches(r) => &mut r.next_step,
        CodeGraphResponse::ApiSurface(r) => &mut r.next_step,
        CodeGraphResponse::BoundaryCheck(r) => &mut r.next_step,
        CodeGraphResponse::Hotspots(r) => &mut r.next_step,
        CodeGraphResponse::Complexity(r) => &mut r.next_step,
        CodeGraphResponse::RefactorCandidates(r) => &mut r.next_step,
        CodeGraphResponse::MetricsAt(r) => &mut r.next_step,
        CodeGraphResponse::DeadSymbols(r) => &mut r.next_step,
        CodeGraphResponse::DeprecatedCallers(r) => &mut r.next_step,
        CodeGraphResponse::TouchesHotPath(r) => &mut r.next_step,
        CodeGraphResponse::Coupling(r) => &mut r.next_step,
        CodeGraphResponse::Churn(r) => &mut r.next_step,
        CodeGraphResponse::CouplingHotspots(r) => &mut r.next_step,
        CodeGraphResponse::CouplingHubs(r) => &mut r.next_step,
        CodeGraphResponse::Ambiguous(r) => &mut r.next_step,
        CodeGraphResponse::NotFound(r) => &mut r.next_step,
        CodeGraphResponse::DetectedChanges(r) => &mut r.next_step,
        CodeGraphResponse::Snapshot(r) => &mut r.next_step,
        CodeGraphResponse::QuerySubgraph(r) => &mut r.next_step,
        CodeGraphResponse::RouteMap(r) => &mut r.next_step,
        CodeGraphResponse::ShapeCheck(r) => &mut r.next_step,
        CodeGraphResponse::ApiImpact(r) => &mut r.next_step,
        CodeGraphResponse::Flow(r) => &mut r.next_step,
        CodeGraphResponse::CrateGraph(r) => &mut r.next_step,
        CodeGraphResponse::ImpactCheck(r) => &mut r.next_step,
        CodeGraphResponse::Coverage(r) => &mut r.next_step,
    }
}

/// Pick the highest-priority touched symbol for the next-step impact
/// hint. High-tier wins over Medium wins over Low; ties break on
/// `name` for stability.
pub(crate) fn pick_next_step_target(
    symbols: &[crate::bridge::DetectedTouchedSymbol],
) -> Option<String> {
    use crate::bridge::PagerankTier;
    fn rank(t: PagerankTier) -> u8 {
        match t {
            PagerankTier::High => 0,
            PagerankTier::Medium => 1,
            PagerankTier::Low => 2,
        }
    }
    symbols
        .iter()
        .min_by(|a, b| {
            rank(a.pagerank_tier)
                .cmp(&rank(b.pagerank_tier))
                .then_with(|| a.name.cmp(&b.name))
        })
        .map(|s| s.uid.clone())
}

// ── jc47: per-query graph staleness ───────────────────────────────────────────

/// Mutable slot for the `graph_staleness` field on every response variant,
/// mirroring [`next_step_slot`]. Returns `&mut Option<GraphStaleness>` so
/// the dispatch layer can populate it after the op returns.
pub(crate) fn graph_staleness_slot(
    response: &mut CodeGraphResponse,
) -> &mut Option<GraphStaleness> {
    match response {
        CodeGraphResponse::Neighbors(r) => &mut r.graph_staleness,
        CodeGraphResponse::Ranked(r) => &mut r.graph_staleness,
        CodeGraphResponse::Implementations(r) => &mut r.graph_staleness,
        CodeGraphResponse::Impact(r) => &mut r.graph_staleness,
        CodeGraphResponse::Search(r) => &mut r.graph_staleness,
        CodeGraphResponse::Cycles(r) => &mut r.graph_staleness,
        CodeGraphResponse::Orphans(r) => &mut r.graph_staleness,
        CodeGraphResponse::Path(r) => &mut r.graph_staleness,
        CodeGraphResponse::Edges(r) => &mut r.graph_staleness,
        CodeGraphResponse::Describe(r) => &mut r.graph_staleness,
        CodeGraphResponse::Context(r) => &mut r.graph_staleness,
        CodeGraphResponse::Status(r) => &mut r.graph_staleness,
        CodeGraphResponse::Workspaces(r) => &mut r.graph_staleness,
        CodeGraphResponse::SymbolsAt(r) => &mut r.graph_staleness,
        CodeGraphResponse::DiffTouches(r) => &mut r.graph_staleness,
        CodeGraphResponse::ApiSurface(r) => &mut r.graph_staleness,
        CodeGraphResponse::BoundaryCheck(r) => &mut r.graph_staleness,
        CodeGraphResponse::Hotspots(r) => &mut r.graph_staleness,
        CodeGraphResponse::Complexity(r) => &mut r.graph_staleness,
        CodeGraphResponse::RefactorCandidates(r) => &mut r.graph_staleness,
        CodeGraphResponse::MetricsAt(r) => &mut r.graph_staleness,
        CodeGraphResponse::DeadSymbols(r) => &mut r.graph_staleness,
        CodeGraphResponse::DeprecatedCallers(r) => &mut r.graph_staleness,
        CodeGraphResponse::TouchesHotPath(r) => &mut r.graph_staleness,
        CodeGraphResponse::Coupling(r) => &mut r.graph_staleness,
        CodeGraphResponse::Churn(r) => &mut r.graph_staleness,
        CodeGraphResponse::CouplingHotspots(r) => &mut r.graph_staleness,
        CodeGraphResponse::CouplingHubs(r) => &mut r.graph_staleness,
        CodeGraphResponse::Ambiguous(r) => &mut r.graph_staleness,
        CodeGraphResponse::NotFound(r) => &mut r.graph_staleness,
        CodeGraphResponse::DetectedChanges(r) => &mut r.graph_staleness,
        CodeGraphResponse::Snapshot(r) => &mut r.graph_staleness,
        CodeGraphResponse::QuerySubgraph(r) => &mut r.graph_staleness,
        CodeGraphResponse::RouteMap(r) => &mut r.graph_staleness,
        CodeGraphResponse::ShapeCheck(r) => &mut r.graph_staleness,
        CodeGraphResponse::ApiImpact(r) => &mut r.graph_staleness,
        CodeGraphResponse::Flow(r) => &mut r.graph_staleness,
        CodeGraphResponse::CrateGraph(r) => &mut r.graph_staleness,
        CodeGraphResponse::ImpactCheck(r) => &mut r.graph_staleness,
        CodeGraphResponse::Coverage(r) => &mut r.graph_staleness,
    }
}

// ── glqk: per-query coverage advisory ─────────────────────────────────────────

/// Mutable slot for the `coverage` advisory field, but ONLY on the six response
/// variants the proposal designates: `dead_symbols`, `orphans`, `impact`,
/// `impact_check`, `search`, `api_surface`. All other variants return `None`
/// (they carry no coverage field), so [`attach_coverage_advisory`] simply skips
/// them.
pub(crate) fn coverage_advisory_slot(
    response: &mut CodeGraphResponse,
) -> Option<&mut Option<CoverageAdvisory>> {
    match response {
        CodeGraphResponse::DeadSymbols(r) => Some(&mut r.coverage),
        CodeGraphResponse::Orphans(r) => Some(&mut r.coverage),
        CodeGraphResponse::Impact(r) => Some(&mut r.coverage),
        CodeGraphResponse::ImpactCheck(r) => Some(&mut r.coverage),
        CodeGraphResponse::Search(r) => Some(&mut r.coverage),
        CodeGraphResponse::ApiSurface(r) => Some(&mut r.coverage),
        _ => None,
    }
}

/// glqk: attach the compact coverage advisory to a successful response when a
/// genuine gap exists. Best-effort and cheap — reads `project_workspace_coverage`
/// rows (no graph blob) and only writes the advisory when at least one in-scope
/// workspace is `indexer_failed` / `timed_out` / `unsupported_language`. A clean
/// or intentionally-`excluded`-only project yields no advisory, mirroring how
/// `graph_staleness` is omitted on a fresh graph.
pub(crate) async fn attach_coverage_advisory(
    db: &djinn_db::Database,
    project_id: &str,
    response: &mut CodeGraphResponse,
) {
    // Skip variants that carry no coverage field before touching the DB.
    if coverage_advisory_slot(response).is_none() {
        return;
    }
    let rows = match djinn_db::ProjectWorkspaceCoverageRepository::new(db.clone())
        .list_for_project(project_id)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "code_graph coverage advisory: list_for_project failed; omitting advisory"
            );
            return;
        }
    };

    let gaps: Vec<CoverageAdvisoryWorkspace> = rows
        .into_iter()
        .filter(|r| djinn_db::coverage_status_is_gap(&r.status))
        .map(|r| CoverageAdvisoryWorkspace {
            workspace_slug: r.workspace_slug,
            language: r.language,
            status: r.status,
            detail: r.detail,
        })
        .collect();

    if gaps.is_empty() {
        return;
    }

    let names: Vec<String> = gaps
        .iter()
        .map(|g| format!("{} ({}, {})", g.workspace_slug, g.language, g.status))
        .collect();
    let message = format!(
        "Index coverage is incomplete: {} workspace(s) not indexed — {}. \
         Graph queries (no callers / unused / safe to remove) may be false \
         negatives for these workspaces; fall back to grep and say so.",
        gaps.len(),
        names.join("; ")
    );

    if let Some(slot) = coverage_advisory_slot(response) {
        *slot = Some(CoverageAdvisory {
            message,
            unindexed_workspaces: gaps,
        });
    }
}

/// jc47: attach per-query staleness metadata to a successful response.
///
/// Calls `RepoGraphOps::status` (the same peek used by the `status` op) to
/// get the cached graph blob's pinned commit, computes [`GraphStaleness`]
/// against the caller-supplied commit, and writes it into the response.
///
/// This is best-effort: if the status lookup fails, the staleness field
/// is left as `None` so the query result is not affected. The status
/// peek MUST NOT trigger warming or indexing — the bridge contract
/// guarantees this.
pub(crate) async fn attach_graph_staleness(
    graph: &dyn crate::bridge::RepoGraphOps,
    ctx: &crate::bridge::ProjectCtx,
    caller_commit: &str,
    response: &mut CodeGraphResponse,
) {
    let cached = match graph.status(ctx).await {
        Ok(status) => status.pinned_commit.as_deref().map(str::to_string),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "code_graph staleness: status lookup failed; omitting graph_staleness"
            );
            return;
        }
    };
    *graph_staleness_slot(response) =
        Some(GraphStaleness::compute(caller_commit, cached.as_deref()));
}
