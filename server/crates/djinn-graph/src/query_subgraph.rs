//! Budgeted natural-language subgraph planning/traversal for `code_graph query_subgraph`.
//!
//! This module stays at the graph layer: it accepts already-warmed
//! [`RepoDependencyGraph`] data, selects a small deterministic seed set from a
//! natural-language query, infers useful edge constraints from query wording,
//! and traverses a hub-avoiding bounded subgraph before any downstream JSON/text
//! rendering can overflow tool-result limits.
//!
//! Seed selection is intentionally pluggable. Control-plane callers that have
//! the hybrid lexical + semantic/RRF code-chunk search available should pass a
//! [`QuerySubgraphSeedProvider`] backed by that plumbing. The graph-local default
//! is only a structural name-search fallback for tests/offline graph use; it is
//! not an IDF-only selector and does not compete with richer hybrid indexes.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::Path;

use petgraph::Direction::{Incoming, Outgoing};
use petgraph::graph::{EdgeIndex, NodeIndex};
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};

use crate::repo_graph::{
    RepoDependencyGraph, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
};

const DEFAULT_TOKEN_BUDGET: usize = 2_000;
const DEFAULT_MAX_SEEDS: usize = 6;
const DEFAULT_MAX_DEPTH: usize = 2;
const DEFAULT_MIN_HUB_DEGREE: usize = 32;
const DEFAULT_SEED_FETCH_LIMIT: usize = 32;

/// Graph-layer inputs for a natural-language, budget-bounded subgraph query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySubgraphParams {
    /// Natural-language question or short search phrase.
    pub query: String,
    /// Optional workspace slug. Nodes whose `workspace` does not match are
    /// ignored when the field is populated on graph nodes.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Optional coarse context hint. At graph-layer this is treated as a
    /// case-insensitive substring over UID/display/file/workspace so callers can
    /// narrow without a separate filter language.
    #[serde(default)]
    pub context_filter: Option<String>,
    /// Optional path substring/glob-ish filter. Current graph APIs mostly expose
    /// simple string filters, so this intentionally maps cleanly to substring
    /// matching on `file_path`.
    #[serde(default)]
    pub file_filter: Option<String>,
    /// Optional node kind filter for seed and traversal candidates.
    #[serde(default)]
    pub kind_filter: Option<RepoGraphNodeKind>,
    /// Optional explicit edge kinds. When empty, edge intent is inferred from
    /// [`Self::query`] and falls back to a broad safe traversal.
    #[serde(default)]
    pub edge_filter: Vec<RepoGraphEdgeKind>,
    /// Approximate response token budget applied at the source.
    #[serde(default)]
    pub token_budget: Option<usize>,
    /// BFS depth from selected seeds. Defaults to 2.
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// Maximum seed nodes to traverse from. Defaults to 6.
    #[serde(default)]
    pub max_seeds: Option<usize>,
    /// Minimum degree before a p99 node is treated as a hub. Defaults to 32;
    /// tests may lower this for tiny fixtures.
    #[serde(default)]
    pub min_hub_degree: Option<usize>,
}

impl QuerySubgraphParams {
    fn token_budget(&self) -> usize {
        self.token_budget.unwrap_or(DEFAULT_TOKEN_BUDGET).max(64)
    }

    fn max_seeds(&self) -> usize {
        self.max_seeds.unwrap_or(DEFAULT_MAX_SEEDS).max(1)
    }

    fn max_depth(&self) -> usize {
        self.max_depth.unwrap_or(DEFAULT_MAX_DEPTH)
    }

    fn min_hub_degree(&self) -> usize {
        self.min_hub_degree.unwrap_or(DEFAULT_MIN_HUB_DEGREE).max(1)
    }
}

/// Seed candidate supplied by either hybrid search plumbing or the structural
/// fallback. `node_uid` is optional so control-plane seed providers may pass the
/// exact UID they already returned; graph traversal resolves by UID when present
/// and otherwise uses `node_index`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuerySubgraphSeed {
    pub node_index: NodeIndex,
    pub node_uid: Option<String>,
    pub score: f64,
    pub source: SeedSource,
    pub matched_text: Option<String>,
    pub debug: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedSource {
    Hybrid,
    Lexical,
    Semantic,
    Structural,
}

/// Pluggable seed-selection seam. Production callers should back this with the
/// existing hybrid lexical/vector/RRF search path when code chunks are ready.
pub trait QuerySubgraphSeedProvider {
    fn select_seeds(
        &self,
        graph: &RepoDependencyGraph,
        params: &QuerySubgraphParams,
        limit: usize,
    ) -> Vec<QuerySubgraphSeed>;
}

/// Graph-local structural fallback for offline/tests. Prefer passing a hybrid
/// provider whenever lexical/vector/RRF indexes are available.
#[derive(Debug, Default, Clone, Copy)]
pub struct GraphNameSeedProvider;

impl QuerySubgraphSeedProvider for GraphNameSeedProvider {
    fn select_seeds(
        &self,
        graph: &RepoDependencyGraph,
        params: &QuerySubgraphParams,
        limit: usize,
    ) -> Vec<QuerySubgraphSeed> {
        graph
            .search_by_name(&params.query, params.kind_filter, limit)
            .into_iter()
            .map(|hit| QuerySubgraphSeed {
                node_index: hit.node_index,
                node_uid: Some(stable_node_uid(graph.node(hit.node_index))),
                score: hit.score,
                source: SeedSource::Structural,
                matched_text: Some(graph.node(hit.node_index).display_name.clone()),
                debug: vec![
                    "structural name-index fallback; prefer hybrid provider when available"
                        .to_string(),
                ],
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuerySubgraphResult {
    pub query: String,
    pub nodes: Vec<QuerySubgraphNode>,
    pub edges: Vec<QuerySubgraphEdge>,
    pub seeds: Vec<QuerySubgraphSeedDebug>,
    pub inferred_edge_kinds: Vec<RepoGraphEdgeKind>,
    pub budget: QuerySubgraphBudget,
    pub traversal: QuerySubgraphTraversalDebug,
    pub narrowing_hints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySubgraphNode {
    pub uid: String,
    pub kind: RepoGraphNodeKind,
    pub display_name: String,
    pub file_path: Option<String>,
    pub workspace: Option<String>,
    pub is_seed: bool,
    pub is_hub: bool,
    pub degree: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuerySubgraphEdge {
    pub from_uid: String,
    pub to_uid: String,
    pub kind: RepoGraphEdgeKind,
    pub confidence: f64,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuerySubgraphSeedDebug {
    pub uid: String,
    pub display_name: String,
    pub score: f64,
    pub source: SeedSource,
    pub matched_text: Option<String>,
    pub debug: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySubgraphBudget {
    pub requested_tokens: usize,
    pub estimated_tokens: usize,
    pub truncated: bool,
    pub omitted_nodes: usize,
    pub omitted_edges: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuerySubgraphTraversalDebug {
    pub max_depth: usize,
    pub hub_degree_threshold: usize,
    pub hubs_blocked: Vec<String>,
    pub skipped_edge_kinds: Vec<RepoGraphEdgeKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetItem {
    Node(NodeIndex),
    Edge(EdgeIndex, NodeIndex, NodeIndex),
}

impl RepoDependencyGraph {
    /// Plan and traverse a bounded subgraph for a natural-language query.
    ///
    /// Pass `Some(provider)` when hybrid lexical/vector/RRF search is available;
    /// otherwise a structural graph-name fallback keeps the graph layer usable in
    /// tests and warmed-graph-only contexts.
    pub fn query_subgraph(
        &self,
        params: QuerySubgraphParams,
        seed_provider: Option<&dyn QuerySubgraphSeedProvider>,
    ) -> QuerySubgraphResult {
        let edge_kinds = if params.edge_filter.is_empty() {
            infer_edge_intent(&params.query)
        } else {
            dedup_edge_kinds(params.edge_filter.clone())
        };
        let edge_filter: BTreeSet<RepoGraphEdgeKind> = edge_kinds.iter().copied().collect();
        let hub_threshold = hub_degree_threshold(self, params.min_hub_degree());
        let hub_nodes: HashSet<NodeIndex> = self
            .graph()
            .node_indices()
            .filter(|&idx| degree(self, idx) >= hub_threshold)
            .collect();

        let provider = seed_provider.unwrap_or(&GraphNameSeedProvider);
        let raw_seeds = provider.select_seeds(
            self,
            &params,
            params.max_seeds().max(DEFAULT_SEED_FETCH_LIMIT),
        );
        let seeds = normalize_seeds(self, raw_seeds, &params);
        let seed_set: HashSet<NodeIndex> = seeds.iter().map(|s| s.node_index).collect();

        let mut item_order: Vec<BudgetItem> = Vec::new();
        let mut queued: HashSet<NodeIndex> = HashSet::new();
        let mut seen_nodes: HashSet<NodeIndex> = HashSet::new();
        let mut seen_edges: HashSet<EdgeIndex> = HashSet::new();
        let mut queue: VecDeque<(NodeIndex, usize)> = VecDeque::new();
        let mut hubs_blocked: BTreeSet<String> = BTreeSet::new();
        let mut skipped_edge_kinds: BTreeSet<RepoGraphEdgeKind> = BTreeSet::new();

        for seed in &seeds {
            if seen_nodes.insert(seed.node_index) {
                item_order.push(BudgetItem::Node(seed.node_index));
            }
            if queued.insert(seed.node_index) {
                queue.push_back((seed.node_index, 0));
            }
        }

        while let Some((node_idx, depth)) = queue.pop_front() {
            if depth >= params.max_depth() {
                continue;
            }
            let is_seed = seed_set.contains(&node_idx);
            if hub_nodes.contains(&node_idx) && !is_seed {
                hubs_blocked.insert(stable_node_uid(self.node(node_idx)));
                continue;
            }

            for direction in [Outgoing, Incoming] {
                for edge in self.graph().edges_directed(node_idx, direction) {
                    let edge_idx = edge.id();
                    let edge_kind = edge.weight().kind;
                    if !edge_filter.is_empty() && !edge_filter.contains(&edge_kind) {
                        skipped_edge_kinds.insert(edge_kind);
                        continue;
                    }
                    let source = edge.source();
                    let target = edge.target();
                    let other = if direction == Outgoing {
                        target
                    } else {
                        source
                    };
                    if !node_allowed(self.node(other), &params) {
                        continue;
                    }

                    if seen_nodes.insert(other) {
                        item_order.push(BudgetItem::Node(other));
                    }
                    if seen_edges.insert(edge_idx) {
                        item_order.push(BudgetItem::Edge(edge_idx, source, target));
                    }
                    if depth + 1 < params.max_depth() {
                        if hub_nodes.contains(&other) {
                            hubs_blocked.insert(stable_node_uid(self.node(other)));
                        } else if queued.insert(other) {
                            queue.push_back((other, depth + 1));
                        }
                    }
                }
            }
        }

        let budgeted = apply_budget(self, &item_order, params.token_budget());
        let final_node_set: HashSet<NodeIndex> = budgeted.nodes.iter().copied().collect();
        let final_edge_items: Vec<_> = budgeted
            .edges
            .iter()
            .copied()
            .filter(|(_, source, target)| {
                final_node_set.contains(source) && final_node_set.contains(target)
            })
            .collect();

        let nodes = budgeted
            .nodes
            .iter()
            .map(|&idx| render_node(self, idx, seed_set.contains(&idx), hub_nodes.contains(&idx)))
            .collect();
        let edges = final_edge_items
            .iter()
            .map(|&(edge_idx, source, target)| render_edge(self, edge_idx, source, target))
            .collect();

        let omitted_edges = budgeted
            .omitted_edges
            .saturating_add(budgeted.edges.len().saturating_sub(final_edge_items.len()));

        let mut narrowing_hints = Vec::new();
        if budgeted.truncated || omitted_edges > 0 {
            narrowing_hints.push(
                "reduce token_budget pressure with a smaller max_depth or max_seeds".to_string(),
            );
        }
        if params.workspace.is_none() {
            narrowing_hints.push(
                "narrow with workspace/context_filter when the repository has multiple workspaces"
                    .to_string(),
            );
        }
        if params.file_filter.is_none() {
            narrowing_hints
                .push("add file_filter to focus traversal on a package or directory".to_string());
        }
        if params.edge_filter.is_empty() && edge_kinds.len() > 3 {
            narrowing_hints.push(
                "ask about calls, reads, writes, implements, or imports to narrow edge kinds"
                    .to_string(),
            );
        }
        if !hubs_blocked.is_empty() {
            narrowing_hints.push("high-degree hub nodes were included but not expanded; query a returned UID directly for hub neighbors".to_string());
        }

        QuerySubgraphResult {
            query: params.query.clone(),
            nodes,
            edges,
            seeds: seeds
                .iter()
                .map(|seed| QuerySubgraphSeedDebug {
                    uid: stable_node_uid(self.node(seed.node_index)),
                    display_name: self.node(seed.node_index).display_name.clone(),
                    score: seed.score,
                    source: seed.source,
                    matched_text: seed.matched_text.clone(),
                    debug: seed.debug.clone(),
                })
                .collect(),
            inferred_edge_kinds: edge_kinds,
            budget: QuerySubgraphBudget {
                requested_tokens: params.token_budget(),
                estimated_tokens: budgeted.estimated_tokens,
                truncated: budgeted.truncated || omitted_edges > 0,
                omitted_nodes: budgeted.omitted_nodes,
                omitted_edges,
            },
            traversal: QuerySubgraphTraversalDebug {
                max_depth: params.max_depth(),
                hub_degree_threshold: hub_threshold,
                hubs_blocked: hubs_blocked.into_iter().collect(),
                skipped_edge_kinds: skipped_edge_kinds.into_iter().collect(),
            },
            narrowing_hints,
        }
    }
}

/// Stable UID string used by existing graph follow-up operations.
pub fn stable_node_uid(node: &RepoGraphNode) -> String {
    match &node.id {
        RepoNodeKey::File(path) => format!("file:{}", path.display()),
        RepoNodeKey::Symbol(symbol) => format!("symbol:{symbol}"),
        RepoNodeKey::Process(id) => format!("process:{id}"),
        RepoNodeKey::Table(name) => format!("table:{name}"),
        // PR s6ch / cs4v: route / tool nodes are synthetic side-channel
        // metadata, but the stable uid keeps `stable_node_uid`
        // exhaustive across the new key variants. Prefix mirrors
        // `process:` / `table:` so the resulting string stays
        // parseable by downstream consumers that split on the first
        // colon.
        RepoNodeKey::Route(id) => format!("route:{id}"),
        RepoNodeKey::Tool(id) => format!("tool:{id}"),
    }
}

pub fn infer_edge_intent(query: &str) -> Vec<RepoGraphEdgeKind> {
    let q = query.to_ascii_lowercase();
    let mut kinds = Vec::new();
    let mut add = |kind| {
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    };

    if contains_any(
        &q,
        &[
            "call", "calls", "caller", "callee", "invoke", "invokes", "use ", "uses ",
        ],
    ) {
        add(RepoGraphEdgeKind::SymbolReference);
        add(RepoGraphEdgeKind::Reads);
        add(RepoGraphEdgeKind::Writes);
    }
    if contains_any(
        &q,
        &[
            "import", "imports", "include", "includes", "require", "requires", "module",
        ],
    ) {
        add(RepoGraphEdgeKind::FileReference);
        add(RepoGraphEdgeKind::DeclaredInFile);
        add(RepoGraphEdgeKind::ContainsDefinition);
    }
    if contains_any(&q, &["return", "returns", "type", "types", "defines type"]) {
        add(RepoGraphEdgeKind::TypeDefines);
        add(RepoGraphEdgeKind::Defines);
    }
    if contains_any(&q, &["read", "reads", "select", "loads", "fetches"]) {
        add(RepoGraphEdgeKind::Reads);
    }
    if contains_any(
        &q,
        &[
            "write", "writes", "update", "updates", "insert", "inserts", "mutate", "mutates",
            "delete", "deletes",
        ],
    ) {
        add(RepoGraphEdgeKind::Writes);
    }
    if contains_any(
        &q,
        &[
            "implement",
            "implements",
            "implementation",
            "trait",
            "interface",
        ],
    ) {
        add(RepoGraphEdgeKind::Implements);
    }
    if contains_any(
        &q,
        &[
            "extend",
            "extends",
            "inherit",
            "inherits",
            "subclass",
            "superclass",
        ],
    ) {
        add(RepoGraphEdgeKind::Extends);
    }

    if kinds.is_empty() {
        // Broad but safe default: no process/community synthetic expansion and
        // no unbounded all-edge fanout. This covers code dependency, file, data
        // access, and type relationship edges.
        kinds = vec![
            RepoGraphEdgeKind::SymbolReference,
            RepoGraphEdgeKind::Reads,
            RepoGraphEdgeKind::Writes,
            RepoGraphEdgeKind::FileReference,
            RepoGraphEdgeKind::DeclaredInFile,
            RepoGraphEdgeKind::ContainsDefinition,
            RepoGraphEdgeKind::Implements,
            RepoGraphEdgeKind::Extends,
            RepoGraphEdgeKind::TypeDefines,
            RepoGraphEdgeKind::Defines,
        ];
    }
    kinds
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn dedup_edge_kinds(kinds: Vec<RepoGraphEdgeKind>) -> Vec<RepoGraphEdgeKind> {
    let mut out = Vec::new();
    for kind in kinds {
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    out
}

fn normalize_seeds(
    graph: &RepoDependencyGraph,
    raw: Vec<QuerySubgraphSeed>,
    params: &QuerySubgraphParams,
) -> Vec<QuerySubgraphSeed> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for mut seed in raw {
        if graph.graph().node_weight(seed.node_index).is_none() {
            continue;
        }
        if !node_allowed(graph.node(seed.node_index), params) {
            continue;
        }
        if !seen.insert(seed.node_index) {
            continue;
        }
        let actual_uid = stable_node_uid(graph.node(seed.node_index));
        if let Some(uid) = &seed.node_uid
            && uid != &actual_uid
        {
            seed.debug
                .push(format!("seed uid normalized from {uid} to {actual_uid}"));
        }
        seed.node_uid = Some(actual_uid);
        out.push(seed);
        if out.len() >= params.max_seeds() {
            break;
        }
    }
    out.sort_by(|a, b| {
        b.score.total_cmp(&a.score).then_with(|| {
            stable_node_uid(graph.node(a.node_index))
                .cmp(&stable_node_uid(graph.node(b.node_index)))
        })
    });
    out
}

fn node_allowed(node: &RepoGraphNode, params: &QuerySubgraphParams) -> bool {
    if let Some(kind) = params.kind_filter
        && node.kind != kind
    {
        return false;
    }
    if let Some(workspace) = params.workspace.as_deref()
        && let Some(node_workspace) = node.workspace.as_deref()
        && node_workspace != workspace
    {
        return false;
    }
    if let Some(file_filter) = params.file_filter.as_deref() {
        let file = node.file_path.as_ref().map(|p| p.to_string_lossy());
        if !file.as_deref().unwrap_or_default().contains(file_filter) {
            return false;
        }
    }
    if let Some(context) = params.context_filter.as_deref() {
        let context = context.to_ascii_lowercase();
        let uid = stable_node_uid(node).to_ascii_lowercase();
        let display = node.display_name.to_ascii_lowercase();
        let file = node
            .file_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let workspace = node
            .workspace
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !uid.contains(&context)
            && !display.contains(&context)
            && !file.contains(&context)
            && !workspace.contains(&context)
        {
            return false;
        }
    }
    true
}

fn degree(graph: &RepoDependencyGraph, idx: NodeIndex) -> usize {
    graph.graph().edges_directed(idx, Incoming).count()
        + graph.graph().edges_directed(idx, Outgoing).count()
}

fn hub_degree_threshold(graph: &RepoDependencyGraph, min_hub_degree: usize) -> usize {
    let mut degrees: Vec<usize> = graph
        .graph()
        .node_indices()
        .map(|idx| degree(graph, idx))
        .collect();
    if degrees.is_empty() {
        return min_hub_degree;
    }
    degrees.sort_unstable();
    let idx = ((degrees.len() as f64 * 0.99).ceil() as usize).saturating_sub(1);
    degrees[idx.min(degrees.len() - 1)].max(min_hub_degree)
}

struct BudgetedItems {
    nodes: Vec<NodeIndex>,
    edges: Vec<(EdgeIndex, NodeIndex, NodeIndex)>,
    estimated_tokens: usize,
    truncated: bool,
    omitted_nodes: usize,
    omitted_edges: usize,
}

fn apply_budget(graph: &RepoDependencyGraph, items: &[BudgetItem], budget: usize) -> BudgetedItems {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut estimated_tokens = 0usize;
    let mut truncated = false;
    let mut omitted_nodes = 0usize;
    let mut omitted_edges = 0usize;

    for item in items {
        let item_tokens = match *item {
            BudgetItem::Node(idx) => estimate_node_tokens(graph.node(idx)),
            BudgetItem::Edge(edge_idx, _, _) => graph
                .graph()
                .edge_weight(edge_idx)
                .map(estimate_edge_tokens)
                .unwrap_or(1),
        };
        if estimated_tokens.saturating_add(item_tokens) > budget {
            truncated = true;
            match item {
                BudgetItem::Node(_) => omitted_nodes += 1,
                BudgetItem::Edge(_, _, _) => omitted_edges += 1,
            }
            continue;
        }
        estimated_tokens += item_tokens;
        match *item {
            BudgetItem::Node(idx) => nodes.push(idx),
            BudgetItem::Edge(edge_idx, source, target) => edges.push((edge_idx, source, target)),
        }
    }

    BudgetedItems {
        nodes,
        edges,
        estimated_tokens,
        truncated,
        omitted_nodes,
        omitted_edges,
    }
}

fn estimate_node_tokens(node: &RepoGraphNode) -> usize {
    let mut chars = stable_node_uid(node).len() + node.display_name.len() + 16;
    chars += node
        .file_path
        .as_deref()
        .map(path_chars)
        .unwrap_or_default();
    chars += node
        .signature
        .as_ref()
        .map(String::len)
        .unwrap_or_default()
        .min(160);
    chars.div_ceil(4).max(8)
}

fn path_chars(path: &Path) -> usize {
    path.to_string_lossy().len()
}

fn estimate_edge_tokens(edge: &crate::repo_graph::RepoGraphEdge) -> usize {
    let reason = edge
        .reason
        .as_ref()
        .map(String::len)
        .unwrap_or_default()
        .min(80);
    (24 + reason).div_ceil(4).max(6)
}

fn render_node(
    graph: &RepoDependencyGraph,
    idx: NodeIndex,
    is_seed: bool,
    is_hub: bool,
) -> QuerySubgraphNode {
    let node = graph.node(idx);
    QuerySubgraphNode {
        uid: stable_node_uid(node),
        kind: node.kind,
        display_name: node.display_name.clone(),
        file_path: node.file_path.as_ref().map(|p| p.display().to_string()),
        workspace: node.workspace.clone(),
        is_seed,
        is_hub,
        degree: degree(graph, idx),
    }
}

fn render_edge(
    graph: &RepoDependencyGraph,
    edge_idx: EdgeIndex,
    source: NodeIndex,
    target: NodeIndex,
) -> QuerySubgraphEdge {
    let edge = graph
        .graph()
        .edge_weight(edge_idx)
        .expect("edge index captured from graph traversal should still exist");
    QuerySubgraphEdge {
        from_uid: stable_node_uid(graph.node(source)),
        to_uid: stable_node_uid(graph.node(target)),
        kind: edge.kind,
        confidence: edge.confidence,
        reason: edge.reason.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_graph::{RepoGraphEdge, edge_confidence_floor, edge_weight_for};
    use petgraph::graph::NodeIndex;
    use std::path::PathBuf;

    fn test_node(name: &str) -> RepoGraphNode {
        RepoGraphNode {
            id: RepoNodeKey::Symbol(format!("scip-test pkg `{name}`().")),
            kind: RepoGraphNodeKind::Symbol,
            display_name: name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(format!("src/{name}.rs"))),
            symbol: Some(format!("scip-test pkg `{name}`().")),
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: Some(format!("fn {name}()")),
            documentation: Vec::new(),
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: Some("root".to_string()),
            // PR s6ch / cs4v: route metadata is not applicable to
            // these placeholder symbol nodes — defaults to `None`.
            route_framework: None,
            route_handler_symbol: None,
        }
    }

    fn edge(kind: RepoGraphEdgeKind) -> RepoGraphEdge {
        RepoGraphEdge {
            kind,
            weight: edge_weight_for(kind),
            evidence_count: 1,
            confidence: edge_confidence_floor(kind),
            reason: Some("fixture".to_string()),
            step: None,
        }
    }

    fn fixture_graph() -> RepoDependencyGraph {
        let mut graph = RepoDependencyGraph::build(&[]);
        let g = graph.graph_mut_unchecked();
        let seed = g.add_node(test_node("seed"));
        let hub = g.add_node(test_node("hub"));
        let leaf = g.add_node(test_node("leaf"));
        let read = g.add_node(test_node("read_target"));
        let write = g.add_node(test_node("write_target"));
        g.add_edge(seed, hub, edge(RepoGraphEdgeKind::SymbolReference));
        g.add_edge(hub, leaf, edge(RepoGraphEdgeKind::SymbolReference));
        g.add_edge(seed, read, edge(RepoGraphEdgeKind::Reads));
        g.add_edge(seed, write, edge(RepoGraphEdgeKind::Writes));
        // Make hub high-degree for a tiny graph when min_hub_degree=3.
        for i in 0..3 {
            let spoke = g.add_node(test_node(&format!("spoke{i}")));
            g.add_edge(hub, spoke, edge(RepoGraphEdgeKind::SymbolReference));
        }
        graph
    }

    struct FixedSeeds(Vec<QuerySubgraphSeed>);

    impl QuerySubgraphSeedProvider for FixedSeeds {
        fn select_seeds(
            &self,
            _graph: &RepoDependencyGraph,
            _params: &QuerySubgraphParams,
            _limit: usize,
        ) -> Vec<QuerySubgraphSeed> {
            self.0.clone()
        }
    }

    fn seed_provider(idx: usize, source: SeedSource) -> FixedSeeds {
        FixedSeeds(vec![QuerySubgraphSeed {
            node_index: NodeIndex::new(idx),
            node_uid: None,
            score: 42.0,
            source,
            matched_text: Some("seed match".to_string()),
            debug: vec!["rrf fused lexical+semantic+structural".to_string()],
        }])
    }

    fn params(query: &str) -> QuerySubgraphParams {
        QuerySubgraphParams {
            query: query.to_string(),
            workspace: None,
            context_filter: None,
            file_filter: None,
            kind_filter: None,
            edge_filter: Vec::new(),
            token_budget: Some(2_000),
            max_depth: Some(3),
            max_seeds: Some(4),
            min_hub_degree: Some(3),
        }
    }

    #[test]
    fn edge_intent_inference_maps_natural_language_to_edge_filters() {
        assert_eq!(
            infer_edge_intent("who writes the users table"),
            vec![RepoGraphEdgeKind::Writes]
        );
        assert!(
            infer_edge_intent("what implements this trait")
                .contains(&RepoGraphEdgeKind::Implements)
        );
        assert!(
            infer_edge_intent("imports from router").contains(&RepoGraphEdgeKind::FileReference)
        );
        assert!(
            infer_edge_intent("what returns this type").contains(&RepoGraphEdgeKind::TypeDefines)
        );
        assert!(
            infer_edge_intent("show related auth code")
                .contains(&RepoGraphEdgeKind::SymbolReference)
        );
    }

    #[test]
    fn seed_debug_metadata_preserves_hybrid_source_and_reason() {
        let graph = fixture_graph();
        let result =
            graph.query_subgraph(params("seed"), Some(&seed_provider(0, SeedSource::Hybrid)));
        assert_eq!(result.seeds.len(), 1);
        assert_eq!(result.seeds[0].source, SeedSource::Hybrid);
        assert_eq!(result.seeds[0].score, 42.0);
        assert!(result.seeds[0].debug[0].contains("rrf"));
        assert!(result.seeds[0].uid.starts_with("symbol:scip-test"));
    }

    #[test]
    fn hub_avoidance_includes_hub_but_does_not_expand_through_it() {
        let graph = fixture_graph();
        let mut p = params("who calls seed");
        p.edge_filter = vec![RepoGraphEdgeKind::SymbolReference];
        let result = graph.query_subgraph(p, Some(&seed_provider(0, SeedSource::Hybrid)));
        let names: BTreeSet<_> = result
            .nodes
            .iter()
            .map(|n| n.display_name.as_str())
            .collect();
        assert!(names.contains("seed"));
        assert!(
            names.contains("hub"),
            "direct hub should appear as bounded result node"
        );
        assert!(!names.contains("leaf"), "hub expansion should be blocked");
        assert!(!result.traversal.hubs_blocked.is_empty());
    }

    #[test]
    fn budget_truncation_happens_at_source_and_emits_hints() {
        let graph = fixture_graph();
        let mut p = params("seed");
        p.token_budget = Some(80);
        let result = graph.query_subgraph(p, Some(&seed_provider(0, SeedSource::Hybrid)));
        assert!(result.budget.truncated);
        assert!(result.budget.estimated_tokens <= result.budget.requested_tokens);
        assert!(!result.narrowing_hints.is_empty());
        assert!(result.budget.omitted_nodes + result.budget.omitted_edges > 0);
    }

    #[test]
    fn edge_filter_limits_traversal_to_inferred_reads() {
        let graph = fixture_graph();
        let result = graph.query_subgraph(
            params("who reads seed"),
            Some(&seed_provider(0, SeedSource::Hybrid)),
        );
        assert_eq!(result.inferred_edge_kinds, vec![RepoGraphEdgeKind::Reads]);
        assert!(
            result
                .edges
                .iter()
                .all(|e| e.kind == RepoGraphEdgeKind::Reads)
        );
        let names: BTreeSet<_> = result
            .nodes
            .iter()
            .map(|n| n.display_name.as_str())
            .collect();
        assert!(names.contains("read_target"));
        assert!(!names.contains("write_target"));
    }
}
