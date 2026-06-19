//! `RepoDependencyGraphBuilder` — the builder that turns a slice of
//! `ParsedScipIndex` into a fully-populated `RepoDependencyGraph`.
//!
//! Pulled verbatim out of the original 4,573-line `repo_graph.rs`
//! (the builder block sat at roughly lines 868–1331 of the post-foundation
//! `mod.rs`). The struct and its `impl` block were a tight, self-contained
//! unit: every dependency they need (`RepoGraphNode`, `RepoGraphEdge`,
//! `RepoNodeKey`, `RepoGraphEdgeKind`, the SCIP parser types, and the
//! per-file `SymbolRange` sidecar) is already re-exported from
//! `super::mod.rs`. Free helpers used during `finish()` —
//! `derive_edge_confidence`, `node_is_local_symbol`, `build_name_index` —
//! remain in `super` and are reachable via the `pub(super)` visibility
//! applied there.
//!
//! No logic changes — the body is byte-for-byte identical to the
//! pre-split tree (only the path of the struct declaration changes from
//! a private item inside `mod.rs` to a `pub(crate)` item in this
//! submodule).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use petgraph::graph::{DiGraph, NodeIndex};

use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipOccurrence, ScipRelationship, ScipRelationshipKind, ScipSymbol,
    ScipSymbolRole, ScipVisibility,
};

use super::{
    RepoDependencyGraph, RepoGraphEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
    RepoNodeKey, RouteExclusionConfig, SymbolRange, build_name_index, derive_edge_confidence,
    edge_weight, is_test_path,
};

#[derive(Default)]
pub(crate) struct RepoDependencyGraphBuilder {
    pub(super) graph: DiGraph<RepoGraphNode, RepoGraphEdge>,
    pub(super) node_lookup: BTreeMap<RepoNodeKey, NodeIndex>,
    pub(super) edge_accumulator: BTreeMap<(NodeIndex, NodeIndex, RepoGraphEdgeKind), usize>,
    pub(super) symbol_file: BTreeMap<String, PathBuf>,
    pub(super) symbol_language: BTreeMap<String, String>,
    pub(super) declared_symbols: BTreeSet<String>,
    /// Accumulator for the per-file `SymbolRange` sidecar. Unsorted; the
    /// builder sorts each entry by `start_line` in `finish()`.
    pub(super) symbol_ranges: BTreeMap<PathBuf, Vec<SymbolRange>>,
    /// Project clone root, when known. When set, edge classification can
    /// fall back to a tree-sitter-based access classifier for occurrences
    /// whose SCIP indexer didn't populate `ReadAccess`/`WriteAccess`
    /// roles (notably rust-analyzer). When `None`, classification stays
    /// SCIP-only — used by unit tests that pass synthetic indices with
    /// no on-disk file backing.
    pub(super) project_root: Option<PathBuf>,
    pub(super) classifier: crate::access_classifier::AccessClassifier,
    /// Per-file source-text cache. `None` means a previous read failed
    /// (file outside project root, missing, not UTF-8) — re-cached so
    /// we don't keep retrying.
    pub(super) source_cache: BTreeMap<PathBuf, Option<String>>,
    /// Workspace slug of the ParsedScipIndex currently being replayed into
    /// the builder. Stamped on every SCIP-derived node created during the pass.
    pub(super) current_workspace: Option<String>,
}

impl RepoDependencyGraphBuilder {
    pub(super) fn add_index(&mut self, index: &ParsedScipIndex) {
        self.add_scip_files(
            &index.workspace_slug,
            &index.external_symbols,
            index.files.iter(),
        );
    }

    /// Process external symbols and iterate over files without requiring
    /// the full [`ParsedScipIndex`] to be resident in memory. Designed for
    /// the bounded-memory out-of-core path where files are streamed one at
    /// a time from the on-disk store.
    ///
    /// # Memory invariant
    ///
    /// Only one `ScipFile` is resident per iteration step — the iterator
    /// is consumed lazily so peak file-data residency is **O(1)**.
    pub(super) fn add_scip_files<'a, I>(
        &mut self,
        workspace_slug: &str,
        external_symbols: &[ScipSymbol],
        files: I,
    ) where
        I: Iterator<Item = &'a ScipFile>,
    {
        self.current_workspace = Some(workspace_slug.to_string());
        for symbol in external_symbols {
            self.ensure_symbol_node(symbol, None, None, true);
        }
        // Bounded-memory invariant: only one ScipFile is resident per
        // iteration step — the iterator is consumed lazily.
        for file in files {
            self.add_file(file);
        }
        self.current_workspace = None;
    }

    /// Like [`Self::add_scip_files`] but accepts a **fallible** iterator
    /// of file entries. Designed for the out-of-core pipeline where each
    /// file is loaded from disk on demand and the load may fail.
    ///
    /// Processing stops at the first error.
    ///
    /// # Memory invariant
    ///
    /// Only one `ScipFile` is resident per iteration step — the iterator
    /// is consumed lazily so peak file-data residency is **O(1)**.
    pub(super) fn add_scip_files_fallible<I, F, E>(
        &mut self,
        workspace_slug: &str,
        external_symbols: &[ScipSymbol],
        files: I,
    ) -> Result<(), String>
    where
        I: Iterator<Item = Result<F, E>>,
        F: std::borrow::Borrow<ScipFile>,
        E: std::fmt::Display,
    {
        self.current_workspace = Some(workspace_slug.to_string());
        for symbol in external_symbols {
            self.ensure_symbol_node(symbol, None, None, true);
        }
        // Bounded-memory invariant: only one ScipFile is resident per
        // iteration step — the iterator is consumed lazily.
        for file_result in files {
            let file = file_result.map_err(|e| e.to_string())?;
            self.add_file(file.borrow());
        }
        self.current_workspace = None;
        Ok(())
    }

    pub(super) fn add_file(&mut self, file: &ScipFile) {
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
            // PR s6ch / cs4v: route metadata is not applicable to file
            // nodes — the field set is shared across kinds so the
            // struct needs the slot but it stays `None`.
            route_framework: None,
            route_handler_symbol: None,
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
            // PR s6ch / cs4v: route metadata is not applicable to
            // symbol nodes — the field set is shared across kinds so
            // the struct needs the slot but it stays `None`.
            route_framework: None,
            route_handler_symbol: None,
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
            // PR s6ch / cs4v: route metadata is not applicable to
            // placeholder symbol nodes — the field set is shared
            // across kinds so the struct needs the slot but it stays
            // `None`.
            route_framework: None,
            route_handler_symbol: None,
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

    pub(super) fn finish(mut self) -> RepoDependencyGraph {
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
            // PR s6ch / 92z7: freshly built graphs start with the
            // baseline exclusion config. The warmer / K8s pipeline
            // can swap in a project-specific config via
            // `set_route_exclusion_config` before the graph
            // round-trips through the artifact.
            route_exclusion_config: RouteExclusionConfig::default(),
            layout_positions: BTreeMap::new(),
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
