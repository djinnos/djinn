use djinn_control_plane::bridge::{ImpactEntry, PagerankTier};
use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};
use djinn_graph::repo_graph::{RepoGraphEdge, RepoGraphEdgeKind, RouteExclusionConfig};
use petgraph::visit::EdgeRef;

use super::graph_neighbors::{
    format_node_key, resolve_node_with_hint, resolve_node_with_hint_and_filter,
};

pub(super) fn normalize_workspace_slug(workspace: Option<&str>) -> Option<String> {
    let slug = workspace?.trim().trim_matches('/').trim();
    if slug.is_empty() {
        None
    } else {
        Some(slug.replace('\\', "/"))
    }
}

pub(super) fn normalize_workspace_prefix(workspace: Option<&str>) -> Option<String> {
    normalize_workspace_slug(workspace).map(|slug| format!("{slug}/"))
}

pub(super) fn repo_graph_node_file_path(
    node: &djinn_graph::repo_graph::RepoGraphNode,
) -> Option<String> {
    node.file_path
        .as_ref()
        .map(|path| path.display().to_string())
}

pub(super) fn repo_graph_node_matches_workspace(
    node: &djinn_graph::repo_graph::RepoGraphNode,
    workspace_slug: &str,
) -> bool {
    if node
        .workspace
        .as_deref()
        .is_some_and(|slug| slug.trim().trim_matches('/').eq(workspace_slug))
    {
        return true;
    }
    let Some(workspace_prefix) = normalize_workspace_prefix(Some(workspace_slug)) else {
        return false;
    };
    repo_graph_node_file_path(node)
        .as_deref()
        .is_some_and(|path| path.starts_with(&workspace_prefix))
}

pub(super) fn active_workspace_prefix(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    workspace: Option<&str>,
) -> Option<String> {
    let slug = normalize_workspace_slug(workspace)?;
    graph
        .graph()
        .node_indices()
        .any(|idx| repo_graph_node_matches_workspace(graph.node(idx), &slug))
        .then_some(slug)
}

pub(super) fn available_workspace_slugs(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
) -> Vec<String> {
    let mut slugs = std::collections::BTreeSet::new();
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if let Some(slug) = node
            .workspace
            .as_deref()
            .and_then(|s| normalize_workspace_slug(Some(s)))
        {
            slugs.insert(slug);
            continue;
        }
        if let Some(path) = repo_graph_node_file_path(node) {
            let path = path.trim().trim_matches('/').replace('\\', "/");
            if let Some((first, _rest)) = path.split_once('/')
                && !first.is_empty()
            {
                slugs.insert(first.to_string());
            }
        }
    }
    slugs.into_iter().collect()
}

pub(super) fn graph_workspace_node_counts(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for idx in graph.graph().node_indices() {
        let node = graph.node(idx);
        if node.is_external {
            continue;
        }
        let slug = node
            .workspace
            .as_deref()
            .and_then(|s| normalize_workspace_slug(Some(s)))
            .unwrap_or_else(|| "root".to_string());
        *counts.entry(slug).or_insert(0) += 1;
    }
    counts
}

pub(super) fn workspace_hint_from_graph(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    workspace: Option<&str>,
) -> Option<Vec<String>> {
    let requested = normalize_workspace_slug(workspace)?;
    if graph
        .graph()
        .node_indices()
        .any(|idx| repo_graph_node_matches_workspace(graph.node(idx), &requested))
    {
        return None;
    }
    let slugs = available_workspace_slugs(graph);
    (slugs.len() > 1).then_some(slugs)
}

pub(super) fn resolve_node_or_err_for_workspace_seed(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    key: &str,
    workspace: Option<&str>,
) -> Result<petgraph::graph::NodeIndex, String> {
    let outcome = match active_workspace_prefix(graph, workspace) {
        Some(workspace_slug) => resolve_node_with_hint_and_filter(graph, key, None, |node| {
            repo_graph_node_matches_workspace(node, &workspace_slug)
        }),
        None => resolve_node_with_hint(graph, key, None),
    };

    match outcome {
        super::graph_neighbors::ResolveOutcome::Found(idx) => Ok(idx),
        super::graph_neighbors::ResolveOutcome::Ambiguous(candidates) => Err(format!(
            "node '{key}' is ambiguous: {} candidates (e.g. {})",
            candidates.len(),
            candidates
                .first()
                .map(|c| c.uid.as_str())
                .unwrap_or("<none>")
        )),
        super::graph_neighbors::ResolveOutcome::NotFound => {
            Err(format!("node '{key}' not found in graph"))
        }
    }
}

/// Render a `since_days` window as an ISO-8601 UTC lower bound
/// (`YYYY-MM-DDTHH:MM:SSZ`). Stored `committed_at` timestamps use the
/// same fixed-width format, so a lexicographic string comparison on
/// the SQL side resolves the window correctly — no chrono dependency.
pub(super) fn since_days_to_cutoff(since_days: Option<u32>) -> Option<String> {
    since_days.map(|d| {
        let clamped = d.clamp(1, 3650) as u64;
        let now = SystemClockTrait::new()
            .now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now.saturating_sub(clamped * 86_400);
        format_utc_iso8601(cutoff)
    })
}

/// Format a Unix timestamp (seconds since epoch) as ISO-8601 UTC with
/// second resolution (`YYYY-MM-DDTHH:MM:SSZ`). Used to render a
/// `since_days` cutoff for the `churn` op into the same lexical shape
/// our stored `committed_at` uses, so a string comparison on the SQL
/// side resolves the window correctly.
pub(super) fn format_utc_iso8601(secs: u64) -> String {
    // Civil-from-Unix conversion via Howard Hinnant's algorithm
    // (public domain). Avoids a chrono dependency for the single
    // timestamp format we need.
    let days = (secs / 86_400) as i64;
    let rem_seconds = secs % 86_400;
    let hour = (rem_seconds / 3600) as u32;
    let minute = ((rem_seconds % 3600) / 60) as u32;
    let second = (rem_seconds % 60) as u32;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Extract the SCIP crate-name token from a symbol identifier.
///
/// SCIP symbols have the shape:
/// `<scheme> <manager> <package-name> <version> <descriptors>`
///
/// Example: `scip-rust cargo my-crate 0.1.0 foo/Bar#`
///
/// This helper returns the `<package-name>` slot (`my-crate`). Locals
/// (symbols of shape `local <id>`) and any symbol with fewer than four
/// leading tokens return `None`, signaling "no crate identity" to the
/// caller (who then conservatively skips the cross-crate check).
pub(super) fn scip_crate_name(symbol: &str) -> Option<&str> {
    if symbol.starts_with("local ") || symbol.is_empty() {
        return None;
    }
    let mut parts = symbol.split_whitespace();
    let _scheme = parts.next()?;
    let _manager = parts.next()?;
    let package = parts.next()?;
    // Ensure there's at least one more token — the version — so we're
    // not mis-reading a malformed short header as a package name.
    let _version = parts.next()?;
    if package.is_empty() || package == "." {
        return None;
    }
    Some(package)
}

/// Scan a symbol's signature + documentation text for a `#[deprecated]`
/// or `@deprecated` marker.
///
/// `@deprecated` matching is case-insensitive so the common JSDoc and
/// Python-docstring conventions both engage. `#[deprecated` does not
/// require a closing bracket — Rust allows both the bare
/// `#[deprecated]` and `#[deprecated(...)]` forms.
pub(super) fn is_deprecated_text(signature: Option<&str>, documentation: &[String]) -> bool {
    if let Some(sig) = signature
        && (sig.contains("#[deprecated") || sig.to_lowercase().contains("@deprecated"))
    {
        return true;
    }
    for line in documentation {
        if line.contains("#[deprecated") || line.to_lowercase().contains("@deprecated") {
            return true;
        }
    }
    false
}

/// PR s6ch / 92z7: machine-readable exclusion reasons stamped on
/// `ImpactEntry.exclusion_reason` / `EdgeEntry.exclusion_reason` /
/// `ApiImpactEntry.excluded_reason`. Stable strings — the UI is
/// expected to switch on these values to render the entry as a
/// suggestion instead of a hard dependency.
pub(super) mod exclusion_reason {
    /// Inferred `Fetches` edge landed below the project policy's
    /// `min_confidence_for_consumer_edge` floor. The blast-radius
    /// BFS treats the link as a suggestion.
    pub(crate) const BELOW_CONFIDENCE_FLOOR: &str = "below-confidence-floor";
    /// The route path is on the project's health-path glob list
    /// (`/health`, `/healthz`, `/ping`, `/readyz`, `/livez`,
    /// `/metrics` by default). Consumer calls into these endpoints
    /// are not interesting blast-radius signal — they're framework
    /// plumbing, not business logic.
    pub(crate) const HEALTH_PATH: &str = "health-path";
    /// The route path has no static segments (e.g. `/{tenant}` or
    /// `/{id}/{slug}`) and is therefore unlikely to be a
    /// real-architecture consumer. Only emitted when the policy
    /// `param_only_paths` flag is on.
    pub(crate) const PARAM_ONLY_PATH: &str = "param-only-path";
}
/// PR s6ch / 92z7: match a path against the health-path glob list.
/// A `path` is a "health" path when, after lowercasing and trimming
/// trailing slashes, it matches one of the configured globs (e.g.
/// `/health`, `/healthz`, `/ping`, or `/api/*`). Globs without `*`
/// are exact-segment matches, so `/healthcheck` does not match
/// `/health`.
pub(super) fn path_matches_health_glob(path: &str, globs: &[String]) -> bool {
    if globs.is_empty() {
        return false;
    }
    let lowered = path.trim().to_ascii_lowercase();
    let trimmed = lowered.trim_start_matches('/').trim_end_matches('/');
    if trimmed.is_empty() {
        return false;
    }
    globs.iter().any(|glob| {
        let glob_lc = glob.trim().to_ascii_lowercase();
        let glob = glob_lc.trim_start_matches('/').trim_end_matches('/');
        !glob.is_empty() && simple_glob_match(glob, trimmed)
    })
}

fn simple_glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    let mut rest = value;
    let mut parts = pattern.split('*').peekable();
    let anchored_start = !pattern.starts_with('*');
    let anchored_end = !pattern.ends_with('*');
    let mut first = true;
    while let Some(part) = parts.next() {
        if part.is_empty() {
            first = false;
            continue;
        }
        if first && anchored_start {
            if !rest.starts_with(part) {
                return false;
            }
            rest = &rest[part.len()..];
        } else if parts.peek().is_none() && anchored_end {
            return rest.ends_with(part);
        } else if let Some(pos) = rest.find(part) {
            rest = &rest[pos + part.len()..];
        } else {
            return false;
        }
        first = false;
    }
    true
}

/// PR s6ch / 92z7: detect a route path that is made up entirely of
/// parameter segments — e.g. `/{tenant}`, `/{id}/{slug}`, or `:tenant`
/// in the axum `/{tenant}` shape. Such paths rarely reflect real
/// consumer edges because the parameter can stand in for any
/// concrete route, so the policy treats them as suggestions when
/// `param_only_paths` is enabled.
pub(super) fn path_is_param_only(path: &str) -> bool {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        return false;
    }
    trimmed.split('/').all(|segment| {
        segment.starts_with(':')
            || (segment.starts_with('{') && segment.ends_with('}'))
            || (segment.starts_with('<') && segment.ends_with('>'))
    })
}

/// PR s6ch / 92z7: enumerate the active exclusion reasons for a
/// `Fetches` edge that points at a `Route` node. Returns an empty
/// `Vec` when the edge is a hard dependency under the active policy
/// (caller proceeds with normal blast-radius propagation).
///
/// `route_path` is the route node's path component — e.g.
/// `"/api/agents"`. We recover it from the route node's
/// `display_name` / `id` so callers don't have to re-parse the
/// `(method, path)` tuple.
pub(super) fn fetches_exclusion_reasons(
    edge: &RepoGraphEdge,
    route_path: Option<&str>,
    config: &RouteExclusionConfig,
) -> Vec<&'static str> {
    if edge.kind != RepoGraphEdgeKind::Fetches {
        return Vec::new();
    }
    let mut reasons = Vec::new();
    if edge.confidence + f64::EPSILON < config.min_confidence_for_consumer_edge {
        reasons.push(exclusion_reason::BELOW_CONFIDENCE_FLOOR);
    }
    if let Some(path) = route_path
        && !config.health_path_globs.is_empty()
        && path_matches_health_glob(path, &config.health_path_globs)
    {
        reasons.push(exclusion_reason::HEALTH_PATH);
    }
    if config.param_only_paths
        && let Some(path) = route_path
        && path_is_param_only(path)
    {
        reasons.push(exclusion_reason::PARAM_ONLY_PATH);
    }
    reasons
}

/// PR s6ch / 92z7: pick the first exclusion reason (in declaration
/// order) for use in single-value wire fields like
/// `ImpactEntry.exclusion_reason`. Returns `None` when the edge
/// is a hard dependency. Order matches the reasons emitted by
/// [`fetches_exclusion_reasons`] so the wire field is stable.
pub(super) fn first_exclusion_reason(
    edge: &RepoGraphEdge,
    route_path: Option<&str>,
    config: &RouteExclusionConfig,
) -> Option<&'static str> {
    fetches_exclusion_reasons(edge, route_path, config)
        .into_iter()
        .next()
}

/// PR s6ch / 92z7: extract the path component from a `Route` node's
/// identifier. Route IDs follow the shape `"<METHOD> <path> (<framework>)"`
/// (e.g. `"GET /api/agents (axum)"`); we split off the leading
/// method token and drop the trailing framework annotation.
pub(super) fn route_node_path(node: &djinn_graph::repo_graph::RepoGraphNode) -> Option<String> {
    use djinn_graph::repo_graph::RepoNodeKey;
    let raw = match &node.id {
        RepoNodeKey::Route(value) => value.clone(),
        _ => node.display_name.clone(),
    };
    let without_framework = raw.split_once(" (").map_or(raw.as_str(), |(left, _)| left);
    let mut parts = without_framework.split_whitespace();
    let _method = parts.next()?;
    let path = parts.next()?;
    (!path.is_empty()).then(|| path.to_string())
}

/// Predicate: does this edge kind carry "if this changes, that breaks"
/// semantics and therefore propagate the BFS frontier?
///
/// Behavioral-edge whitelist shared by [`impact_bfs`] and
/// [`impact_bfs_with_policy`]. Pure structural anchors
/// (`ContainsDefinition`, `DeclaredInFile`) and synthetic
/// side-channel edges (`MemberOf`, `StepInProcess`,
/// `EntryPointOf`) are skipped.
fn edge_propagates(kind: RepoGraphEdgeKind) -> bool {
    match kind {
        RepoGraphEdgeKind::Reads
        | RepoGraphEdgeKind::Writes
        | RepoGraphEdgeKind::SymbolReference
        | RepoGraphEdgeKind::FileReference
        | RepoGraphEdgeKind::Route
        | RepoGraphEdgeKind::Implements
        | RepoGraphEdgeKind::Extends
        | RepoGraphEdgeKind::TypeDefines
        | RepoGraphEdgeKind::Defines
        | RepoGraphEdgeKind::HandlesRoute
        | RepoGraphEdgeKind::Fetches
        // PR t16t: synthesized trait-dispatch caller edge. It carries the
        // same "if this changes, that breaks" blast-radius semantics as a
        // direct call site — the caller depends on the trait method, even
        // though we don't yet know which concrete impl it resolves to.
        | RepoGraphEdgeKind::TraitDispatchCall => true,
        RepoGraphEdgeKind::ContainsDefinition
        | RepoGraphEdgeKind::DeclaredInFile
        | RepoGraphEdgeKind::MemberOf
        | RepoGraphEdgeKind::StepInProcess
        // Proposal qoxm: co-change coupling is circumstantial history, NOT
        // "if this changes that breaks" — it must never propagate the impact
        // frontier (and in fact lives outside the petgraph the BFS walks). A
        // hard `false` keeps blast radii from silently inflating.
        | RepoGraphEdgeKind::CoChangedWith
        | RepoGraphEdgeKind::EntryPointOf => false,
    }
}

/// Build an [`ImpactEntry`] for a BFS-reached node.
///
/// `exclusion_reason` is `Some(...)` when the policy stamped the
/// first-reached `Fetches` edge as a soft dependency; `None`
/// means the entry is a hard blast-radius link.
fn build_impact_entry(
    node: &djinn_graph::repo_graph::RepoGraphNode,
    depth: usize,
    exclusion_reason: Option<String>,
) -> ImpactEntry {
    let tier = format!("{:?}", node.kind).to_ascii_lowercase();
    ImpactEntry {
        uid: node.stable_uid(),
        key: format_node_key(&node.id),
        depth,
        file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
        confidence_tier: Some(tier),
        exclusion_reason,
    }
}

/// PR s6ch / 92z7: when a newly-reached `Fetches` edge points at a
/// `Route` node, return the first policy exclusion reason (if any).
///
/// Returns `Some(reason)` when the edge is classified as a noisy
/// inferred consumer; `None` when it's a hard dependency or not
/// applicable. Callers should only invoke this when
/// `newly_inserted == true` — stamping happens once on first reach
/// so that a node also reachable via a hard edge keeps its hard
/// classification.
fn stamp_fetches_route_exclusion(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    edge: &RepoGraphEdge,
    target: petgraph::graph::NodeIndex,
    policy: &RouteExclusionConfig,
) -> Option<&'static str> {
    use djinn_graph::repo_graph::RepoGraphNodeKind;
    if edge.kind != RepoGraphEdgeKind::Fetches {
        return None;
    }
    let target_node = graph.node(target);
    if target_node.kind != RepoGraphNodeKind::Route {
        return None;
    }
    let route_path = route_node_path(target_node);
    first_exclusion_reason(edge, route_path.as_deref(), policy)
}

/// PR F4: pick the [`djinn_graph::processes::Process`] id whose member
/// list places `node` at the lowest step ordinal (most upstream). When
/// the node sits in two flows — say it's `step=0` in process A and
/// `step=5` in process B — process A wins because it identifies this
/// v8: BFS used by `impact` and its tests. Walks Incoming edges from
/// `start` up to `max_depth`, returning each visited node with the
/// depth at which it was first reached.
///
/// Two filters cut the BFS frontier so transitive impact reflects
/// load-bearing propagation, not "every node anchored to the queried
/// file":
///
/// * **Behavioral-edge whitelist.** Only edges that actually carry
///   "if this changes, that breaks" semantics propagate the BFS:
///   `Reads`, `Writes`, `SymbolReference`, `FileReference` (the
///   file→file dependency edge that drives file-level impact),
///   `Implements`, `Extends`, `TypeDefines`, `Defines`. Pure
///   structural anchors (`ContainsDefinition` = "file contains this
///   symbol", `DeclaredInFile` = "this symbol lives in this file")
///   and synthetic side-channel edges (`MemberOf`, `StepInProcess`,
///   `EntryPointOf`) are skipped — they connect everything that
///   contains everything, not "this changes when that changes".
/// * **Confidence floor.** Defaults to 0.85 when the caller passes
///   `None`; pass `Some(0.0)` to opt back into the full set.
#[allow(dead_code)] // Kept for the parity-disabled shadow path and the
// pre-92z7 test suite. Production code routes
// through [`impact_bfs_with_policy`] instead.
pub(super) fn impact_bfs(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    start: petgraph::graph::NodeIndex,
    max_depth: usize,
    min_confidence: Option<f64>,
) -> Vec<(petgraph::graph::NodeIndex, ImpactEntry)> {
    let confidence_threshold = min_confidence.unwrap_or(0.85);

    let mut visited = std::collections::HashSet::new();
    visited.insert(start);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start, 0usize));
    let mut result: Vec<(petgraph::graph::NodeIndex, ImpactEntry)> = Vec::new();

    while let Some((current, depth)) = queue.pop_front() {
        if depth > 0 {
            let node = graph.node(current);
            result.push((current, build_impact_entry(node, depth, None)));
        }
        if depth < max_depth {
            for edge in graph
                .graph()
                .edges_directed(current, petgraph::Direction::Incoming)
            {
                if !edge_propagates(edge.weight().kind) {
                    continue;
                }
                if edge.weight().confidence < confidence_threshold {
                    continue;
                }
                let source = edge.source();
                if visited.insert(source) {
                    queue.push_back((source, depth + 1));
                }
            }
        }
    }
    result
}

/// PR s6ch / 92z7: parity-aware variant of [`impact_bfs`]. Walks the
/// same behavioral-edge whitelist but, when a `Fetches` edge lands
/// on a `Route` whose path or confidence falls inside the
/// [`RouteExclusionConfig`] policy, the upstream node is still
/// visited (so the UI can render it as a soft dependency) but the
/// `ImpactEntry` carries an `exclusion_reason` instead of being a
/// hard blast-radius link.
///
/// `min_confidence`: the legacy caller-supplied threshold. When
/// `None`, the floor is `0.85` (matching `impact_bfs`).
/// `policy`: the project's [`RouteExclusionConfig`]. When `None`,
/// behaves identically to `impact_bfs` — i.e. every `Fetches` edge
/// above the confidence floor counts as a hard dependency. This
/// keeps the parity-disabled shadow path byte-compatible with the
/// pre-92z7 behaviour.
pub(super) fn impact_bfs_with_policy(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    start: petgraph::graph::NodeIndex,
    max_depth: usize,
    min_confidence: Option<f64>,
    policy: Option<&RouteExclusionConfig>,
) -> Vec<(petgraph::graph::NodeIndex, ImpactEntry)> {
    let confidence_threshold = min_confidence.unwrap_or(0.85);

    let mut visited = std::collections::HashSet::new();
    visited.insert(start);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start, 0usize));
    // Per-node side table: maps the BFS-reached upstream caller
    // node to the exclusion reason the policy stamped on the
    // `Fetches` edge that *first* reached it. A node that's
    // reached through a hard path later keeps its hard
    // classification (we never overwrite a `None` once we've
    // placed a `Some`).
    let mut suggestion_reasons: std::collections::HashMap<
        petgraph::graph::NodeIndex,
        &'static str,
    > = std::collections::HashMap::new();
    let mut result: Vec<(petgraph::graph::NodeIndex, ImpactEntry)> = Vec::new();

    while let Some((current, depth)) = queue.pop_front() {
        if depth > 0 {
            let node = graph.node(current);
            let exclusion_reason: Option<String> =
                suggestion_reasons.get(&current).map(|s| (*s).to_string());
            result.push((current, build_impact_entry(node, depth, exclusion_reason)));
        }
        if depth < max_depth {
            for edge in graph
                .graph()
                .edges_directed(current, petgraph::Direction::Incoming)
            {
                if !edge_propagates(edge.weight().kind) {
                    continue;
                }
                if edge.weight().confidence < confidence_threshold {
                    continue;
                }
                let source = edge.source();
                let newly_inserted = visited.insert(source);
                if newly_inserted {
                    queue.push_back((source, depth + 1));
                }
                // PR s6ch / 92z7: stamp the first exclusion reason
                // for a newly-reached `Fetches`→`Route` edge via
                // the shared policy helper. Only records when this
                // edge was the *first* to reach the upstream node,
                // so a node reachable via a hard edge and a soft
                // edge keeps the hard-edge classification.
                if let (Some(cfg), true) = (policy, newly_inserted)
                    && let Some(reason) =
                        stamp_fetches_route_exclusion(graph, edge.weight(), edge.target(), cfg)
                {
                    suggestion_reasons.entry(source).or_insert(reason);
                }
            }
        }
    }
    result
}

/// node as an entry point (or near-entry), which is the more
/// actionable bucket label for the UI.
///
/// Returns `None` when the node is not a step in any process. Ties on
/// step ordinal are broken by `Process::id` (lex asc) so the result
/// is deterministic across rebuilds.
pub(super) fn pick_lowest_ordinal_process_id(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    node: petgraph::graph::NodeIndex,
) -> Option<String> {
    let processes = graph.processes_for_node(node);
    if processes.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &str)> = None;
    for proc in processes {
        let step_ord = proc
            .steps
            .iter()
            .position(|step| *step == node)
            .unwrap_or(usize::MAX);
        match best {
            None => best = Some((step_ord, proc.id.as_str())),
            Some((cur_ord, cur_id)) => {
                if step_ord < cur_ord || (step_ord == cur_ord && proc.id.as_str() < cur_id) {
                    best = Some((step_ord, proc.id.as_str()));
                }
            }
        }
    }
    best.map(|(_, id)| id.to_string())
}

/// Quartile thresholds for PageRank tiering, computed once per
/// `detect_changes` call.
///
/// Returns `(q33, q67)` from the symbol-only PageRank distribution:
/// scores ≥ q67 → High, q33..q67 → Medium, < q33 → Low.
///
/// Symbol nodes only because file nodes' PageRank is structurally
/// inflated by the `ContainsDefinition` fan-out (every symbol
/// declares-in its file), so mixing them in produces thresholds
/// that flag every method as Low and every file as High.
pub(super) fn quartile_thresholds(
    ranking: &djinn_graph::repo_graph::RepoGraphRanking,
) -> (f64, f64) {
    use djinn_graph::repo_graph::RepoGraphNodeKind;
    let mut scores: Vec<f64> = ranking
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, RepoGraphNodeKind::Symbol))
        .map(|n| n.page_rank)
        .collect();
    scores.sort_by(|a, b| a.total_cmp(b));
    if scores.is_empty() {
        return (0.0, 0.0);
    }
    // Use 1/3 and 2/3 quantiles — three roughly equal-sized buckets.
    // True quartiles would split four ways; we want three (High /
    // Medium / Low) so 33rd and 67th percentiles are the right cuts.
    let q33_idx = (scores.len() as f64 * 0.34).floor() as usize;
    let q67_idx = (scores.len() as f64 * 0.67).floor() as usize;
    let q33 = scores[q33_idx.min(scores.len() - 1)];
    let q67 = scores[q67_idx.min(scores.len() - 1)];
    (q33, q67)
}

pub(super) fn bucket_pagerank(thresholds: &(f64, f64), score: f64) -> PagerankTier {
    let (q33, q67) = *thresholds;
    if score >= q67 {
        PagerankTier::High
    } else if score >= q33 {
        PagerankTier::Medium
    } else {
        PagerankTier::Low
    }
}

pub(super) fn tier_rank(t: PagerankTier) -> u8 {
    match t {
        PagerankTier::High => 0,
        PagerankTier::Medium => 1,
        PagerankTier::Low => 2,
    }
}

/// Resolve the (start_line, end_line) enclosing range for a touched
/// symbol. Falls back to (0, 0) when the per-file `symbol_ranges`
/// sidecar is empty (cache-restored graph) — see
/// `RepoDependencyGraph::range_for_node` for the limitation.
pub(super) fn symbol_range_for_node(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    idx: petgraph::graph::NodeIndex,
    file: &std::path::Path,
) -> (u32, u32) {
    graph.range_for_node(idx, file).unwrap_or((0, 0))
}
