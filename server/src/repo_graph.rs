use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use petgraph::Direction::{Incoming, Outgoing};
use petgraph::algo::page_rank;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef as PetgraphEdgeRef;
use serde::{Deserialize, Serialize};

use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipOccurrence, ScipRelationship, ScipRelationshipKind, ScipSymbol,
    ScipSymbolKind,
};

const PAGE_RANK_DAMPING_FACTOR: f64 = 0.85;
const PAGE_RANK_ITERATIONS: usize = 25;
const EDGE_WEIGHT_DEFINITION_TO_FILE: f64 = 4.0;
const EDGE_WEIGHT_FILE_TO_DEFINITION: f64 = 1.5;
const EDGE_WEIGHT_FILE_REFERENCE: f64 = 2.5;
const EDGE_WEIGHT_SYMBOL_REFERENCE: f64 = 3.5;
const EDGE_WEIGHT_SYMBOL_RELATIONSHIP_REFERENCE: f64 = 2.0;
const EDGE_WEIGHT_SYMBOL_RELATIONSHIP_IMPLEMENTATION: f64 = 2.5;
const EDGE_WEIGHT_SYMBOL_RELATIONSHIP_TYPE_DEFINITION: f64 = 1.75;
const EDGE_WEIGHT_SYMBOL_RELATIONSHIP_DEFINITION: f64 = 2.25;
const SYMBOL_KIND_TYPE_MULTIPLIER: f64 = 1.15;
const SYMBOL_KIND_METHOD_MULTIPLIER: f64 = 1.05;
const SYMBOL_KIND_FUNCTION_MULTIPLIER: f64 = 1.0;
const SYMBOL_KIND_VARIABLE_MULTIPLIER: f64 = 0.7;
const SYMBOL_KIND_DEFAULT_MULTIPLIER: f64 = 0.9;

/// Stable, reusable repository dependency graph built from normalized SCIP parse output.
#[derive(Debug, Clone)]
pub struct RepoDependencyGraph {
    graph: DiGraph<RepoGraphNode, RepoGraphEdge>,
    node_lookup: BTreeMap<RepoNodeKey, NodeIndex>,
}

impl RepoDependencyGraph {
    pub fn build(indices: &[ParsedScipIndex]) -> Self {
        let mut builder = RepoDependencyGraphBuilder::default();
        for index in indices {
            builder.add_index(index);
        }
        builder.finish()
    }

    pub fn graph(&self) -> &DiGraph<RepoGraphNode, RepoGraphEdge> {
        &self.graph
    }

    pub fn node(&self, index: NodeIndex) -> &RepoGraphNode {
        &self.graph[index]
    }

    pub fn edge(&self, edge_index: petgraph::graph::EdgeIndex) -> &RepoGraphEdge {
        &self.graph[edge_index]
    }

    pub fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph.edge_count()
    }

    pub fn file_node(&self, path: impl AsRef<Path>) -> Option<NodeIndex> {
        self.node_lookup
            .get(&RepoNodeKey::File(path.as_ref().to_path_buf()))
            .copied()
    }

    pub fn symbol_node(&self, symbol: &str) -> Option<NodeIndex> {
        self.node_lookup
            .get(&RepoNodeKey::Symbol(symbol.to_string()))
            .copied()
    }

    pub fn rank(&self) -> RepoGraphRanking {
        let page_rank_scores =
            page_rank(&self.graph, PAGE_RANK_DAMPING_FACTOR, PAGE_RANK_ITERATIONS);

        let mut scored_nodes = Vec::with_capacity(self.graph.node_count());
        for node_index in self.graph.node_indices() {
            let node = &self.graph[node_index];
            let page_rank = page_rank_scores[node_index.index()];
            let structural_weight = self.structural_weight(node_index);
            let score = page_rank * structural_weight;
            scored_nodes.push(RankedRepoGraphNode {
                node_index,
                key: node.key(),
                kind: node.kind(),
                score,
                page_rank,
                structural_weight,
                inbound_edge_weight: self.total_edge_weight(node_index, Incoming),
                outbound_edge_weight: self.total_edge_weight(node_index, Outgoing),
            });
        }

        scored_nodes.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| right.page_rank.total_cmp(&left.page_rank))
                .then_with(|| right.structural_weight.total_cmp(&left.structural_weight))
                .then_with(|| left.key.cmp(&right.key))
        });

        RepoGraphRanking {
            nodes: scored_nodes,
        }
    }

    fn structural_weight(&self, node_index: NodeIndex) -> f64 {
        let node = &self.graph[node_index];
        let inbound_edge_weight = self.total_edge_weight(node_index, Incoming);
        let outbound_edge_weight = self.total_edge_weight(node_index, Outgoing);
        let degree_bonus = (inbound_edge_weight * 1.2) + (outbound_edge_weight * 0.8);
        node.intrinsic_weight() + degree_bonus
    }

    fn total_edge_weight(&self, node_index: NodeIndex, direction: petgraph::Direction) -> f64 {
        self.graph
            .edges_directed(node_index, direction)
            .map(|edge| edge.weight().weight)
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RepoNodeKey {
    File(PathBuf),
    Symbol(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoGraphNodeKind {
    File,
    Symbol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoGraphNode {
    pub id: RepoNodeKey,
    pub kind: RepoGraphNodeKind,
    pub display_name: String,
    pub language: Option<String>,
    pub file_path: Option<PathBuf>,
    pub symbol: Option<String>,
    pub symbol_kind: Option<ScipSymbolKind>,
    pub is_external: bool,
}

impl RepoGraphNode {
    pub fn key(&self) -> RepoNodeKey {
        self.id.clone()
    }

    pub fn kind(&self) -> RepoGraphNodeKind {
        self.kind
    }

    fn intrinsic_weight(&self) -> f64 {
        match self.kind {
            RepoGraphNodeKind::File => 1.0,
            RepoGraphNodeKind::Symbol => match self.symbol_kind {
                Some(ScipSymbolKind::Type)
                | Some(ScipSymbolKind::Struct)
                | Some(ScipSymbolKind::Interface)
                | Some(ScipSymbolKind::Enum) => SYMBOL_KIND_TYPE_MULTIPLIER,
                Some(ScipSymbolKind::Method) | Some(ScipSymbolKind::Constructor) => {
                    SYMBOL_KIND_METHOD_MULTIPLIER
                }
                Some(ScipSymbolKind::Function) => SYMBOL_KIND_FUNCTION_MULTIPLIER,
                Some(ScipSymbolKind::Variable)
                | Some(ScipSymbolKind::Field)
                | Some(ScipSymbolKind::Property)
                | Some(ScipSymbolKind::Constant) => SYMBOL_KIND_VARIABLE_MULTIPLIER,
                _ => SYMBOL_KIND_DEFAULT_MULTIPLIER,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoGraphEdgeKind {
    ContainsDefinition,
    DeclaredInFile,
    FileReference,
    SymbolReference,
    SymbolRelationshipReference,
    SymbolRelationshipImplementation,
    SymbolRelationshipTypeDefinition,
    SymbolRelationshipDefinition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoGraphEdge {
    pub kind: RepoGraphEdgeKind,
    pub weight: f64,
    pub evidence_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepoGraphRanking {
    pub nodes: Vec<RankedRepoGraphNode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedRepoGraphNode {
    pub node_index: NodeIndex,
    pub key: RepoNodeKey,
    pub kind: RepoGraphNodeKind,
    pub score: f64,
    pub page_rank: f64,
    pub structural_weight: f64,
    pub inbound_edge_weight: f64,
    pub outbound_edge_weight: f64,
}

#[derive(Default)]
struct RepoDependencyGraphBuilder {
    graph: DiGraph<RepoGraphNode, RepoGraphEdge>,
    node_lookup: BTreeMap<RepoNodeKey, NodeIndex>,
    edge_accumulator: BTreeMap<(NodeIndex, NodeIndex, RepoGraphEdgeKind), usize>,
    symbol_file: BTreeMap<String, PathBuf>,
    symbol_language: BTreeMap<String, String>,
    declared_symbols: BTreeSet<String>,
}

impl RepoDependencyGraphBuilder {
    fn add_index(&mut self, index: &ParsedScipIndex) {
        for external_symbol in &index.external_symbols {
            self.ensure_symbol_node(external_symbol, None, None, true);
        }

        for file in &index.files {
            self.add_file(file);
        }
    }

    fn add_file(&mut self, file: &ScipFile) {
        let file_index = self.ensure_file_node(&file.relative_path, &file.language);

        for symbol in &file.symbols {
            let symbol_index = self.ensure_symbol_node(
                symbol,
                Some(&file.relative_path),
                Some(&file.language),
                false,
            );
            self.symbol_file
                .insert(symbol.symbol.clone(), file.relative_path.clone());
            self.symbol_language
                .insert(symbol.symbol.clone(), file.language.clone());
            self.declared_symbols.insert(symbol.symbol.clone());
            self.bump_edge(
                symbol_index,
                file_index,
                RepoGraphEdgeKind::DeclaredInFile,
                1,
            );
            self.bump_edge(
                file_index,
                symbol_index,
                RepoGraphEdgeKind::ContainsDefinition,
                1,
            );

            for relationship in &symbol.relationships {
                self.add_relationship(symbol_index, relationship);
            }
        }

        for definition in &file.definitions {
            if let Some(symbol_index) = self.ensure_known_symbol_from_occurrence(definition, file) {
                self.bump_edge(
                    file_index,
                    symbol_index,
                    RepoGraphEdgeKind::ContainsDefinition,
                    1,
                );
                self.bump_edge(
                    symbol_index,
                    file_index,
                    RepoGraphEdgeKind::DeclaredInFile,
                    1,
                );
            }
        }

        for reference in &file.references {
            self.add_reference(file_index, file, reference);
        }
    }

    fn add_reference(
        &mut self,
        source_file_index: NodeIndex,
        file: &ScipFile,
        occurrence: &ScipOccurrence,
    ) {
        let symbol_index = self.ensure_symbol_node_from_occurrence(occurrence, file);
        self.bump_edge(
            source_file_index,
            symbol_index,
            RepoGraphEdgeKind::FileReference,
            1,
        );

        if let Some(target_file) = self.symbol_file.get(&occurrence.symbol).cloned() {
            let target_file_index = self.ensure_file_node(&target_file, file.language.as_str());
            self.bump_edge(
                source_file_index,
                target_file_index,
                RepoGraphEdgeKind::FileReference,
                1,
            );
            self.bump_edge(
                symbol_index,
                target_file_index,
                RepoGraphEdgeKind::SymbolReference,
                1,
            );
        }
    }

    fn add_relationship(
        &mut self,
        source_symbol_index: NodeIndex,
        relationship: &ScipRelationship,
    ) {
        let target_symbol_index = self.ensure_placeholder_symbol_node(&relationship.target_symbol);

        for kind in &relationship.kinds {
            let edge_kind = match kind {
                ScipRelationshipKind::Reference => RepoGraphEdgeKind::SymbolRelationshipReference,
                ScipRelationshipKind::Implementation => {
                    RepoGraphEdgeKind::SymbolRelationshipImplementation
                }
                ScipRelationshipKind::TypeDefinition => {
                    RepoGraphEdgeKind::SymbolRelationshipTypeDefinition
                }
                ScipRelationshipKind::Definition => RepoGraphEdgeKind::SymbolRelationshipDefinition,
            };
            self.bump_edge(source_symbol_index, target_symbol_index, edge_kind, 1);
        }
    }

    fn ensure_file_node(&mut self, path: &Path, language: &str) -> NodeIndex {
        let key = RepoNodeKey::File(path.to_path_buf());
        if let Some(index) = self.node_lookup.get(&key) {
            return *index;
        }

        let display_name = path.display().to_string();
        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::File,
            display_name,
            language: Some(language.to_string()),
            file_path: Some(path.to_path_buf()),
            symbol: None,
            symbol_kind: None,
            is_external: false,
        };
        let node_index = self.graph.add_node(node);
        self.node_lookup.insert(key, node_index);
        node_index
    }

    fn ensure_symbol_node(
        &mut self,
        symbol: &ScipSymbol,
        file_path: Option<&Path>,
        language: Option<&str>,
        is_external: bool,
    ) -> NodeIndex {
        let key = RepoNodeKey::Symbol(symbol.symbol.clone());
        if let Some(index) = self.node_lookup.get(&key) {
            return *index;
        }

        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::Symbol,
            display_name: symbol
                .display_name
                .clone()
                .unwrap_or_else(|| symbol.symbol.clone()),
            language: language.map(ToOwned::to_owned),
            file_path: file_path.map(Path::to_path_buf),
            symbol: Some(symbol.symbol.clone()),
            symbol_kind: symbol.kind.clone(),
            is_external,
        };
        let node_index = self.graph.add_node(node);
        self.node_lookup.insert(key, node_index);
        node_index
    }

    fn ensure_known_symbol_from_occurrence(
        &mut self,
        occurrence: &ScipOccurrence,
        file: &ScipFile,
    ) -> Option<NodeIndex> {
        self.declared_symbols
            .contains(&occurrence.symbol)
            .then(|| self.ensure_symbol_node_from_occurrence(occurrence, file))
    }

    fn ensure_symbol_node_from_occurrence(
        &mut self,
        occurrence: &ScipOccurrence,
        file: &ScipFile,
    ) -> NodeIndex {
        if let Some(index) = self
            .node_lookup
            .get(&RepoNodeKey::Symbol(occurrence.symbol.clone()))
            .copied()
        {
            return index;
        }

        let symbol = ScipSymbol {
            symbol: occurrence.symbol.clone(),
            kind: None,
            display_name: Some(occurrence.symbol.clone()),
            signature: None,
            documentation: Vec::new(),
            relationships: Vec::new(),
        };
        self.ensure_symbol_node(
            &symbol,
            Some(&file.relative_path),
            Some(&file.language),
            false,
        )
    }

    fn ensure_placeholder_symbol_node(&mut self, symbol: &str) -> NodeIndex {
        if let Some(index) = self
            .node_lookup
            .get(&RepoNodeKey::Symbol(symbol.to_string()))
            .copied()
        {
            return index;
        }

        let file_path = self.symbol_file.get(symbol).cloned();
        let language = self.symbol_language.get(symbol).cloned();
        let node = RepoGraphNode {
            id: RepoNodeKey::Symbol(symbol.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: symbol.to_string(),
            language,
            file_path,
            symbol: Some(symbol.to_string()),
            symbol_kind: None,
            is_external: !self.declared_symbols.contains(symbol),
        };
        let key = node.id.clone();
        let node_index = self.graph.add_node(node);
        self.node_lookup.insert(key, node_index);
        node_index
    }

    fn bump_edge(
        &mut self,
        source: NodeIndex,
        target: NodeIndex,
        kind: RepoGraphEdgeKind,
        count: usize,
    ) {
        *self
            .edge_accumulator
            .entry((source, target, kind))
            .or_default() += count;
    }

    fn finish(mut self) -> RepoDependencyGraph {
        for ((source, target, kind), evidence_count) in self.edge_accumulator {
            self.graph.add_edge(
                source,
                target,
                RepoGraphEdge {
                    kind,
                    weight: edge_weight(kind) * (evidence_count as f64),
                    evidence_count,
                },
            );
        }

        RepoDependencyGraph {
            graph: self.graph,
            node_lookup: self.node_lookup,
        }
    }
}

/// Minimal serializable artifact capturing the per-file and per-symbol graph
/// relationships needed for incremental changed-file patch planning.
///
/// This is persisted alongside the rendered repo-map cache so that later
/// operations can recover the dependency graph without re-parsing raw SCIP
/// outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoGraphArtifact {
    pub nodes: Vec<RepoGraphNode>,
    pub edges: Vec<RepoGraphArtifactEdge>,
}

/// A serializable directed edge between two graph nodes, identified by their
/// position in the `nodes` vec of the parent [`RepoGraphArtifact`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoGraphArtifactEdge {
    pub source: usize,
    pub target: usize,
    pub kind: RepoGraphEdgeKind,
    pub weight: f64,
    pub evidence_count: usize,
}

impl RepoDependencyGraph {
    /// Serialize the graph into a compact JSON artifact suitable for DB
    /// persistence.
    pub fn to_artifact(&self) -> RepoGraphArtifact {
        let mut index_map: BTreeMap<NodeIndex, usize> = BTreeMap::new();
        let mut nodes = Vec::with_capacity(self.graph.node_count());
        for (i, node_index) in self.graph.node_indices().enumerate() {
            index_map.insert(node_index, i);
            nodes.push(self.graph[node_index].clone());
        }

        let mut edges = Vec::with_capacity(self.graph.edge_count());
        for edge_ref in self.graph.edge_references() {
            let source = index_map[&edge_ref.source()];
            let target = index_map[&edge_ref.target()];
            let w = edge_ref.weight();
            edges.push(RepoGraphArtifactEdge {
                source,
                target,
                kind: w.kind,
                weight: w.weight,
                evidence_count: w.evidence_count,
            });
        }

        RepoGraphArtifact { nodes, edges }
    }

    /// Rebuild a `RepoDependencyGraph` from a previously persisted artifact.
    pub fn from_artifact(artifact: &RepoGraphArtifact) -> Self {
        let mut graph = DiGraph::new();
        let mut node_lookup = BTreeMap::new();
        let mut index_map = Vec::with_capacity(artifact.nodes.len());

        for node in &artifact.nodes {
            let node_index = graph.add_node(node.clone());
            node_lookup.insert(node.id.clone(), node_index);
            index_map.push(node_index);
        }

        for edge in &artifact.edges {
            graph.add_edge(
                index_map[edge.source],
                index_map[edge.target],
                RepoGraphEdge {
                    kind: edge.kind,
                    weight: edge.weight,
                    evidence_count: edge.evidence_count,
                },
            );
        }

        RepoDependencyGraph { graph, node_lookup }
    }

    /// Serialize the graph artifact to a JSON string for DB storage.
    pub fn serialize_artifact(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.to_artifact())
    }

    /// Deserialize a graph from a previously stored JSON artifact string.
    pub fn deserialize_artifact(json: &str) -> Result<Self, serde_json::Error> {
        let artifact: RepoGraphArtifact = serde_json::from_str(json)?;
        Ok(Self::from_artifact(&artifact))
    }

    /// Patch the graph by removing all contributions from `changed_files` and
    /// re-adding them from the supplied SCIP parse output.
    ///
    /// This is the core of the small-diff incremental path: instead of
    /// rebuilding the entire graph from scratch we strip the stale file/symbol
    /// nodes and edges, then replay only the changed files through the normal
    /// builder pipeline.
    ///
    /// The caller is responsible for ensuring `new_indices` contains parsed
    /// SCIP data for exactly the changed files (additional files are harmless
    /// but defeat the purpose).
    pub fn patch_changed_files(
        &self,
        changed_files: &BTreeSet<PathBuf>,
        new_indices: &[ParsedScipIndex],
    ) -> Self {
        // Step 1: Build a filtered artifact that excludes nodes owned by
        // changed files and any edges touching those nodes.
        let artifact = self.to_artifact();
        let removed_positions: BTreeSet<usize> = artifact
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| is_owned_by_changed_file(node, changed_files))
            .map(|(i, _)| i)
            .collect();

        // Collect surviving nodes and build old-position -> new-position map.
        let mut position_map: BTreeMap<usize, usize> = BTreeMap::new();
        let mut surviving_nodes = Vec::new();
        for (old_pos, node) in artifact.nodes.iter().enumerate() {
            if removed_positions.contains(&old_pos) {
                continue;
            }
            position_map.insert(old_pos, surviving_nodes.len());
            surviving_nodes.push(node.clone());
        }

        let surviving_edges: Vec<RepoGraphArtifactEdge> = artifact
            .edges
            .iter()
            .filter(|edge| {
                !removed_positions.contains(&edge.source)
                    && !removed_positions.contains(&edge.target)
            })
            .map(|edge| RepoGraphArtifactEdge {
                source: position_map[&edge.source],
                target: position_map[&edge.target],
                kind: edge.kind,
                weight: edge.weight,
                evidence_count: edge.evidence_count,
            })
            .collect();

        let filtered_artifact = RepoGraphArtifact {
            nodes: surviving_nodes,
            edges: surviving_edges,
        };

        // Step 2: Rebuild the base graph from the filtered artifact.
        // We use a builder so that the new SCIP data can link to existing
        // nodes (e.g. symbols defined in unchanged files that are referenced
        // by changed files).
        let base = Self::from_artifact(&filtered_artifact);
        let mut builder = RepoDependencyGraphBuilder {
            graph: base.graph,
            node_lookup: base.node_lookup,
            ..Default::default()
        };
        // Reconstruct declared_symbols and symbol_file from the surviving nodes.
        for node_index in builder.graph.node_indices() {
            let node = &builder.graph[node_index];
            if let RepoGraphNodeKind::Symbol = node.kind
                && let Some(sym) = &node.symbol
            {
                if !node.is_external {
                    builder.declared_symbols.insert(sym.clone());
                }
                if let Some(fp) = &node.file_path {
                    builder.symbol_file.insert(sym.clone(), fp.clone());
                }
                if let Some(lang) = &node.language {
                    builder.symbol_language.insert(sym.clone(), lang.clone());
                }
            }
        }

        // Step 3: Replay changed-file SCIP data through the builder.
        for index in new_indices {
            for file in &index.files {
                if changed_files.contains(&file.relative_path) {
                    builder.add_file(file);
                }
            }
        }

        builder.finish()
    }
}

/// Returns `true` when `node` is "owned by" one of the changed files:
/// - file nodes whose path is in the set
/// - symbol nodes whose `file_path` is in the set *and* that are not external
fn is_owned_by_changed_file(node: &RepoGraphNode, changed_files: &BTreeSet<PathBuf>) -> bool {
    match &node.kind {
        RepoGraphNodeKind::File => {
            node.file_path
                .as_ref()
                .is_some_and(|p| changed_files.contains(p))
        }
        RepoGraphNodeKind::Symbol => {
            !node.is_external
                && node
                    .file_path
                    .as_ref()
                    .is_some_and(|p| changed_files.contains(p))
        }
    }
}

fn edge_weight(kind: RepoGraphEdgeKind) -> f64 {
    match kind {
        RepoGraphEdgeKind::ContainsDefinition => EDGE_WEIGHT_DEFINITION_TO_FILE,
        RepoGraphEdgeKind::DeclaredInFile => EDGE_WEIGHT_FILE_TO_DEFINITION,
        RepoGraphEdgeKind::FileReference => EDGE_WEIGHT_FILE_REFERENCE,
        RepoGraphEdgeKind::SymbolReference => EDGE_WEIGHT_SYMBOL_REFERENCE,
        RepoGraphEdgeKind::SymbolRelationshipReference => EDGE_WEIGHT_SYMBOL_RELATIONSHIP_REFERENCE,
        RepoGraphEdgeKind::SymbolRelationshipImplementation => {
            EDGE_WEIGHT_SYMBOL_RELATIONSHIP_IMPLEMENTATION
        }
        RepoGraphEdgeKind::SymbolRelationshipTypeDefinition => {
            EDGE_WEIGHT_SYMBOL_RELATIONSHIP_TYPE_DEFINITION
        }
        RepoGraphEdgeKind::SymbolRelationshipDefinition => {
            EDGE_WEIGHT_SYMBOL_RELATIONSHIP_DEFINITION
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use petgraph::visit::EdgeRef;

    use super::*;
    use crate::scip_parser::{
        ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
        ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
    };

    #[test]
    fn builds_dependency_graph_with_file_and_symbol_metadata() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);

        assert_eq!(graph.node_count(), 5);
        assert!(graph.edge_count() >= 8);

        let app_file = graph
            .file_node("src/app.rs")
            .expect("app file node should exist");
        let app_node = graph.node(app_file);
        assert_eq!(app_node.kind, RepoGraphNodeKind::File);
        assert_eq!(app_node.language.as_deref(), Some("rust"));
        assert_eq!(app_node.file_path.as_deref(), Some(Path::new("src/app.rs")));

        let helper_symbol = graph
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .expect("helper symbol node should exist");
        let helper_node = graph.node(helper_symbol);
        assert_eq!(helper_node.kind, RepoGraphNodeKind::Symbol);
        assert_eq!(helper_node.symbol_kind, Some(ScipSymbolKind::Function));
        assert_eq!(
            helper_node.file_path.as_deref(),
            Some(Path::new("src/helper.rs"))
        );

        let has_file_reference = graph.graph().edges(app_file).any(|edge| {
            edge.target() == helper_symbol && edge.weight().kind == RepoGraphEdgeKind::FileReference
        });
        assert!(has_file_reference, "expected file->symbol reference edge");
    }

    #[test]
    fn page_rank_ordering_favors_referenced_symbols_and_files() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let ranking = graph.rank();

        let helper_symbol_rank = ranking
            .nodes
            .iter()
            .position(|node| {
                node.key
                    == RepoNodeKey::Symbol("scip-rust pkg src/helper.rs `helper`().".to_string())
            })
            .expect("helper symbol should be ranked");
        let app_symbol_rank = ranking
            .nodes
            .iter()
            .position(|node| {
                node.key == RepoNodeKey::Symbol("scip-rust pkg src/app.rs `main`().".to_string())
            })
            .expect("main symbol should be ranked");
        let helper_file_rank = ranking
            .nodes
            .iter()
            .position(|node| node.key == RepoNodeKey::File(PathBuf::from("src/helper.rs")))
            .expect("helper file should be ranked");
        let app_file_rank = ranking
            .nodes
            .iter()
            .position(|node| node.key == RepoNodeKey::File(PathBuf::from("src/app.rs")))
            .expect("app file should be ranked");

        assert!(helper_symbol_rank < app_symbol_rank);
        assert!(helper_file_rank < app_file_rank);

        let helper_symbol_score = ranking.nodes[helper_symbol_rank].score;
        let app_symbol_score = ranking.nodes[app_symbol_rank].score;
        assert!(helper_symbol_score > app_symbol_score);
    }

    fn fixture_index() -> ParsedScipIndex {
        let helper_symbol_name = "scip-rust pkg src/helper.rs `helper`().".to_string();
        let helper_symbol = ScipSymbol {
            symbol: helper_symbol_name.clone(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("helper".to_string()),
            signature: Some("fn helper()".to_string()),
            documentation: vec!["returns a value".to_string()],
            relationships: vec![],
        };
        let trait_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
            kind: Some(ScipSymbolKind::Type),
            display_name: Some("HelperTrait".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
        };
        let main_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("main".to_string()),
            signature: Some("fn main()".to_string()),
            documentation: vec![],
            relationships: vec![ScipRelationship {
                source_symbol: "scip-rust pkg src/app.rs `main`().".to_string(),
                target_symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
                kinds: BTreeSet::from([ScipRelationshipKind::Implementation]),
            }],
        };

        ParsedScipIndex {
            metadata: ScipMetadata {
                project_root: Some("file:///workspace/repo".to_string()),
                tool_name: Some("rust-analyzer".to_string()),
                tool_version: Some("1.0.0".to_string()),
            },
            files: vec![
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/helper.rs"),
                    definitions: vec![definition_occurrence(&helper_symbol_name)],
                    references: vec![],
                    occurrences: vec![definition_occurrence(&helper_symbol_name)],
                    symbols: vec![helper_symbol],
                },
                ScipFile {
                    language: "rust".to_string(),
                    relative_path: PathBuf::from("src/app.rs"),
                    definitions: vec![definition_occurrence(&main_symbol.symbol)],
                    references: vec![reference_occurrence(&helper_symbol_name)],
                    occurrences: vec![
                        definition_occurrence(&main_symbol.symbol),
                        reference_occurrence(&helper_symbol_name),
                    ],
                    symbols: vec![main_symbol, trait_symbol],
                },
            ],
            external_symbols: vec![],
        }
    }

    fn definition_occurrence(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 6,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    fn reference_occurrence(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 1,
                start_character: 4,
                end_line: 1,
                end_character: 10,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::ReadAccess]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    #[test]
    fn artifact_round_trip_preserves_graph_structure() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let original_node_count = graph.node_count();
        let original_edge_count = graph.edge_count();

        let artifact = graph.to_artifact();
        assert_eq!(artifact.nodes.len(), original_node_count);
        assert_eq!(artifact.edges.len(), original_edge_count);

        let restored = RepoDependencyGraph::from_artifact(&artifact);
        assert_eq!(restored.node_count(), original_node_count);
        assert_eq!(restored.edge_count(), original_edge_count);

        // Verify file and symbol lookups still work after round-trip.
        assert!(restored.file_node("src/app.rs").is_some());
        assert!(restored.file_node("src/helper.rs").is_some());
        assert!(
            restored
                .symbol_node("scip-rust pkg src/helper.rs `helper`().")
                .is_some()
        );
        assert!(
            restored
                .symbol_node("scip-rust pkg src/app.rs `main`().")
                .is_some()
        );

        // Verify ranking still produces valid results.
        let ranking = restored.rank();
        assert!(!ranking.nodes.is_empty());
    }

    #[test]
    fn artifact_json_round_trip_preserves_graph() {
        let graph = RepoDependencyGraph::build(&[fixture_index()]);
        let json = graph.serialize_artifact().expect("serialize");
        let restored =
            RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize");

        assert_eq!(restored.node_count(), graph.node_count());
        assert_eq!(restored.edge_count(), graph.edge_count());

        // Verify node metadata survived serialization.
        let helper_idx = restored
            .symbol_node("scip-rust pkg src/helper.rs `helper`().")
            .expect("helper symbol");
        let helper_node = restored.node(helper_idx);
        assert_eq!(helper_node.symbol_kind, Some(ScipSymbolKind::Function));
        assert_eq!(helper_node.display_name, "helper");
        assert_eq!(
            helper_node.file_path.as_deref(),
            Some(Path::new("src/helper.rs"))
        );

        // Verify edge metadata survived.
        let app_idx = restored
            .file_node("src/app.rs")
            .expect("app file");
        let has_contains_def = restored
            .graph()
            .edges(app_idx)
            .any(|e| e.weight().kind == RepoGraphEdgeKind::ContainsDefinition);
        assert!(has_contains_def, "expected ContainsDefinition edge from app file");
    }

    #[test]
    fn empty_artifact_round_trip() {
        let empty = RepoGraphArtifact {
            nodes: vec![],
            edges: vec![],
        };
        let json = serde_json::to_string(&empty).expect("serialize empty");
        let restored =
            RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize empty");
        assert_eq!(restored.node_count(), 0);
        assert_eq!(restored.edge_count(), 0);
    }

    // ---- patch_changed_files tests ----

    /// Build a modified index where src/app.rs has a new symbol and a removed
    /// reference, then patch the original graph and verify the result reflects
    /// the changes.
    #[test]
    fn patch_changed_files_updates_graph_for_modified_file() {
        let original = RepoDependencyGraph::build(&[fixture_index()]);

        // The original graph has src/helper.rs and src/app.rs.
        assert!(original.file_node("src/app.rs").is_some());
        assert!(original.file_node("src/helper.rs").is_some());
        assert!(original.symbol_node("scip-rust pkg src/app.rs `main`().").is_some());

        // Build a replacement for src/app.rs that has a new symbol "run"
        // instead of "main" and no reference to helper.
        let run_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/app.rs `run`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("run".to_string()),
            signature: Some("fn run()".to_string()),
            documentation: vec![],
            relationships: vec![],
        };
        let new_index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/app.rs"),
                definitions: vec![definition_occurrence(&run_symbol.symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&run_symbol.symbol)],
                symbols: vec![run_symbol],
            }],
            external_symbols: vec![],
        };

        let changed = BTreeSet::from([PathBuf::from("src/app.rs")]);
        let patched = original.patch_changed_files(&changed, &[new_index]);

        // The old "main" symbol should be gone; the new "run" symbol should exist.
        assert!(
            patched.symbol_node("scip-rust pkg src/app.rs `main`().").is_none(),
            "old main symbol should be removed after patch"
        );
        assert!(
            patched.symbol_node("scip-rust pkg src/app.rs `run`().").is_some(),
            "new run symbol should be present after patch"
        );

        // src/helper.rs and its symbol should be untouched.
        assert!(patched.file_node("src/helper.rs").is_some());
        assert!(patched.symbol_node("scip-rust pkg src/helper.rs `helper`().").is_some());

        // src/app.rs file node should still exist (re-added by the new index).
        assert!(patched.file_node("src/app.rs").is_some());

        // Ranking should still work and produce valid output.
        let patched_ranking = patched.rank();
        assert!(!patched_ranking.nodes.is_empty());

        // The helper symbol should still rank high (it was not changed).
        let helper_rank = patched_ranking
            .nodes
            .iter()
            .position(|n| {
                n.key == RepoNodeKey::Symbol("scip-rust pkg src/helper.rs `helper`().".to_string())
            })
            .expect("helper should be ranked");
        assert!(helper_rank < patched_ranking.nodes.len());
    }

    /// Patching with an empty changed-file set produces the same graph.
    #[test]
    fn patch_with_no_changed_files_preserves_graph() {
        let original = RepoDependencyGraph::build(&[fixture_index()]);
        let changed: BTreeSet<PathBuf> = BTreeSet::new();
        let patched = original.patch_changed_files(&changed, &[]);
        assert_eq!(patched.node_count(), original.node_count());
        assert_eq!(patched.edge_count(), original.edge_count());
    }

    /// Patching a file that does not exist in the graph is a no-op for removal
    /// and just adds new data.
    #[test]
    fn patch_nonexistent_file_adds_new_data() {
        let original = RepoDependencyGraph::build(&[fixture_index()]);
        let original_node_count = original.node_count();

        let new_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/new.rs `new_fn`().".to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some("new_fn".to_string()),
            signature: None,
            documentation: vec![],
            relationships: vec![],
        };
        let new_index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/new.rs"),
                definitions: vec![definition_occurrence(&new_symbol.symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&new_symbol.symbol)],
                symbols: vec![new_symbol],
            }],
            external_symbols: vec![],
        };

        let changed = BTreeSet::from([PathBuf::from("src/new.rs")]);
        let patched = original.patch_changed_files(&changed, &[new_index]);

        // New file and symbol added.
        assert!(patched.file_node("src/new.rs").is_some());
        assert!(patched.symbol_node("scip-rust pkg src/new.rs `new_fn`().").is_some());
        // Original nodes preserved.
        assert!(patched.node_count() > original_node_count);
        assert!(patched.file_node("src/app.rs").is_some());
        assert!(patched.file_node("src/helper.rs").is_some());
    }

    /// Verify that the patched graph produces a different rendered map when
    /// file content changes.
    #[test]
    fn patch_produces_updated_rendered_map() {
        use crate::repo_map::{RepoMapRenderOptions, render_repo_map};

        let original = RepoDependencyGraph::build(&[fixture_index()]);
        let original_ranking = original.rank();
        let original_rendered = render_repo_map(
            &original,
            &original_ranking,
            &RepoMapRenderOptions::new(2000),
        )
        .expect("render original");

        // Replace src/app.rs with entirely different content.
        let widget_symbol = ScipSymbol {
            symbol: "scip-rust pkg src/app.rs `Widget`#".to_string(),
            kind: Some(ScipSymbolKind::Struct),
            display_name: Some("Widget".to_string()),
            signature: Some("struct Widget".to_string()),
            documentation: vec![],
            relationships: vec![],
        };
        let new_index = ParsedScipIndex {
            metadata: ScipMetadata::default(),
            files: vec![ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/app.rs"),
                definitions: vec![definition_occurrence(&widget_symbol.symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&widget_symbol.symbol)],
                symbols: vec![widget_symbol],
            }],
            external_symbols: vec![],
        };

        let changed = BTreeSet::from([PathBuf::from("src/app.rs")]);
        let patched = original.patch_changed_files(&changed, &[new_index]);
        let patched_ranking = patched.rank();
        let patched_rendered = render_repo_map(
            &patched,
            &patched_ranking,
            &RepoMapRenderOptions::new(2000),
        )
        .expect("render patched");

        // The rendered maps should differ because the file content changed.
        assert_ne!(
            original_rendered.content, patched_rendered.content,
            "patched rendered map should differ from original"
        );
        // The patched map should mention Widget (the new symbol).
        assert!(
            patched_rendered.content.contains("Widget"),
            "patched map should contain the new Widget symbol"
        );
        // The patched map should NOT mention main (the removed symbol).
        assert!(
            !patched_rendered.content.contains("main"),
            "patched map should not contain the removed main symbol"
        );
    }
}
