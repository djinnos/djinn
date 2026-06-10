//! `djinn-graph::repo_graph` — in-memory repository dependency graph
//! data structure plus its builders, queries, serialization, and
//! persistence helpers.
//!
//! The module used to be a single 4,573-line `repo_graph.rs` file. The
//! follow-up tasks in this wave split it into focused submodules so
//! individual concerns can be reviewed and tested in isolation:
//!
//! | submodule  | concern                                                     |
//! |------------|-------------------------------------------------------------|
//! | `constants`| edge confidence/weight tables, PageRank knobs, version stamp |
//! | `node`     | `RepoGraphNode`, `RepoNodeKey`, `RepoGraphNodeKind`           |
//! | `edge`     | `RepoGraphEdge`, `RepoGraphEdgeKind`, edge weight/confidence |
//! | `tests`    | the `repo_graph::tests` test module                          |
//! | `artifact` | `RepoGraphArtifact` + v10 compat (sibling task `yxp7`)       |
//! | `builder`  | `RepoDependencyGraphBuilder` (sibling task `3hrr`)           |
//! | `graph`    | `RepoDependencyGraph` + queries (this task, `our5`)          |
//! | `ranking`  | PageRank / RRF (this task, `our5`)                           |
//!
//! All public types are re-exported here so downstream consumers
//! (`crate::repo_graph::RepoGraphNode`, etc.) keep working without
//! edits.

mod artifact;
mod builder;
mod constants;
mod edge;
mod graph;
mod node;
mod ranking;

#[cfg(test)]
mod tests;

// Re-exports for the public API — see `crates/djinn-control-plane/src/
// tools/graph_tools.rs`, `server/src/mcp_bridge.rs`, `cluster_doc.rs`,
// `communities.rs`, etc. for the consumer side.
pub use self::artifact::{
    RepoGraphArtifact, RepoGraphArtifactEdge, RepoGraphArtifactProcess,
    RepoGraphArtifactSymbolRange, deserialize_repo_graph_artifact_bincode,
};
pub use self::constants::{REPO_GRAPH_ARTIFACT_VERSION, is_test_path};
pub use self::edge::{RepoGraphEdge, RepoGraphEdgeKind, edge_confidence_floor};
pub use self::graph::{RepoDependencyGraph, SymbolRange};
pub use self::node::{RepoGraphNode, RepoGraphNodeKind, RepoGraphSearchHit, RepoNodeKey};
pub use self::ranking::{RankedRepoGraphNode, RepoGraphRanking};

// Re-exports for sibling submodule bodies (the impl blocks in
// `mod.rs` itself) AND the `repo_graph::tests` test module. `pub(crate)`
// so the items stay crate-internal while still being reachable from
// `mod.rs` and its descendants (including the test module, which is a
// child of `mod.rs`).
//
// `#[allow(unused_imports)]` because the `EDGE_CONFIDENCE_*` constants
// and ranking helpers are only consumed by the test module / sibling
// submodules — the lib build doesn't directly reference them. Without
// the attribute the lib build would emit unused-imports warnings.
#[allow(unused_imports)]
pub(crate) use self::constants::{
    EDGE_CONFIDENCE_LOCAL_PENALTY, EDGE_CONFIDENCE_READS, EDGE_CONFIDENCE_WRITES,
    PAGE_RANK_DAMPING_FACTOR, PAGE_RANK_ITERATIONS,
};
pub(crate) use self::edge::{edge_weight, edge_weight_for};
#[allow(unused_imports)]
pub(crate) use self::ranking::{
    apply_rrf_fused_rank, compute_entry_point_distance, compute_pagerank_sparse,
};

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipOccurrence, ScipRelationship, ScipRelationshipKind, ScipSymbol,
    ScipSymbolRole, ScipVisibility,
};
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;

#[cfg(test)]
use self::graph::is_owned_by_changed_file;
use self::graph::{build_name_index, build_process_lookup, derive_edge_confidence};

#[derive(Default)]
struct RepoDependencyGraphBuilder {
    graph: DiGraph<RepoGraphNode, RepoGraphEdge>,
    node_lookup: BTreeMap<RepoNodeKey, NodeIndex>,
    edge_accumulator: BTreeMap<(NodeIndex, NodeIndex, RepoGraphEdgeKind), usize>,
    symbol_file: BTreeMap<String, PathBuf>,
    symbol_language: BTreeMap<String, String>,
    declared_symbols: BTreeSet<String>,
    /// Accumulator for the per-file `SymbolRange` sidecar. Unsorted; the
    /// builder sorts each entry by `start_line` in `finish()`.
    symbol_ranges: BTreeMap<PathBuf, Vec<SymbolRange>>,
    /// Project clone root, when known. When set, edge classification can
    /// fall back to a tree-sitter-based access classifier for occurrences
    /// whose SCIP indexer didn't populate `ReadAccess`/`WriteAccess`
    /// roles (notably rust-analyzer). When `None`, classification stays
    /// SCIP-only — used by unit tests that pass synthetic indices with
    /// no on-disk file backing.
    project_root: Option<PathBuf>,
    classifier: crate::access_classifier::AccessClassifier,
    /// Per-file source-text cache. `None` means a previous read failed
    /// (file outside project root, missing, not UTF-8) — re-cached so
    /// we don't keep retrying.
    source_cache: BTreeMap<PathBuf, Option<String>>,
    /// Workspace slug of the ParsedScipIndex currently being replayed into
    /// the builder. Stamped on every SCIP-derived node created during the pass.
    current_workspace: Option<String>,
}

impl RepoDependencyGraphBuilder {
    fn add_index(&mut self, index: &ParsedScipIndex) {
        self.current_workspace = Some(index.workspace_slug.clone());
        for external_symbol in &index.external_symbols {
            self.ensure_symbol_node(external_symbol, None, None, true);
        }

        for file in &index.files {
            self.add_file(file);
        }
        self.current_workspace = None;
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
                self.record_symbol_range(symbol_index, file, definition);
                // PR F1: propagate the SCIP `Test` role from the
                // definition occurrence onto the symbol node so the
                // entry-point detector can use it as the high-confidence
                // signal for test detection. SCIP `Test` is bit 32 on
                // `SymbolRole`; not every indexer stamps it (the Rust
                // scip-rust shipped as of 2026-04 does not), which is
                // why we keep the file-path / name-prefix heuristics in
                // [`crate::entry_points`] as a fallback.
                if definition.roles.contains(&ScipSymbolRole::Test) {
                    self.graph[symbol_index].is_test = true;
                }
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
            // PR A3: split symbol-to-file references on SCIP role flags so
            // `code_graph neighbors --kind_filter=writes` can pick out
            // mutators of a field. SCIP can stamp both `ReadAccess` and
            // `WriteAccess` on the same occurrence (e.g. `x += 1`); when
            // both flags are present we treat it as a write since the
            // mutation is the more load-bearing signal for callers asking
            // "who changes X".
            //
            // Indexer-quality fallback: when neither role bit is set
            // (notably rust-analyzer, which emits no access roles at all),
            // consult the tree-sitter `AccessClassifier` to recover the
            // read/write distinction from AST context. Only fires when
            // the builder was created via `build_with_source` with a
            // project root — `build` keeps the SCIP-only fast path for
            // unit tests with synthetic indices.
            let edge_kind = self.classify_reference_edge_kind(file, occurrence);
            self.bump_edge(symbol_index, target_file_index, edge_kind, 1);
        }
    }

    /// Classify the symbol→target_file reference edge for an occurrence.
    /// SCIP role bits are the primary signal. When the indexer didn't
    /// populate either `ReadAccess` or `WriteAccess` (rust-analyzer is
    /// the canonical case), fall back to the tree-sitter
    /// [`crate::access_classifier::AccessClassifier`] which derives the
    /// distinction from AST context (`assignment_expression` LHS, etc.).
    /// The fallback only fires when the builder has a `project_root`
    /// and the occurrence's file is readable as UTF-8.
    fn classify_reference_edge_kind(
        &mut self,
        file: &ScipFile,
        occurrence: &ScipOccurrence,
    ) -> RepoGraphEdgeKind {
        if occurrence.roles.contains(&ScipSymbolRole::WriteAccess) {
            return RepoGraphEdgeKind::Writes;
        }
        if occurrence.roles.contains(&ScipSymbolRole::ReadAccess) {
            return RepoGraphEdgeKind::Reads;
        }
        let Some(root) = self.project_root.as_ref() else {
            return RepoGraphEdgeKind::SymbolReference;
        };
        // Read-and-cache the file source. Failures are negative-cached
        // so subsequent occurrences in the same file don't re-stat.
        let rel = file.relative_path.clone();
        if !self.source_cache.contains_key(&rel) {
            let abs = root.join(&rel);
            let read = std::fs::read_to_string(&abs).ok();
            self.source_cache.insert(rel.clone(), read);
        }
        let Some(source) = self.source_cache.get(&rel).and_then(|s| s.as_deref()) else {
            return RepoGraphEdgeKind::SymbolReference;
        };
        let kind = self.classifier.classify(
            file.language.as_str(),
            source,
            occurrence.range.start_line as u32,
            occurrence.range.start_character as u32,
        );
        use crate::access_classifier::AccessKind;
        match kind {
            AccessKind::Write | AccessKind::ReadWrite => RepoGraphEdgeKind::Writes,
            AccessKind::Read => RepoGraphEdgeKind::Reads,
            AccessKind::NotAnAccess | AccessKind::Unknown => RepoGraphEdgeKind::SymbolReference,
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
                ScipRelationshipKind::Reference => RepoGraphEdgeKind::Extends,
                ScipRelationshipKind::Implementation => RepoGraphEdgeKind::Implements,
                ScipRelationshipKind::TypeDefinition => RepoGraphEdgeKind::TypeDefines,
                ScipRelationshipKind::Definition => RepoGraphEdgeKind::Defines,
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
        // v10: stamp File nodes with the canonical path-convention test
        // flag so the `/code-graph` UI toggle and `code_graph tests=`
        // filter can hide whole test files.
        let is_test = is_test_path(&display_name);
        let node = RepoGraphNode {
            id: key.clone(),
            kind: RepoGraphNodeKind::File,
            display_name,
            language: Some(language.to_string()),
            file_path: Some(path.to_path_buf()),
            symbol: None,
            symbol_kind: None,
            is_external: false,
            visibility: None,
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test,
            complexity: None,
            workspace: self.current_workspace.clone(),
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
            visibility: symbol.visibility,
            signature: symbol.signature.clone(),
            documentation: symbol.documentation.clone(),
            signature_parts: symbol.signature_parts.clone(),
            // v10: a symbol defined in a test file is a test symbol. The
            // SCIP `Test`-role signal is OR-ed in later (see `add_file`).
            is_test: file_path
                .map(|p| is_test_path(&p.display().to_string()))
                .unwrap_or(false),
            complexity: None,
            workspace: self.current_workspace.clone(),
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
            visibility: Some(crate::scip_parser::ScipVisibility::from_symbol_identifier(
                &occurrence.symbol,
            )),
            signature_parts: None,
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
        let is_external = !self.declared_symbols.contains(symbol);
        // v10: classify placeholder symbols by their recorded definition
        // file when known. Computed before the struct literal moves
        // `file_path`.
        let is_test = file_path
            .as_ref()
            .map(|p| is_test_path(&p.display().to_string()))
            .unwrap_or(false);
        let node = RepoGraphNode {
            id: RepoNodeKey::Symbol(symbol.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: symbol.to_string(),
            language,
            file_path,
            symbol: Some(symbol.to_string()),
            symbol_kind: None,
            is_external,
            visibility: Some(ScipVisibility::from_symbol_identifier(symbol)),
            signature: None,
            documentation: Vec::new(),
            signature_parts: None,
            is_test,
            complexity: None,
            workspace: self.current_workspace.clone(),
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

    /// Record the definition's enclosing range (if any) into the sidecar
    /// `symbol_ranges` map. SCIP lines are 0-indexed on the wire; we
    /// normalize to the 1-indexed inclusive convention used by callers.
    fn record_symbol_range(
        &mut self,
        symbol_index: NodeIndex,
        file: &ScipFile,
        occurrence: &ScipOccurrence,
    ) {
        let Some(enclosing) = occurrence.enclosing_range.as_ref() else {
            return;
        };
        let start_line = (enclosing.start_line.max(0) as u32).saturating_add(1);
        let end_line = (enclosing.end_line.max(0) as u32).saturating_add(1);
        let (start_line, end_line) = if start_line <= end_line {
            (start_line, end_line)
        } else {
            (end_line, start_line)
        };
        self.symbol_ranges
            .entry(file.relative_path.clone())
            .or_default()
            .push(SymbolRange {
                start_line,
                end_line,
                node: symbol_index,
            });
    }

    fn finish(mut self) -> RepoDependencyGraph {
        for ((source, target, kind), evidence_count) in self.edge_accumulator {
            let (confidence, reason) = derive_edge_confidence(&self.graph, source, target, kind);
            self.graph.add_edge(
                source,
                target,
                RepoGraphEdge {
                    kind,
                    weight: edge_weight(kind) * (evidence_count as f64),
                    evidence_count,
                    confidence,
                    reason,
                    step: None,
                },
            );
        }

        // Sort each per-file range vec by `start_line` so callers can reason
        // about ordering even though nesting still demands a linear overlap
        // scan.
        for ranges in self.symbol_ranges.values_mut() {
            ranges.sort_by_key(|r| (r.start_line, r.end_line));
        }

        let name_index = build_name_index(&self.graph);
        let mut graph = RepoDependencyGraph {
            graph: self.graph,
            node_lookup: self.node_lookup,
            name_index,
            symbol_ranges: self.symbol_ranges,
            communities: Vec::new(),
            community_lookup: BTreeMap::new(),
            processes: Vec::new(),
            process_lookup: BTreeMap::new(),
        };

        // PR F3: run modularity-based community detection unless the
        // feature flag is explicitly turned off. The detector is
        // O((V + E) × iterations); on a 12k-node, 150k-edge canonical
        // graph it lands in the ~hundreds-of-ms range — comparable to
        // the SCC pass that already runs in `derive_graph_caches`.
        if crate::communities::detection_enabled() {
            let communities = crate::communities::detect_communities(&graph);
            graph.install_communities(communities);
        }

        graph
    }
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
                confidence: w.confidence,
                reason: w.reason.clone(),
                step: w.step,
            });
        }

        let mut symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>> =
            BTreeMap::new();
        for (file, ranges) in &self.symbol_ranges {
            let mut translated = Vec::with_capacity(ranges.len());
            for range in ranges {
                // Skip ranges whose node isn't in the artifact's node table —
                // shouldn't happen in practice, but guards against bookkeeping
                // drift between the petgraph and the sidecar.
                let Some(&node_pos) = index_map.get(&range.node) else {
                    continue;
                };
                translated.push(RepoGraphArtifactSymbolRange {
                    start_line: range.start_line,
                    end_line: range.end_line,
                    node: node_pos,
                });
            }
            if !translated.is_empty() {
                symbol_ranges.insert(file.clone(), translated);
            }
        }

        // PR F2: serialize the process sidecar. Each `Process` is keyed
        // by node positions (a `Vec<usize>`) rather than `NodeIndex`
        // values so the artifact survives a `from_artifact` rebuild.
        let mut processes_out: Vec<RepoGraphArtifactProcess> =
            Vec::with_capacity(self.processes.len());
        for process in &self.processes {
            let Some(&entry_pos) = index_map.get(&process.entry_point_id) else {
                continue;
            };
            let Some(&terminal_pos) = index_map.get(&process.terminal_id) else {
                continue;
            };
            let Some(&process_node_pos) = index_map.get(&process.process_node_id) else {
                continue;
            };
            let mut steps_out = Vec::with_capacity(process.steps.len());
            let mut steps_complete = true;
            for step in &process.steps {
                let Some(&pos) = index_map.get(step) else {
                    steps_complete = false;
                    break;
                };
                steps_out.push(pos);
            }
            if !steps_complete {
                continue;
            }
            processes_out.push(RepoGraphArtifactProcess {
                id: process.id.clone(),
                label: process.label.clone(),
                process_node: process_node_pos,
                entry_point: entry_pos,
                terminal: terminal_pos,
                steps: steps_out,
            });
        }

        RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes,
            edges,
            symbol_ranges,
            communities: self.communities.clone(),
            processes: processes_out,
        }
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
                    confidence: edge.confidence,
                    reason: edge.reason.clone(),
                    step: edge.step,
                },
            );
        }

        let name_index = build_name_index(&graph);

        let mut symbol_ranges: BTreeMap<PathBuf, Vec<SymbolRange>> = BTreeMap::new();
        for (file, ranges) in &artifact.symbol_ranges {
            let mut translated = Vec::with_capacity(ranges.len());
            for range in ranges {
                let Some(&node) = index_map.get(range.node) else {
                    continue;
                };
                translated.push(SymbolRange {
                    start_line: range.start_line,
                    end_line: range.end_line,
                    node,
                });
            }
            translated.sort_by_key(|r| (r.start_line, r.end_line));
            if !translated.is_empty() {
                symbol_ranges.insert(file.clone(), translated);
            }
        }

        // PR F2: rehydrate the process sidecar. Reject any process whose
        // step list references a node position outside the artifact's
        // bounds — defensive guard against an artifact and node table
        // that drifted out of sync.
        let mut processes: Vec<crate::processes::Process> =
            Vec::with_capacity(artifact.processes.len());
        for process in &artifact.processes {
            let Some(&entry_id) = index_map.get(process.entry_point) else {
                continue;
            };
            let Some(&terminal_id) = index_map.get(process.terminal) else {
                continue;
            };
            let Some(&process_node_id) = index_map.get(process.process_node) else {
                continue;
            };
            let mut steps_out = Vec::with_capacity(process.steps.len());
            let mut steps_complete = true;
            for &step_pos in &process.steps {
                let Some(&node) = index_map.get(step_pos) else {
                    steps_complete = false;
                    break;
                };
                steps_out.push(node);
            }
            if !steps_complete {
                continue;
            }
            processes.push(crate::processes::Process {
                id: process.id.clone(),
                label: process.label.clone(),
                process_node_id,
                entry_point_id: entry_id,
                terminal_id,
                step_count: steps_out.len(),
                steps: steps_out,
            });
        }
        let process_lookup = build_process_lookup(&processes);

        let mut out = RepoDependencyGraph {
            graph,
            node_lookup,
            name_index,
            symbol_ranges,
            communities: Vec::new(),
            community_lookup: BTreeMap::new(),
            processes,
            process_lookup,
        };
        // PR F3: rehydrate the community sidecar verbatim — node
        // positions in the artifact match `NodeIndex` 0..n thanks to the
        // ordered `add_node` loop above.
        if !artifact.communities.is_empty() {
            out.install_communities(artifact.communities.clone());
        }
        out
    }

    /// Serialize the graph artifact to a JSON string for DB storage.
    pub fn serialize_artifact(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.to_artifact())
    }

    /// Deserialize a graph from a previously stored JSON artifact string.
    #[cfg(test)]
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
    #[cfg(test)]
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
                confidence: edge.confidence,
                reason: edge.reason.clone(),
                step: edge.step,
            })
            .collect();

        let mut surviving_symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>> =
            BTreeMap::new();
        for (file, ranges) in &artifact.symbol_ranges {
            if changed_files.contains(file) {
                continue;
            }
            let mut translated = Vec::with_capacity(ranges.len());
            for range in ranges {
                let Some(&new_node) = position_map.get(&range.node) else {
                    continue;
                };
                translated.push(RepoGraphArtifactSymbolRange {
                    start_line: range.start_line,
                    end_line: range.end_line,
                    node: new_node,
                });
            }
            if !translated.is_empty() {
                surviving_symbol_ranges.insert(file.clone(), translated);
            }
        }

        // PR F2: drop the process sidecar entirely on patch — the
        // changed files may have rewritten the call chains the trace
        // followed, and the test path doesn't exercise the process
        // detector anyway. The next full rebuild re-runs detection
        // from scratch.
        let filtered_artifact = RepoGraphArtifact {
            version: REPO_GRAPH_ARTIFACT_VERSION,
            nodes: surviving_nodes,
            edges: surviving_edges,
            symbol_ranges: surviving_symbol_ranges,
            // Communities are recomputed when the rebuilt graph runs
            // through `finish()`; dropping the stale sidecar here is
            // the safe choice since member positions get remapped
            // anyway.
            communities: Vec::new(),
            // Processes are likewise recomputed by the post-build pass.
            processes: Vec::new(),
        };

        // Step 2: Rebuild the base graph from the filtered artifact.
        // We use a builder so that the new SCIP data can link to existing
        // nodes (e.g. symbols defined in unchanged files that are referenced
        // by changed files).
        let base = Self::from_artifact(&filtered_artifact);
        let mut builder = RepoDependencyGraphBuilder {
            graph: base.graph,
            node_lookup: base.node_lookup,
            symbol_ranges: base.symbol_ranges,
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
