use std::collections::BTreeMap;

use djinn_control_plane::bridge::{
    Candidate, EdgeCategory, FileGroupEntry, GraphNeighbor, ImpactEntry, MethodMeta, MethodParam,
    RelatedSymbol,
};

use djinn_graph::repo_graph::{
    RepoDependencyGraph, RepoGraphEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
    RepoNodeKey,
};
use djinn_graph::scip_parser::ScipSymbolKind;

pub(super) fn group_neighbors_by_file(
    neighbors: &[(&RepoGraphNode, GraphNeighbor)],
) -> Vec<FileGroupEntry> {
    let mut by_file: BTreeMap<String, FileGroupEntry> = BTreeMap::new();
    for (node, neighbor) in neighbors {
        let file_label = node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| match &node.id {
                RepoNodeKey::File(p) => p.display().to_string(),
                RepoNodeKey::Symbol(s) => s.clone(),
            });
        let entry = by_file.entry(file_label.clone()).or_insert(FileGroupEntry {
            file: file_label,
            occurrence_count: 0,
            max_depth: 1,
            sample_keys: Vec::new(),
        });
        entry.occurrence_count += 1;
        if entry.sample_keys.len() < 5 {
            entry.sample_keys.push(neighbor.key.clone());
        }
    }
    by_file.into_values().collect()
}

pub(super) fn group_impact_by_file(
    graph: &RepoDependencyGraph,
    entries: &[(petgraph::graph::NodeIndex, ImpactEntry)],
) -> Vec<FileGroupEntry> {
    let mut by_file: BTreeMap<String, FileGroupEntry> = BTreeMap::new();
    for (idx, entry) in entries {
        let node = graph.node(*idx);
        let file_label = node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| node.display_name.clone());
        let group = by_file.entry(file_label.clone()).or_insert(FileGroupEntry {
            file: file_label,
            occurrence_count: 0,
            max_depth: 0,
            sample_keys: Vec::new(),
        });
        group.occurrence_count += 1;
        if entry.depth > group.max_depth {
            group.max_depth = entry.depth;
        }
        if group.sample_keys.len() < 5 {
            group.sample_keys.push(entry.key.clone());
        }
    }
    by_file.into_values().collect()
}

pub(crate) fn format_node_key(key: &RepoNodeKey) -> String {
    match key {
        RepoNodeKey::File(path) => format!("file:{}", path.display()),
        RepoNodeKey::Symbol(sym) => format!("symbol:{sym}"),
    }
}

/// Outcome of an attempt to resolve a `key` (file path / SCIP symbol /
/// short name) into a single graph node.
///
/// `Found`  — exact (file or symbol) match landed on a unique node.
/// `Ambiguous` — exact match failed but `search_by_name` returned >1 candidates.
/// `NotFound` — neither exact match nor name search produced any hits.
///
/// Behavior gate: when `DJINN_CODE_GRAPH_AMBIGUITY=false`, the resolver
/// collapses the multi-match case to `NotFound` (preserving pre-PR-C2
/// semantics for callers that haven't been updated).
pub(crate) enum ResolveOutcome {
    Found(petgraph::graph::NodeIndex),
    Ambiguous(Vec<Candidate>),
    NotFound,
}

const CANDIDATE_CAP: usize = 8;

/// Read the `DJINN_CODE_GRAPH_AMBIGUITY` env var. Default `true` —
/// "false" / "0" / "off" disable the new behavior and force the
/// resolver to emit `NotFound` rather than `Ambiguous`.
fn ambiguity_enabled() -> bool {
    match std::env::var("DJINN_CODE_GRAPH_AMBIGUITY") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "false" | "0" | "off" | "no" => false,
            _ => true,
        },
        Err(_) => true,
    }
}

/// Map a `RepoGraphNodeKind` to a kind hint string used in candidate
/// scoring: `"file"`, `"function"`, `"method"`, `"class"`, etc.
fn kind_label(node: &RepoGraphNode) -> String {
    match node.kind {
        RepoGraphNodeKind::File => "file".to_string(),
        RepoGraphNodeKind::Symbol => match &node.symbol_kind {
            Some(ScipSymbolKind::Type) => "class".to_string(),
            Some(ScipSymbolKind::Struct) => "struct".to_string(),
            Some(ScipSymbolKind::Interface) => "interface".to_string(),
            Some(ScipSymbolKind::Function) => "function".to_string(),
            Some(ScipSymbolKind::Method) => "method".to_string(),
            Some(ScipSymbolKind::Constructor) => "constructor".to_string(),
            Some(ScipSymbolKind::Enum) => "enum".to_string(),
            Some(ScipSymbolKind::Field) => "field".to_string(),
            Some(ScipSymbolKind::Property) => "property".to_string(),
            Some(ScipSymbolKind::Variable) => "variable".to_string(),
            Some(ScipSymbolKind::Constant) => "constant".to_string(),
            Some(ScipSymbolKind::Namespace) => "namespace".to_string(),
            Some(ScipSymbolKind::Package) => "package".to_string(),
            Some(ScipSymbolKind::EnumMember) => "enum_member".to_string(),
            Some(ScipSymbolKind::Event) => "event".to_string(),
            Some(ScipSymbolKind::Operator) => "operator".to_string(),
            _ => "symbol".to_string(),
        },
    }
}

/// Tiebreaker bonus by SCIP kind, used after the primary score formula.
/// Class/Type > Interface > Function > Method > Constructor > others.
/// Values are tiny (<0.1) so they only break ties between same-base-score
/// candidates; the file-path / kind-hint signals dominate.
fn kind_priority_tiebreaker(node: &RepoGraphNode) -> f64 {
    if matches!(node.kind, RepoGraphNodeKind::File) {
        return 0.01;
    }
    match &node.symbol_kind {
        Some(ScipSymbolKind::Type) | Some(ScipSymbolKind::Struct) | Some(ScipSymbolKind::Enum) => {
            0.05
        }
        Some(ScipSymbolKind::Interface) => 0.04,
        Some(ScipSymbolKind::Function) => 0.03,
        Some(ScipSymbolKind::Method) => 0.02,
        Some(ScipSymbolKind::Constructor) => 0.01,
        _ => 0.0,
    }
}

/// 1.0 if the query (case-insensitive) appears as a substring of the
/// node's file path; 0.0 otherwise.
fn file_path_substring_match(node: &RepoGraphNode, query: &str) -> f64 {
    let q = query.to_lowercase();
    if q.is_empty() {
        return 0.0;
    }
    let candidate_path = node
        .file_path
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| match &node.id {
            RepoNodeKey::File(p) => Some(p.display().to_string()),
            RepoNodeKey::Symbol(_) => None,
        });
    match candidate_path {
        Some(path) if path.to_lowercase().contains(&q) => 1.0,
        _ => 0.0,
    }
}

/// 1.0 if the caller's `kind_hint` (e.g. "class", "function") matches the
/// node's resolved kind label; 0.0 otherwise. None hint disables the
/// signal.
fn kind_hint_match(node: &RepoGraphNode, kind_hint: Option<&str>) -> f64 {
    match kind_hint {
        Some(hint) if !hint.is_empty() => {
            if kind_label(node).eq_ignore_ascii_case(hint) {
                1.0
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

/// Score formula (per the PR C2 spec):
/// ```text
/// score = 0.5
///       + 0.4 * file_path_substring_match(query)
///       + 0.2 * kind_hint_match(hint)
///       + kind_priority_tiebreaker
/// ```
pub(crate) fn score_candidate(
    node: &RepoGraphNode,
    query: &str,
    kind_hint: Option<&str>,
) -> f64 {
    0.5 + 0.4 * file_path_substring_match(node, query)
        + 0.2 * kind_hint_match(node, kind_hint)
        + kind_priority_tiebreaker(node)
}

fn build_candidate(node: &RepoGraphNode, score: f64) -> Candidate {
    let uid = format_node_key(&node.id);
    let file_path = node
        .file_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| match &node.id {
            RepoNodeKey::File(p) => p.display().to_string(),
            RepoNodeKey::Symbol(_) => String::new(),
        });
    Candidate {
        uid,
        name: node.display_name.clone(),
        kind: kind_label(node),
        file_path,
        score,
    }
}

/// Resolve `key` to a single `NodeIndex` in the graph, falling back to
/// a ranked candidate list when the exact match fails. `kind_hint` (e.g.
/// `"class"`, `"function"`) feeds into the score formula and lets the
/// caller bias the disambiguation list.
///
/// Resolution order:
/// 1. Strip `file:` prefix and try the file index.
/// 2. Strip `symbol:` prefix and try the symbol index.
/// 3. Fall back to `RepoDependencyGraph::search_by_name` and emit up
///    to 8 ranked candidates (or `NotFound` when the search returns
///    zero hits / when `DJINN_CODE_GRAPH_AMBIGUITY=false`).
pub(crate) fn resolve_node(graph: &RepoDependencyGraph, key: &str) -> ResolveOutcome {
    resolve_node_with_hint(graph, key, None)
}

pub(crate) fn resolve_node_with_hint(
    graph: &RepoDependencyGraph,
    key: &str,
    kind_hint: Option<&str>,
) -> ResolveOutcome {
    let stripped_file = key.strip_prefix("file:").unwrap_or(key);
    if let Some(index) = graph.file_node(stripped_file) {
        return ResolveOutcome::Found(index);
    }

    let stripped_symbol = key.strip_prefix("symbol:").unwrap_or(key);
    if let Some(index) = graph.symbol_node(stripped_symbol) {
        return ResolveOutcome::Found(index);
    }

    // Exact match failed — fall back to the name index. We over-fetch
    // (3× the cap) so the rescoring pass below has room to pick the
    // strongest candidates after the file-path / kind-hint signals fire.
    let raw_hits = graph.search_by_name(stripped_symbol, None, CANDIDATE_CAP * 3);
    if raw_hits.is_empty() {
        return ResolveOutcome::NotFound;
    }

    if !ambiguity_enabled() {
        return ResolveOutcome::NotFound;
    }

    // Re-rank with the C2 score formula. We apply it to every name-index
    // hit so the wire response orders by `(file-path-match, kind-hint-match,
    // kind-priority)` rather than the raw exact/suffix/substring tiers
    // that `search_by_name` returns.
    let mut scored: Vec<(f64, Candidate)> = raw_hits
        .into_iter()
        .map(|hit| {
            let node = graph.node(hit.node_index);
            let score = score_candidate(node, key, kind_hint);
            (score, build_candidate(node, score))
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.uid.cmp(&b.1.uid))
    });

    let candidates: Vec<Candidate> = scored
        .into_iter()
        .take(CANDIDATE_CAP)
        .map(|(_, c)| c)
        .collect();

    if candidates.is_empty() {
        ResolveOutcome::NotFound
    } else {
        ResolveOutcome::Ambiguous(candidates)
    }
}

/// Compatibility shim for callers that need today's `Result<NodeIndex, String>`
/// semantics — internal mcp_bridge ops that are happy to surface
/// "not found" / "ambiguous" as an opaque `Err` because the user-facing
/// `code_graph` dispatch already pre-resolved the key into a unique
/// node.
pub(super) fn resolve_node_or_err(
    graph: &RepoDependencyGraph,
    key: &str,
) -> Result<petgraph::graph::NodeIndex, String> {
    match resolve_node(graph, key) {
        ResolveOutcome::Found(idx) => Ok(idx),
        ResolveOutcome::Ambiguous(candidates) => Err(format!(
            "node '{key}' is ambiguous: {} candidates (e.g. {})",
            candidates.len(),
            candidates
                .first()
                .map(|c| c.uid.as_str())
                .unwrap_or("<none>")
        )),
        ResolveOutcome::NotFound => Err(format!("node '{key}' not found in graph")),
    }
}

// ── PR C1 helpers (context op) ──────────────────────────────────────────────

/// Map a `RepoGraphEdge` to the [`EdgeCategory`] used to bucket
/// incoming/outgoing neighbors in `code_graph context`. The mapping
/// follows the inter-PR contract table verbatim:
///
/// ```text
/// calls         <- SymbolReference where target.kind ∈ {Function, Method, Constructor}
/// references    <- SymbolReference (catch-all fallback)
/// imports       <- FileReference
/// contains      <- ContainsDefinition | DeclaredInFile
/// extends       <- SymbolRelationshipReference
/// implements    <- SymbolRelationshipImplementation
/// type_defines  <- SymbolRelationshipTypeDefinition
/// defines       <- SymbolRelationshipDefinition
/// reads         <- Reads
/// writes        <- Writes
/// ```
///
/// `target` is whichever endpoint sits opposite the queried node, so the
/// `Calls` discrimination on `SymbolReference` looks at the symbol kind
/// of the *other* endpoint regardless of edge direction.
pub(super) fn classify_edge_category(
    edge: Option<&RepoGraphEdge>,
    other: &RepoGraphNode,
) -> EdgeCategory {
    let Some(edge) = edge else {
        // Defensive: callers always pass a real edge weight, but a None
        // here would mean the graph layer returned an out-of-band
        // sentinel. Treat as a generic reference rather than panic.
        return EdgeCategory::References;
    };
    match edge.kind {
        RepoGraphEdgeKind::FileReference => EdgeCategory::Imports,
        RepoGraphEdgeKind::ContainsDefinition | RepoGraphEdgeKind::DeclaredInFile => {
            EdgeCategory::Contains
        }
        RepoGraphEdgeKind::SymbolReference => {
            // The plan's mapping table says SymbolReference → Calls when
            // the *target* is a callable kind. We let either endpoint's
            // callable-ness decide because neighbors() flips direction
            // based on incoming/outgoing.
            match other.symbol_kind {
                Some(ScipSymbolKind::Function)
                | Some(ScipSymbolKind::Method)
                | Some(ScipSymbolKind::Constructor) => EdgeCategory::Calls,
                _ => EdgeCategory::References,
            }
        }
        RepoGraphEdgeKind::Reads => EdgeCategory::Reads,
        RepoGraphEdgeKind::Writes => EdgeCategory::Writes,
        RepoGraphEdgeKind::SymbolRelationshipReference => EdgeCategory::Extends,
        RepoGraphEdgeKind::SymbolRelationshipImplementation => EdgeCategory::Implements,
        RepoGraphEdgeKind::SymbolRelationshipTypeDefinition => EdgeCategory::TypeDefines,
        RepoGraphEdgeKind::SymbolRelationshipDefinition => EdgeCategory::Defines,
        // PR F1: `EntryPointOf` is metadata stamped by the entry-point
        // detector. Surface as its own category so the UI can badge
        // entry-point symbols.
        RepoGraphEdgeKind::EntryPointOf => EdgeCategory::EntryPoint,
        // PR F3: `MemberOf` is a synthetic community-membership edge.
        // It currently never lives in the petgraph (the membership
        // sidecar carries it instead), but if a future warm pass
        // materializes it, "References" is the closest neutral
        // category — consumers asking "who imports/calls X" should
        // not see community membership in their answers.
        RepoGraphEdgeKind::MemberOf => EdgeCategory::References,
    }
}

/// Public wrapper for the EdgeCategory mapping; used by integration
/// tests in `mcp_bridge.rs` that want to assert classification without
/// going through a full graph round-trip.
#[cfg(test)]
pub(crate) fn edge_category_for(
    edge: Option<&RepoGraphEdge>,
    other: &RepoGraphNode,
) -> EdgeCategory {
    classify_edge_category(edge, other)
}

/// Build a `RelatedSymbol` payload for the `context` op. The
/// `confidence` is plumbed straight from the underlying edge so the UI
/// can de-emphasize weak references.
pub(super) fn build_related_symbol(node: &RepoGraphNode, confidence: f64) -> RelatedSymbol {
    let file_path = node
        .file_path
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| match &node.id {
            RepoNodeKey::File(p) => Some(p.display().to_string()),
            RepoNodeKey::Symbol(_) => None,
        });
    RelatedSymbol {
        uid: format_node_key(&node.id),
        name: node.display_name.clone(),
        kind: kind_label(node),
        file_path,
        confidence,
    }
}

/// Public-in-crate wrapper around the existing `kind_label` so the
/// `context` op picks up identical labels as the resolver.
pub(super) fn kind_label_for_node(node: &RepoGraphNode) -> String {
    kind_label(node)
}

/// Build `MethodMeta` from a node's structured signature parts. Returns
/// `None` whenever SCIP did not populate `signature_parts` — per the
/// plan contract, never regex the markdown signature blob.
pub(super) fn build_method_metadata(node: &RepoGraphNode) -> Option<MethodMeta> {
    let parts = node.signature_parts.as_ref()?;
    let params: Vec<MethodParam> = parts
        .parameters
        .iter()
        .map(|p| MethodParam {
            name: p.name.clone(),
            type_name: p.type_name.clone(),
            default_value: p.default_value.clone(),
        })
        .collect();
    Some(MethodMeta {
        visibility: parts.visibility.clone(),
        is_async: parts.is_async,
        params,
        return_type: parts.return_type.clone(),
        annotations: parts.annotations.clone(),
    })
}

/// Best-effort body-content read for `code_graph context include_content=true`.
/// Reads `start_line..=end_line` from the project clone, with a 64KiB
/// cap so a runaway range cannot blow out the wire size. Failures
/// (missing file, IO error, out-of-range lines) collapse to `None` —
/// the caller treats the field as advisory.
pub(super) fn read_symbol_content(
    clone_path: &str,
    file_path: &str,
    start_line: u32,
    end_line: u32,
) -> Option<String> {
    if start_line == 0 {
        return None;
    }
    let abs_path = std::path::Path::new(clone_path).join(file_path);
    let body = std::fs::read_to_string(&abs_path).ok()?;
    let start = start_line.saturating_sub(1) as usize;
    let end = end_line.max(start_line).saturating_sub(1) as usize;
    let lines: Vec<&str> = body.lines().collect();
    if start >= lines.len() {
        return None;
    }
    let upper = (end + 1).min(lines.len());
    let slice = lines[start..upper].join("\n");
    // 64 KiB hard cap — a worst-case 800-line method at 80 cols ≈ 64KB.
    const MAX: usize = 64 * 1024;
    if slice.len() > MAX {
        Some(slice[..MAX].to_string())
    } else {
        Some(slice)
    }
}
