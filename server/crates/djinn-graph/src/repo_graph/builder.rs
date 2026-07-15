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

use super::constants::TRAIT_DISPATCH_FANOUT_CAP;

use super::{
    RepoDependencyGraph, RepoGraphEdge, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind,
    RepoNodeKey, RouteExclusionConfig, SymbolRange, build_name_index, derive_edge_confidence,
    edge_weight, is_test_path,
};

/// A single definition range stamped during `add_file`. Used to find
/// the enclosing caller symbol for a reference occurrence when
/// synthesizing caller → trait-method edges.
///
/// Stored as 1-indexed inclusive lines to match the consumer-facing
/// `SymbolRange` convention (SCIP wire values are 0-indexed; the
/// builder normalizes on the way in).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FileDefRange {
    pub start_line: u32,
    pub end_line: u32,
    pub node: NodeIndex,
}

pub(crate) type CommunitySeedCrateMap = BTreeMap<PathBuf, String>;

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
    /// Per-file definition ranges used to identify the enclosing
    /// caller/definition symbol for a reference occurrence when
    /// synthesizing caller → trait-method edges. Each entry stores
    /// `(start_line, end_line, node)` for every definition observed in
    /// the file, sorted by enclosing-region size (smallest first) so
    /// lookup picks the innermost containing symbol.
    ///
    /// Populated as `add_file` walks `file.definitions`; consumed by
    /// `enclosing_definition_for` during `add_reference`. Re-built on
    /// every `add_scip_files` call (the same builder sees multiple
    /// files, so the map is keyed by `PathBuf`).
    pub(super) file_def_ranges: BTreeMap<PathBuf, Vec<FileDefRange>>,
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
    /// Optional crate map supplied by a caller that explicitly opted into
    /// crate-aware community seeding. `None` keeps the legacy unseeded
    /// community-detection behaviour.
    pub(super) community_seed_by_crate: Option<CommunitySeedCrateMap>,
    /// In-memory trait-method → implementation-method index built from
    /// SCIP `Implementation` relationships. Each key is a trait-method
    /// symbol identifier, and the value is the list of concrete
    /// implementation method symbols that implement it. Populated
    /// during `add_relationship` when an `Implementation` relationship
    /// is observed from an impl method to its trait method. Consumed
    /// during `maybe_add_trait_dispatch_call` to emit bounded fan-out
    /// edges from a caller to known concrete implementations.
    pub(super) trait_impl_index: BTreeMap<String, Vec<String>>,
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
                // PR 8tu1: also stamp the per-file definition range so
                // `enclosing_definition_for` can resolve the caller
                // symbol for reference occurrences. The sidecar's
                // `SymbolRange` is sorted in `finish()`; the in-memory
                // copy is kept in (start, end, node) form, normalized
                // to 1-indexed inclusive lines, so callers can pick
                // the innermost containing definition without a
                // second conversion.
                let start_line = (definition.range.start_line.max(0) as u32).saturating_add(1);
                let end_line = (definition.range.end_line.max(0) as u32).saturating_add(1);
                let (start_line, end_line) = if start_line <= end_line {
                    (start_line, end_line)
                } else {
                    (end_line, start_line)
                };
                self.file_def_ranges
                    .entry(file.relative_path.clone())
                    .or_default()
                    .push(FileDefRange {
                        start_line,
                        end_line,
                        node: symbol_index,
                    });
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

        // Sort the per-file def ranges by enclosing-region size
        // (smallest first; ties broken by start_line) so
        // `enclosing_definition_for` picks the innermost container
        // with a single linear scan.
        for ranges in self.file_def_ranges.values_mut() {
            ranges.sort_by_key(|r| {
                let size = r.end_line.saturating_sub(r.start_line);
                (size, r.start_line, r.end_line)
            });
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

        // PR 8tu1: caller → trait-method dispatch edge (proposal t16t).
        // Materialize a symbol-level edge when a Rust reference
        // occurrence resolves to a trait-method symbol AND the
        // enclosing caller symbol can be resolved confidently. The
        // existing `FileReference` / `Reads` / `Writes` / `SymbolReference`
        // edges stay intact — this is additive metadata so
        // `code_graph neighbors` can resolve trait dispatch without
        // losing the file-level signal.
        self.maybe_add_trait_dispatch_call(file, occurrence, symbol_index);
    }

    /// When `occurrence.symbol` looks like a trait-method identifier
    /// (Method suffix with a Type/Interface parent descriptor in the
    /// SCIP symbol grammar) and the target is declared in-repo, stamp a
    /// `TraitDispatchCall` edge from the enclosing caller symbol to the
    /// trait-method symbol. Falls back silently when either side of
    /// the edge can't be resolved confidently — see the
    /// acceptance-criteria bullet about avoiding the edge on
    /// ambiguous callers.
    fn maybe_add_trait_dispatch_call(
        &mut self,
        file: &ScipFile,
        occurrence: &ScipOccurrence,
        target_symbol_index: NodeIndex,
    ) {
        // Rust-scoped for this epic. The SCIP parser strips
        // function-scoped variables, so most cross-language
        // `Method`-suffix identifiers in our indices come from
        // Rust; confining the check to the host file's language
        // keeps the synthetic edges well-typed without losing any
        // in-repo Rust signal.
        if file.language != "rust" {
            return;
        }
        // The target must be a known in-repo symbol — placeholder
        // external symbols never get the synthesized edge, which
        // preserves the "bounded to in-repo" contract.
        if !self.declared_symbols.contains(&occurrence.symbol) {
            return;
        }
        // The target must look like a trait method. We check the
        // descriptor chain rather than the kind field because
        // rust-analyzer emits `Method` for both trait methods and
        // impl methods; the `Type`/`Interface` parent is the
        // distinguishing signal that the method is bound to a
        // type/interface declaration.
        if !symbol_looks_like_trait_method(&occurrence.symbol, &self.declared_symbols) {
            return;
        }
        // Find the enclosing caller symbol. The reference's own
        // range is the most precise signal; fall back to
        // `enclosing_range` if the indexer provided it.
        let caller_index = match self.enclosing_definition_for(&file.relative_path, occurrence) {
            Some(idx) => idx,
            None => return,
        };
        // Avoid self-loops (the trait method calling itself via
        // an associated function or recursive path) and avoid
        // duplicating the edge if caller and target collapse to the
        // same symbol (e.g. a trait method referenced from a
        // sibling trait default body — same SCIP identifier).
        if caller_index == target_symbol_index {
            return;
        }
        // Stamping the reason is what makes the edge filterable
        // through `confidence_tier()` — `suppressed`/`below-floor`
        // substring matches would push it to `Ambiguous`, but the
        // canonical `trait-dispatch-call` reason keeps it on the
        // `Inferred` track.
        self.bump_edge(
            caller_index,
            target_symbol_index,
            RepoGraphEdgeKind::TraitDispatchCall,
            1,
        );
        // The edge's provenance is carried by the
        // `RepoGraphEdgeKind::TraitDispatchCall` kind on its own —
        // `edge_confidence_floor` and `edge_confidence_tier` in
        // `edge.rs` translate that into a stable `Inferred` tier at
        // the 0.70 floor. The canonical reason constant
        // ([`crate::repo_graph::REASON_TRAIT_DISPATCH_CALL`]) is the
        // human-readable companion for downstream consumers that
        // want to string-match provenance without enumerating edge
        // kinds; the bump itself doesn't need to stamp a reason
        // field since the kind is the load-bearing provenance
        // signal.

        // PR 1h6c: Bounded fan-out to known concrete implementations.
        // When the trait method has known implementations indexed from
        // SCIP `Implementation` relationships AND the count is within
        // `TRAIT_DISPATCH_FANOUT_CAP`, emit a `TraitDispatchCall` edge
        // from the caller to each concrete implementation method. This
        // gives downstream consumers (e.g. `code_graph neighbors`) a
        // direct path to the concrete call targets without requiring
        // dynamic type resolution.
        //
        // When the cap is exceeded, no impl fan-out edges are emitted —
        // the direct caller → trait-method edge remains as the only
        // dispatch signal. This prevents unbounded edge multiplication
        // for widely-implemented traits (e.g. `RuntimeOps` with 10+ impls).
        if let Some(impls) = self.trait_impl_index.get(&occurrence.symbol)
            && impls.len() <= TRAIT_DISPATCH_FANOUT_CAP
        {
            // Clone the impl list to release the immutable borrow
            // on `self.trait_impl_index` before calling
            // `self.bump_edge` (which requires &mut self).
            let impls_clone = impls.clone();
            for impl_sym in &impls_clone {
                if let Some(&impl_index) =
                    self.node_lookup.get(&RepoNodeKey::Symbol(impl_sym.clone()))
                {
                    // Avoid self-loops and duplicates with the
                    // direct caller → trait-method edge.
                    if impl_index == caller_index || impl_index == target_symbol_index {
                        continue;
                    }
                    self.bump_edge(
                        caller_index,
                        impl_index,
                        RepoGraphEdgeKind::TraitDispatchCall,
                        1,
                    );
                }
            }
        }
    }

    /// Find the innermost definition whose range contains
    /// `occurrence`'s `enclosing_range` (or its own `range` when no
    /// enclosing range is supplied) within the file at `file_path`.
    /// Returns `None` when no definition contains the occurrence —
    /// happens when the reference is at file scope (e.g. a
    /// trait-level type alias) or when the indexer emitted no
    /// enclosing region for the occurrence.
    fn enclosing_definition_for(
        &self,
        file_path: &Path,
        occurrence: &ScipOccurrence,
    ) -> Option<NodeIndex> {
        let ranges = self.file_def_ranges.get(file_path)?;
        let (start_line, end_line) = match occurrence.enclosing_range.as_ref() {
            Some(enc) => {
                let s = (enc.start_line.max(0) as u32).saturating_add(1);
                let e = (enc.end_line.max(0) as u32).saturating_add(1);
                if s <= e { (s, e) } else { (e, s) }
            }
            None => {
                let s = (occurrence.range.start_line.max(0) as u32).saturating_add(1);
                let e = (occurrence.range.end_line.max(0) as u32).saturating_add(1);
                if s <= e { (s, e) } else { (e, s) }
            }
        };
        // The reference is "inside" a definition when the
        // definition's [start, end] contains the reference's
        // [start, end]. We scan the per-file list (already sorted
        // smallest-first) and return the first match — the
        // smallest container is necessarily the innermost.
        for r in ranges {
            if r.start_line <= start_line && end_line <= r.end_line {
                return Some(r.node);
            }
        }
        None
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

            // PR 1h6c: Build the trait-method → implementation-method
            // index from `Implementation` relationships. When a concrete
            // impl method has an `Implementation` relationship to a trait
            // method, record the mapping so `maybe_add_trait_dispatch_call`
            // can fan out to known implementations. The source symbol is
            // the concrete impl method; the target symbol is the trait
            // method it implements.
            if matches!(kind, ScipRelationshipKind::Implementation) {
                let source_sym = self
                    .graph
                    .node_weight(source_symbol_index)
                    .and_then(|n| n.symbol.as_deref());
                let target_sym = self
                    .graph
                    .node_weight(target_symbol_index)
                    .and_then(|n| n.symbol.as_deref());
                if let (Some(source), Some(target)) = (source_sym, target_sym)
                    && symbol_looks_like_impl_method(source, &self.declared_symbols)
                {
                    self.trait_impl_index
                        .entry(target.to_string())
                        .or_default()
                        .push(source.to_string());
                }
            }
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
        // PR 1h6c: Build a set of node indices that are *sources* of
        // `Implements` edges (i.e., concrete implementation methods).
        // An implementation method has an `Implements` edge FROM itself
        // TO the trait method it implements. The trait method is the
        // *target* of that edge, so by collecting sources we get exactly
        // the impl method nodes — not the trait method itself. This lets
        // us stamp the fan-out reason on caller → impl-method
        // `TraitDispatchCall` edges while leaving the direct caller →
        // trait-method edge without the fan-out reason.
        let impl_method_nodes: BTreeSet<NodeIndex> = self
            .edge_accumulator
            .keys()
            .filter(|(_, _, kind)| *kind == RepoGraphEdgeKind::Implements)
            .map(|(source, _, _)| *source)
            .collect();

        for ((source, target, kind), evidence_count) in self.edge_accumulator {
            let (confidence, mut reason) =
                derive_edge_confidence(&self.graph, source, target, kind);
            // PR 1h6c: stamp the fan-out reason on TraitDispatchCall
            // edges whose target is an implementation method (is a
            // source of an Implements edge). This distinguishes caller
            // → impl-method fan-out edges from direct caller →
            // trait-method edges, which carry no explicit reason
            // (or "local-prefix" when a local symbol is involved).
            if kind == RepoGraphEdgeKind::TraitDispatchCall
                && impl_method_nodes.contains(&target)
                && reason.as_deref() != Some("local-prefix")
            {
                reason = Some(super::constants::REASON_TRAIT_DISPATCH_FANOUT.to_string());
            }
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
            galaxy_positions: BTreeMap::new(),
            galaxy_degrees: BTreeMap::new(),
        };

        // PR F3: run modularity-based community detection unless the
        // feature flag is explicitly turned off. The detector is
        // O((V + E) × iterations); on a 12k-node, 150k-edge canonical
        // graph it lands in the ~hundreds-of-ms range — comparable to
        // the SCC pass that already runs in `derive_graph_caches`.
        if crate::communities::detection_enabled() {
            let communities = match self.community_seed_by_crate.take() {
                Some(crate_map) => crate::communities::detect_communities_with_options(
                    &graph,
                    crate::communities::CommunityDetectionOptions {
                        seed_by_crate: Some(crate_map),
                        ..Default::default()
                    },
                ),
                None => crate::communities::detect_communities(&graph),
            };
            graph.install_communities(communities);
        }

        graph
    }
}

/// Best-effort check for "is this SCIP symbol identifier a method that
/// belongs to an in-repo type/interface declaration?" — i.e. the
/// target of a candidate caller → trait-method edge.
///
/// SCIP descriptors end in one of the suffix markers documented in
/// [`scip::types::descriptor::Suffix`]. A trait method is conventionally
/// a `Method`-suffixed descriptor whose parent is `Type` (covers
/// `Trait#method().` for traits, `Struct#method().` for inherent impl
/// methods, `Class#method().` for OOP languages, etc.). We treat the
/// parent as a trait/interface when the parent symbol identifier is
/// present in `declared_symbols` — the conservative "this is an
/// in-repo type whose method we can dispatch on" gate that bounds
/// the synthesized edge to local symbols.
///
/// The helper intentionally returns `false` for symbols the parser
/// couldn't even identify (`local N`-style) and for free functions
/// (no parent type). The caller decides what to do with `false`
/// (silently skip in this task; future fan-out code may record
/// `REASON_TRAIT_DISPATCH_SUPPRESSED` for telemetry).
fn symbol_looks_like_trait_method(symbol: &str, declared_symbols: &BTreeSet<String>) -> bool {
    // Local symbols have no descriptor chain and never resolve to
    // a method on a declared type.
    if crate::scip_parser::is_local_symbol(symbol) {
        return false;
    }
    let parsed = match scip::symbol::parse_symbol(symbol) {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };
    let descriptors = parsed.descriptors;
    if descriptors.len() < 2 {
        return false;
    }
    use scip::types::descriptor::Suffix;
    // Last descriptor must be a method.
    let last = match descriptors.last() {
        Some(d) => d,
        None => return false,
    };
    if last.suffix.enum_value().ok() != Some(Suffix::Method) {
        return false;
    }
    // Parent descriptor must be a type-like suffix.
    let parent = &descriptors[descriptors.len() - 2];
    let parent_suffix = parent.suffix.enum_value().ok();
    if !matches!(
        parent_suffix,
        Some(Suffix::Type) | Some(Suffix::UnspecifiedSuffix)
    ) {
        // UnspecifiedSuffix covers the SCIP `local`/term descriptors
        // rust-analyzer occasionally emits for non-trait methods; we
        // require an explicit `Type` parent for trait methods to
        // avoid false positives on free functions.
        return false;
    }
    // The parent type must be a declared in-repo symbol. Reconstruct
    // the parent symbol's identifier by formatting the parsed symbol
    // with the trailing method descriptor dropped — the SCIP
    // `format_symbol` helper handles the descriptor / package /
    // scheme assembly, and we re-use it for the parent instead of
    // poking at the wire string. The `Symbol` type isn't `Clone`, so
    // we rebuild a fresh symbol with the trimmed descriptor list.
    let parent_descriptor_count = descriptors.len() - 1;
    let parent_identifier = format_parent_symbol_identifier(
        &parsed.scheme,
        parsed.package.as_ref(),
        &descriptors[..parent_descriptor_count],
    );
    declared_symbols.contains(&parent_identifier)
}

/// Best-effort check for "is this SCIP symbol identifier a concrete
/// implementation method that implements a trait method?" Used during
/// trait-impl index construction to filter `Implementation`
/// relationships to only those from concrete impl methods (not from
/// free functions, variables, or other non-method symbols).
///
/// The check mirrors [`symbol_looks_like_trait_method`] but does NOT
/// require the parent type to be declared in-repo — an impl method's
/// parent type (e.g. `StructA`) is always declared (it's the struct
/// being implemented), but we don't need to gate on that since the
/// `Implementation` relationship itself is the authoritative signal.
fn symbol_looks_like_impl_method(symbol: &str, declared_symbols: &BTreeSet<String>) -> bool {
    if crate::scip_parser::is_local_symbol(symbol) {
        return false;
    }
    let parsed = match scip::symbol::parse_symbol(symbol) {
        Ok(parsed) => parsed,
        Err(_) => return false,
    };
    let descriptors = parsed.descriptors;
    if descriptors.len() < 2 {
        return false;
    }
    use scip::types::descriptor::Suffix;
    let last = match descriptors.last() {
        Some(d) => d,
        None => return false,
    };
    if last.suffix.enum_value().ok() != Some(Suffix::Method) {
        return false;
    }
    let parent = &descriptors[descriptors.len() - 2];
    let parent_suffix = parent.suffix.enum_value().ok();
    if !matches!(
        parent_suffix,
        Some(Suffix::Type) | Some(Suffix::UnspecifiedSuffix)
    ) {
        return false;
    }
    // The impl method itself must be a declared in-repo symbol.
    declared_symbols.contains(symbol)
}

/// Format a SCIP `Symbol` carrying only the parent descriptors (no
/// trailing method). Mirrors the logic in
/// `scip::symbol::format_symbol_with` but takes the components
/// directly so the caller doesn't need to mutate the protobuf
/// descriptor list in-place.
fn format_parent_symbol_identifier(
    scheme: &str,
    package: Option<&scip::types::Package>,
    descriptors: &[scip::types::Descriptor],
) -> String {
    use scip::types::descriptor::Suffix;
    let mut parts: Vec<String> = Vec::new();
    parts.push(scheme.to_string());
    parts.push(
        package
            .map(|p| {
                if p.manager.is_empty() {
                    ".".to_string()
                } else {
                    p.manager.clone()
                }
            })
            .unwrap_or_else(|| ".".to_string()),
    );
    parts.push(
        package
            .map(|p| {
                if p.name.is_empty() {
                    ".".to_string()
                } else {
                    p.name.clone()
                }
            })
            .unwrap_or_else(|| ".".to_string()),
    );
    parts.push(
        package
            .map(|p| {
                if p.version.is_empty() {
                    ".".to_string()
                } else {
                    p.version.clone()
                }
            })
            .unwrap_or_else(|| ".".to_string()),
    );
    let mut descriptor_str = String::new();
    for desc in descriptors {
        let name = &desc.name;
        let escaped = if name.chars().all(|ch| {
            ch == '_' || ch == '+' || ch == '-' || ch == '$' || ch.is_ascii_alphanumeric()
        }) {
            name.clone()
        } else {
            format!("`{}`", name.replace('`', "``"))
        };
        match desc.suffix.enum_value().ok() {
            Some(Suffix::Package) | Some(Suffix::Namespace) => {
                descriptor_str.push_str(&format!("{}/", escaped));
            }
            Some(Suffix::Type) => descriptor_str.push_str(&format!("{}#", escaped)),
            Some(Suffix::Term) => descriptor_str.push_str(&format!("{}.", escaped)),
            Some(Suffix::Method) => {
                descriptor_str.push_str(&format!("{}({}).", escaped, desc.disambiguator));
            }
            Some(Suffix::TypeParameter) => descriptor_str.push_str(&format!("[{}]", escaped)),
            Some(Suffix::Parameter) => descriptor_str.push_str(&format!("({})", escaped)),
            Some(Suffix::Macro) => descriptor_str.push_str(&format!("{}!", escaped)),
            Some(Suffix::Meta) => descriptor_str.push_str(&format!("{}:", escaped)),
            Some(Suffix::Local) | Some(Suffix::UnspecifiedSuffix) | None => {}
        }
    }
    parts.push(descriptor_str);
    parts.join(" ")
}
