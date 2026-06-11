use djinn_control_plane::bridge::{ImpactEntry, PagerankTier};
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
        let now = std::time::SystemTime::now()
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
pub(super) fn impact_bfs(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
    start: petgraph::graph::NodeIndex,
    max_depth: usize,
    min_confidence: Option<f64>,
) -> Vec<(petgraph::graph::NodeIndex, ImpactEntry)> {
    use djinn_graph::repo_graph::RepoGraphEdgeKind;
    let propagates = |kind: RepoGraphEdgeKind| match kind {
        RepoGraphEdgeKind::Reads
        | RepoGraphEdgeKind::Writes
        | RepoGraphEdgeKind::SymbolReference
        | RepoGraphEdgeKind::FileReference
        | RepoGraphEdgeKind::Implements
        | RepoGraphEdgeKind::Extends
        | RepoGraphEdgeKind::TypeDefines
        | RepoGraphEdgeKind::Defines
        | RepoGraphEdgeKind::HandlesRoute
        | RepoGraphEdgeKind::Fetches => true,
        RepoGraphEdgeKind::ContainsDefinition
        | RepoGraphEdgeKind::DeclaredInFile
        | RepoGraphEdgeKind::MemberOf
        | RepoGraphEdgeKind::StepInProcess
        | RepoGraphEdgeKind::EntryPointOf
        | RepoGraphEdgeKind::HandlesRoute
        | RepoGraphEdgeKind::Fetches => false,
    };
    let confidence_threshold = min_confidence.unwrap_or(0.85);

    let mut visited = std::collections::HashSet::new();
    visited.insert(start);
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((start, 0usize));
    let mut result: Vec<(petgraph::graph::NodeIndex, ImpactEntry)> = Vec::new();

    while let Some((current, depth)) = queue.pop_front() {
        if depth > 0 {
            let node = graph.node(current);
            result.push((
                current,
                ImpactEntry {
                    key: format_node_key(&node.id),
                    depth,
                    file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
                },
            ));
        }
        if depth < max_depth {
            for edge in graph
                .graph()
                .edges_directed(current, petgraph::Direction::Incoming)
            {
                if !propagates(edge.weight().kind) {
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
