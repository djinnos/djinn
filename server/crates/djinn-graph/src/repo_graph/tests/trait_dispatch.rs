// Tests for PR 8tu1: caller → trait-method synthesized edges.
//
// The builder materializes a `TraitDispatchCall` edge when a Rust
// reference occurrence resolves to a trait-method symbol AND the
// enclosing caller symbol can be resolved confidently via
// `enclosing_definition_for`. These tests pin the additive behavior:
// the existing file-level `FileReference` / `Reads` / `Writes` edges
// must remain in place, the new edge must carry the trait-dispatch
// kind, and confidence / reason must classify the edge as
// `Inferred` at the 0.70 floor (matches the c6es `edge_confidence_tier`
// contract).

use std::collections::BTreeSet;
use std::path::PathBuf;

use petgraph::visit::EdgeRef;

use super::*;
use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
    ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
};

/// Trait method identifier (the trait is declared, the method has a
/// `Method`-suffixed parent that resolves to the trait type).
/// Uses rust-analyzer's SCIP output: `<scheme> <manager> <package> <version>
/// <descriptors>`. The package `src/traits.rs` is a path with slashes
/// (legal in the package name field) and the version is a simple
/// identifier (`0.1.0`). Descriptors use SCIP suffix markers: `Name#`
/// for types, `name().` for methods.
const TRAIT_METHOD_SYMBOL: &str = "scip-rust pkg src/traits.rs 0.1.0 RuntimeOps#list_jobs().";

/// Trait type identifier (declared so the parent lookup succeeds).
const TRAIT_TYPE_SYMBOL: &str = "scip-rust pkg src/traits.rs 0.1.0 RuntimeOps#";

/// Caller function identifier (the function that invokes the trait
/// method). Lives in a separate file from the trait so we exercise
/// the cross-file caller → trait-method path.
const CALLER_SYMBOL: &str = "scip-rust pkg src/main.rs 0.1.0 run().";

/// Helper: build a definition occurrence with an explicit
/// `(start_line, end_line)` so the caller's range contains the
/// reference.
fn definition_at(symbol: &str, start_line: i32, end_line: i32) -> ScipOccurrence {
    ScipOccurrence {
        symbol: symbol.to_string(),
        range: ScipRange {
            start_line,
            start_character: 0,
            end_line,
            end_character: 1,
        },
        enclosing_range: None,
        roles: BTreeSet::from([ScipSymbolRole::Definition]),
        syntax_kind: None,
        override_documentation: vec![],
    }
}

/// Helper: build a reference occurrence at a specific (line, col).
/// `enclosing_range` is set to the caller's range so
/// `enclosing_definition_for` resolves the caller.
fn reference_at(
    symbol: &str,
    line: i32,
    enclosing_start: i32,
    enclosing_end: i32,
) -> ScipOccurrence {
    ScipOccurrence {
        symbol: symbol.to_string(),
        range: ScipRange {
            start_line: line,
            start_character: 4,
            end_line: line,
            end_character: 14,
        },
        enclosing_range: Some(ScipRange {
            start_line: enclosing_start,
            start_character: 0,
            end_line: enclosing_end,
            end_character: 1,
        }),
        roles: BTreeSet::from([ScipSymbolRole::ReadAccess]),
        syntax_kind: None,
        override_documentation: vec![],
    }
}

/// Build a synthetic index that:
/// - Declares the `RuntimeOps` trait in `src/traits.rs`.
/// - Declares the `RuntimeOps::list_jobs` method in `src/traits.rs`.
/// - Declares the `run` caller function in `src/main.rs`.
/// - Records a reference to the trait method from inside `run` (with
///   `enclosing_range` pointing at `run`'s definition range).
fn trait_dispatch_index() -> ParsedScipIndex {
    let trait_type_symbol = ScipSymbol {
        symbol: TRAIT_TYPE_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("RuntimeOps".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let trait_method_symbol = ScipSymbol {
        symbol: TRAIT_METHOD_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Method),
        display_name: Some("list_jobs".to_string()),
        signature: Some("fn list_jobs(&self) -> Vec<Job>".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let caller_symbol = ScipSymbol {
        symbol: CALLER_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("run".to_string()),
        signature: Some("fn run()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/traits.rs"),
                definitions: vec![
                    definition_at(TRAIT_TYPE_SYMBOL, 0, 5),
                    definition_at(TRAIT_METHOD_SYMBOL, 1, 3),
                ],
                references: vec![],
                occurrences: vec![
                    definition_at(TRAIT_TYPE_SYMBOL, 0, 5),
                    definition_at(TRAIT_METHOD_SYMBOL, 1, 3),
                ],
                symbols: vec![trait_type_symbol, trait_method_symbol],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/main.rs"),
                definitions: vec![definition_at(CALLER_SYMBOL, 0, 10)],
                references: vec![reference_at(TRAIT_METHOD_SYMBOL, 5, 0, 10)],
                occurrences: vec![
                    definition_at(CALLER_SYMBOL, 0, 10),
                    reference_at(TRAIT_METHOD_SYMBOL, 5, 0, 10),
                ],
                symbols: vec![caller_symbol],
            },
        ],
        external_symbols: vec![],
    }
}

#[test]
fn rust_caller_occurrence_resolving_to_trait_method_emits_trait_dispatch_call_edge() {
    let graph = RepoDependencyGraph::build(&[trait_dispatch_index()]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");
    let trait_method_index = graph
        .symbol_node(TRAIT_METHOD_SYMBOL)
        .expect("trait-method symbol must exist");

    // Find the synthesized edge.
    let mut trait_dispatch_edges: Vec<_> = graph
        .graph()
        .edges(caller_index)
        .filter(|edge| edge.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .collect();
    assert_eq!(
        trait_dispatch_edges.len(),
        1,
        "exactly one TraitDispatchCall edge from caller to trait method"
    );
    let edge = trait_dispatch_edges.pop().unwrap();
    assert_eq!(
        edge.target(),
        trait_method_index,
        "edge target must be the trait method symbol"
    );

    // Confidence floor / tier — kind is `Inferred` at 0.70 (c6es
    // contract). The edge is NOT classified as `Ambiguous` because
    // the reason doesn't trigger the suppressed / below-floor
    // substring matches in `edge_confidence_tier`.
    let weight = edge.weight();
    assert!(
        (weight.confidence
            - crate::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall))
        .abs()
            < f64::EPSILON,
        "edge confidence should match the trait-dispatch floor"
    );
    assert_eq!(
        weight.confidence_tier(),
        EdgeConfidenceTier::Inferred,
        "synthesized trait-dispatch edge classifies as Inferred at the floor"
    );

    // The direct caller → trait-method edge carries its provenance via
    // the `TraitDispatchCall` edge *kind*, not via an explicit `reason`
    // string. For non-local symbols, `derive_edge_confidence` leaves the
    // reason as `None` (no local-prefix penalty applied) — the fan-out
    // reason constant (`REASON_TRAIT_DISPATCH_FANOUT`) is only stamped
    // on caller → impl-method edges, NOT on this direct edge.
    assert_ne!(
        weight.reason.as_deref(),
        Some(crate::repo_graph::REASON_TRAIT_DISPATCH_FANOUT),
        "direct caller → trait-method edge must not carry the fan-out reason"
    );
    assert_ne!(
        weight.reason.as_deref(),
        Some(crate::repo_graph::REASON_TRAIT_DISPATCH_SUPPRESSED),
        "direct caller → trait-method edge must not carry the suppressed reason"
    );
}

#[test]
fn trait_dispatch_call_preserves_existing_file_reference_and_reads_edges() {
    let graph = RepoDependencyGraph::build(&[trait_dispatch_index()]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");
    let trait_method_index = graph
        .symbol_node(TRAIT_METHOD_SYMBOL)
        .expect("trait-method symbol must exist");

    // The pre-existing file-level FileReference (caller-file ->
    // trait-method-symbol) MUST remain — the trait-dispatch edge
    // is additive, not replacement. The existing add_reference
    // contract adds this edge from the source FILE node, not the
    // caller symbol.
    let main_file_index = graph.file_node("src/main.rs").expect("main.rs file node");
    let file_to_symbol_ref = graph
        .graph()
        .find_edge(main_file_index, trait_method_index)
        .expect("main.rs -> trait-method FileReference edge must remain");
    let file_to_symbol_ref_kind = graph.graph()[file_to_symbol_ref].kind;
    assert_eq!(
        file_to_symbol_ref_kind,
        RepoGraphEdgeKind::FileReference,
        "main.rs -> trait-method symbol-level FileReference must remain"
    );

    // The inter-file FileReference (main.rs -> traits.rs) must
    // also remain.
    let traits_file_index = graph
        .file_node("src/traits.rs")
        .expect("traits.rs file node");
    let inter_file_ref = graph
        .graph()
        .find_edge(main_file_index, traits_file_index)
        .expect("main.rs -> traits.rs file reference must exist");
    let inter_file_ref_kind = graph.graph()[inter_file_ref].kind;
    assert_eq!(
        inter_file_ref_kind,
        RepoGraphEdgeKind::FileReference,
        "main.rs -> traits.rs file reference must remain a FileReference"
    );

    // The Reads / SymbolReference / Writes edge from the
    // trait-method symbol to its host file must also remain.
    let symbol_to_host_file = graph
        .graph()
        .find_edge(trait_method_index, traits_file_index)
        .expect("trait-method symbol -> traits.rs reads/writes/symbol_reference edge must remain");
    let symbol_to_host_file_kind = graph.graph()[symbol_to_host_file].kind;
    assert!(
        matches!(
            symbol_to_host_file_kind,
            RepoGraphEdgeKind::Reads
                | RepoGraphEdgeKind::SymbolReference
                | RepoGraphEdgeKind::Writes
        ),
        "trait-method symbol -> traits.rs edge should be Reads/SymbolReference/Writes, got {symbol_to_host_file_kind:?}"
    );

    // The synthesized TraitDispatchCall edge sits in addition to
    // the file-level reference — it does not replace it.
    let trait_dispatch_count = graph
        .graph()
        .edges(caller_index)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .count();
    assert_eq!(
        trait_dispatch_count, 1,
        "TraitDispatchCall edge from caller to trait method is present (additive)"
    );
}

#[test]
fn trait_dispatch_edge_is_suppressed_when_caller_cannot_be_resolved() {
    // The reference's enclosing_range is 999..1000 — well outside
    // the caller's actual range (0..10). The builder should NOT
    // stamp a TraitDispatchCall edge in that case.
    let mut index = trait_dispatch_index();
    if let Some(file) = index
        .files
        .iter_mut()
        .find(|f| f.relative_path == Path::new("src/main.rs"))
    {
        // Re-stamp the reference with an out-of-range enclosing
        // range so `enclosing_definition_for` returns None.
        for occ in &mut file.references {
            if occ.symbol == TRAIT_METHOD_SYMBOL {
                occ.enclosing_range = Some(ScipRange {
                    start_line: 999,
                    start_character: 0,
                    end_line: 1000,
                    end_character: 1,
                });
            }
        }
        for occ in &mut file.occurrences {
            if occ.symbol == TRAIT_METHOD_SYMBOL {
                occ.enclosing_range = Some(ScipRange {
                    start_line: 999,
                    start_character: 0,
                    end_line: 1000,
                    end_character: 1,
                });
            }
        }
    }
    let graph = RepoDependencyGraph::build(&[index]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");
    let trait_dispatch_edges = graph
        .graph()
        .edges(caller_index)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .count();
    assert_eq!(
        trait_dispatch_edges, 0,
        "no TraitDispatchCall edge when the caller cannot be resolved confidently"
    );
}

#[test]
fn trait_dispatch_edge_is_suppressed_for_non_rust_file() {
    // Same fixture as trait_dispatch_index but with the file
    // language set to "typescript" — the builder must not stamp
    // trait-dispatch edges for non-Rust files (epic scope).
    let mut index = trait_dispatch_index();
    if let Some(file) = index
        .files
        .iter_mut()
        .find(|f| f.relative_path == Path::new("src/main.rs"))
    {
        file.language = "typescript".to_string();
        for occ in &mut file.occurrences {
            occ.override_documentation.clear();
        }
    }
    let graph = RepoDependencyGraph::build(&[index]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");
    let trait_dispatch_edges = graph
        .graph()
        .edges(caller_index)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .count();
    assert_eq!(
        trait_dispatch_edges, 0,
        "TraitDispatchCall edge is Rust-scoped; TypeScript hosts do not emit it"
    );
}

#[test]
fn trait_dispatch_edge_is_suppressed_for_external_target() {
    // The trait-method symbol is NOT declared in the index (only
    // referenced from main.rs). The builder should not stamp an
    // edge because the target is external.
    let mut index = trait_dispatch_index();
    if let Some(file) = index
        .files
        .iter_mut()
        .find(|f| f.relative_path == Path::new("src/traits.rs"))
    {
        // Drop the trait-method symbol from the file's symbols
        // list, and from definitions / occurrences, so the parser
        // sees it as an undeclared reference.
        file.symbols.retain(|s| s.symbol != TRAIT_METHOD_SYMBOL);
        file.definitions.retain(|d| d.symbol != TRAIT_METHOD_SYMBOL);
        file.occurrences.retain(|o| o.symbol != TRAIT_METHOD_SYMBOL);
    }
    let graph = RepoDependencyGraph::build(&[index]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");
    let trait_dispatch_edges = graph
        .graph()
        .edges(caller_index)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .count();
    assert_eq!(
        trait_dispatch_edges, 0,
        "TraitDispatchCall edge is bounded to in-repo symbols; external targets get no edge"
    );
}

// ── PR 1h6c: Bounded trait-method fan-out tests ──────────────────────────

/// Concrete implementation method: `StructA` implements
/// `RuntimeOps::list_jobs`.
const IMPL_A_METHOD_SYMBOL: &str = "scip-rust pkg src/impl_a.rs 0.1.0 StructA#list_jobs().";
const IMPL_A_TYPE_SYMBOL: &str = "scip-rust pkg src/impl_a.rs 0.1.0 StructA#";

/// Second concrete implementation: `StructB` implements
/// `RuntimeOps::list_jobs`.
const IMPL_B_METHOD_SYMBOL: &str = "scip-rust pkg src/impl_b.rs 0.1.0 StructB#list_jobs().";
const IMPL_B_TYPE_SYMBOL: &str = "scip-rust pkg src/impl_b.rs 0.1.0 StructB#";

/// Build a synthetic index with the trait, two concrete implementations,
/// and a caller that references the trait method. The impl methods have
/// `Implementation` relationships pointing to the trait method.
fn trait_dispatch_fanout_index() -> ParsedScipIndex {
    let trait_type_symbol = ScipSymbol {
        symbol: TRAIT_TYPE_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("RuntimeOps".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let trait_method_symbol = ScipSymbol {
        symbol: TRAIT_METHOD_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Method),
        display_name: Some("list_jobs".to_string()),
        signature: Some("fn list_jobs(&self) -> Vec<Job>".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let caller_symbol = ScipSymbol {
        symbol: CALLER_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("run".to_string()),
        signature: Some("fn run()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    // StructA type symbol.
    let impl_a_type = ScipSymbol {
        symbol: IMPL_A_TYPE_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("StructA".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    // StructA::list_jobs — implements the trait method.
    let impl_a_method = ScipSymbol {
        symbol: IMPL_A_METHOD_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Method),
        display_name: Some("list_jobs".to_string()),
        signature: Some("fn list_jobs(&self) -> Vec<Job>".to_string()),
        documentation: vec![],
        relationships: vec![ScipRelationship {
            source_symbol: IMPL_A_METHOD_SYMBOL.to_string(),
            target_symbol: TRAIT_METHOD_SYMBOL.to_string(),
            kinds: BTreeSet::from([ScipRelationshipKind::Implementation]),
        }],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    // StructB type symbol.
    let impl_b_type = ScipSymbol {
        symbol: IMPL_B_TYPE_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("StructB".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    // StructB::list_jobs — implements the trait method.
    let impl_b_method = ScipSymbol {
        symbol: IMPL_B_METHOD_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Method),
        display_name: Some("list_jobs".to_string()),
        signature: Some("fn list_jobs(&self) -> Vec<Job>".to_string()),
        documentation: vec![],
        relationships: vec![ScipRelationship {
            source_symbol: IMPL_B_METHOD_SYMBOL.to_string(),
            target_symbol: TRAIT_METHOD_SYMBOL.to_string(),
            kinds: BTreeSet::from([ScipRelationshipKind::Implementation]),
        }],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/traits.rs"),
                definitions: vec![
                    definition_at(TRAIT_TYPE_SYMBOL, 0, 5),
                    definition_at(TRAIT_METHOD_SYMBOL, 1, 3),
                ],
                references: vec![],
                occurrences: vec![
                    definition_at(TRAIT_TYPE_SYMBOL, 0, 5),
                    definition_at(TRAIT_METHOD_SYMBOL, 1, 3),
                ],
                symbols: vec![trait_type_symbol, trait_method_symbol],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/impl_a.rs"),
                definitions: vec![
                    definition_at(IMPL_A_TYPE_SYMBOL, 0, 10),
                    definition_at(IMPL_A_METHOD_SYMBOL, 2, 5),
                ],
                references: vec![],
                occurrences: vec![
                    definition_at(IMPL_A_TYPE_SYMBOL, 0, 10),
                    definition_at(IMPL_A_METHOD_SYMBOL, 2, 5),
                ],
                symbols: vec![impl_a_type, impl_a_method],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/impl_b.rs"),
                definitions: vec![
                    definition_at(IMPL_B_TYPE_SYMBOL, 0, 10),
                    definition_at(IMPL_B_METHOD_SYMBOL, 2, 5),
                ],
                references: vec![],
                occurrences: vec![
                    definition_at(IMPL_B_TYPE_SYMBOL, 0, 10),
                    definition_at(IMPL_B_METHOD_SYMBOL, 2, 5),
                ],
                symbols: vec![impl_b_type, impl_b_method],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/main.rs"),
                definitions: vec![definition_at(CALLER_SYMBOL, 0, 10)],
                references: vec![reference_at(TRAIT_METHOD_SYMBOL, 5, 0, 10)],
                occurrences: vec![
                    definition_at(CALLER_SYMBOL, 0, 10),
                    reference_at(TRAIT_METHOD_SYMBOL, 5, 0, 10),
                ],
                symbols: vec![caller_symbol],
            },
        ],
        external_symbols: vec![],
    }
}

/// Build a fanout index with `count` implementations (plus the trait
/// and caller). Each impl method has an `Implementation` relationship
/// to the trait method.
fn trait_dispatch_fanout_index_with_impl_count(count: usize) -> ParsedScipIndex {
    let trait_type_symbol = ScipSymbol {
        symbol: TRAIT_TYPE_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("RuntimeOps".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let trait_method_symbol = ScipSymbol {
        symbol: TRAIT_METHOD_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Method),
        display_name: Some("list_jobs".to_string()),
        signature: Some("fn list_jobs(&self) -> Vec<Job>".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let caller_symbol = ScipSymbol {
        symbol: CALLER_SYMBOL.to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("run".to_string()),
        signature: Some("fn run()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    let mut files = vec![ScipFile {
        language: "rust".to_string(),
        relative_path: PathBuf::from("src/traits.rs"),
        definitions: vec![
            definition_at(TRAIT_TYPE_SYMBOL, 0, 5),
            definition_at(TRAIT_METHOD_SYMBOL, 1, 3),
        ],
        references: vec![],
        occurrences: vec![
            definition_at(TRAIT_TYPE_SYMBOL, 0, 5),
            definition_at(TRAIT_METHOD_SYMBOL, 1, 3),
        ],
        symbols: vec![trait_type_symbol, trait_method_symbol],
    }];

    // Generate `count` impl files.
    for i in 0..count {
        let type_sym = format!("scip-rust pkg src/impl_{i}.rs 0.1.0 Impl{i}#");
        let method_sym = format!("scip-rust pkg src/impl_{i}.rs 0.1.0 Impl{i}#list_jobs().");
        let impl_type = ScipSymbol {
            symbol: type_sym.clone(),
            kind: Some(ScipSymbolKind::Type),
            display_name: Some(format!("Impl{i}")),
            signature: None,
            documentation: vec![],
            relationships: vec![],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
            signature_parts: None,
        };
        let impl_method = ScipSymbol {
            symbol: method_sym.clone(),
            kind: Some(ScipSymbolKind::Method),
            display_name: Some("list_jobs".to_string()),
            signature: Some("fn list_jobs(&self) -> Vec<Job>".to_string()),
            documentation: vec![],
            relationships: vec![ScipRelationship {
                source_symbol: method_sym.clone(),
                target_symbol: TRAIT_METHOD_SYMBOL.to_string(),
                kinds: BTreeSet::from([ScipRelationshipKind::Implementation]),
            }],
            visibility: Some(crate::scip_parser::ScipVisibility::Public),
            signature_parts: None,
        };
        files.push(ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from(format!("src/impl_{i}.rs")),
            definitions: vec![
                definition_at(&type_sym, 0, 10),
                definition_at(&method_sym, 2, 5),
            ],
            references: vec![],
            occurrences: vec![
                definition_at(&type_sym, 0, 10),
                definition_at(&method_sym, 2, 5),
            ],
            symbols: vec![impl_type, impl_method],
        });
    }

    // Caller file.
    files.push(ScipFile {
        language: "rust".to_string(),
        relative_path: PathBuf::from("src/main.rs"),
        definitions: vec![definition_at(CALLER_SYMBOL, 0, 10)],
        references: vec![reference_at(TRAIT_METHOD_SYMBOL, 5, 0, 10)],
        occurrences: vec![
            definition_at(CALLER_SYMBOL, 0, 10),
            reference_at(TRAIT_METHOD_SYMBOL, 5, 0, 10),
        ],
        symbols: vec![caller_symbol],
    });

    ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files,
        external_symbols: vec![],
    }
}

#[test]
fn trait_dispatch_fanout_emits_edges_to_known_implementations_within_cap() {
    let graph = RepoDependencyGraph::build(&[trait_dispatch_fanout_index()]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");
    let trait_method_index = graph
        .symbol_node(TRAIT_METHOD_SYMBOL)
        .expect("trait-method symbol must exist");
    let impl_a_index = graph
        .symbol_node(IMPL_A_METHOD_SYMBOL)
        .expect("StructA impl method must exist");
    let impl_b_index = graph
        .symbol_node(IMPL_B_METHOD_SYMBOL)
        .expect("StructB impl method must exist");

    // Collect all TraitDispatchCall edges from the caller.
    let dispatch_edges: Vec<_> = graph
        .graph()
        .edges(caller_index)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .collect();

    // We expect 3 edges: 1 direct caller → trait-method, plus 2
    // caller → impl-method fan-out edges.
    assert_eq!(
        dispatch_edges.len(),
        3,
        "expected 3 TraitDispatchCall edges (1 direct + 2 fan-out), got {}",
        dispatch_edges.len()
    );

    let targets: BTreeSet<NodeIndex> = dispatch_edges.iter().map(|e| e.target()).collect();
    assert!(
        targets.contains(&trait_method_index),
        "direct caller → trait-method edge must be present"
    );
    assert!(
        targets.contains(&impl_a_index),
        "fan-out to StructA implementation must be present"
    );
    assert!(
        targets.contains(&impl_b_index),
        "fan-out to StructB implementation must be present"
    );

    // The direct caller → trait-method edge should NOT carry the
    // fan-out reason (it has no Implements relationship).
    let direct_edge = dispatch_edges
        .iter()
        .find(|e| e.target() == trait_method_index)
        .expect("direct edge");
    assert_ne!(
        direct_edge.weight().reason.as_deref(),
        Some("trait-dispatch-fanout"),
        "direct caller → trait-method edge should not carry the fan-out reason"
    );

    // The fan-out edges (to impl methods) should carry the fan-out
    // reason, distinguishing them from the direct edge.
    let fanout_a = dispatch_edges
        .iter()
        .find(|e| e.target() == impl_a_index)
        .expect("fan-out A edge");
    assert_eq!(
        fanout_a.weight().reason.as_deref(),
        Some("trait-dispatch-fanout"),
        "fan-out edge to StructA should carry the fan-out reason"
    );

    let fanout_b = dispatch_edges
        .iter()
        .find(|e| e.target() == impl_b_index)
        .expect("fan-out B edge");
    assert_eq!(
        fanout_b.weight().reason.as_deref(),
        Some("trait-dispatch-fanout"),
        "fan-out edge to StructB should carry the fan-out reason"
    );

    // All TraitDispatchCall edges classify as Inferred.
    for edge in &dispatch_edges {
        assert_eq!(
            edge.weight().confidence_tier(),
            EdgeConfidenceTier::Inferred,
            "all TraitDispatchCall edges should be Inferred tier"
        );
    }

    // The Implements edges from impl methods to the trait method
    // must still be present (fan-out does not consume/remove them).
    let impl_a_impl_edges: Vec<_> = graph
        .graph()
        .edges(impl_a_index)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::Implements)
        .collect();
    assert!(
        !impl_a_impl_edges.is_empty(),
        "StructA → trait Implements edge must remain"
    );

    // Criterion 2: the extracted SCIP `Implements` relationship edges
    // must retain their original high-confidence classification — they
    // are directly extracted from SCIP, NOT synthesized, so their
    // confidence value must be unchanged at the `Implements` floor and
    // must NOT carry a synthesized reason.
    let impl_edge = impl_a_impl_edges[0].weight();
    assert!(
        (impl_edge.confidence
            - crate::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::Implements))
        .abs()
            < f64::EPSILON,
        "Implements edge confidence must remain at the extracted floor, got {}",
        impl_edge.confidence
    );
    assert_ne!(
        impl_edge.confidence_tier(),
        EdgeConfidenceTier::Ambiguous,
        "extracted Implements edge must never be classified Ambiguous"
    );
    assert_ne!(
        impl_edge.reason.as_deref(),
        Some(crate::repo_graph::REASON_TRAIT_DISPATCH_FANOUT),
        "extracted Implements edge must not carry a synthesized fan-out reason"
    );
    assert_ne!(
        impl_edge.reason.as_deref(),
        Some(crate::repo_graph::REASON_TRAIT_DISPATCH_CALL),
        "extracted Implements edge must not carry a synthesized dispatch reason"
    );
}

#[test]
fn trait_dispatch_fanout_suppressed_when_impl_count_exceeds_cap() {
    // Build an index with TRAIT_DISPATCH_FANOUT_CAP + 1 implementations.
    // The builder should NOT emit any fan-out edges — only the direct
    // caller → trait-method edge remains.
    let cap_plus_one = crate::repo_graph::TRAIT_DISPATCH_FANOUT_CAP + 1;
    let graph =
        RepoDependencyGraph::build(&[trait_dispatch_fanout_index_with_impl_count(cap_plus_one)]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");

    // Collect all TraitDispatchCall edges from the caller.
    let dispatch_edges: Vec<_> = graph
        .graph()
        .edges(caller_index)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .collect();

    // Only the direct caller → trait-method edge should exist.
    // Fan-out edges are suppressed because impl_count > cap.
    assert_eq!(
        dispatch_edges.len(),
        1,
        "when impl count ({cap_plus_one}) exceeds cap ({cap}), only the \
         direct caller → trait-method edge should exist (got {})",
        dispatch_edges.len(),
        cap = crate::repo_graph::TRAIT_DISPATCH_FANOUT_CAP,
    );

    let trait_method_index = graph
        .symbol_node(TRAIT_METHOD_SYMBOL)
        .expect("trait-method symbol must exist");
    assert_eq!(
        dispatch_edges[0].target(),
        trait_method_index,
        "the single remaining edge must point to the trait method"
    );
}

#[test]
fn trait_dispatch_fanout_emits_edges_at_exact_cap() {
    // Build an index with exactly TRAIT_DISPATCH_FANOUT_CAP implementations.
    // Fan-out should be emitted (count <= cap).
    let graph = RepoDependencyGraph::build(&[trait_dispatch_fanout_index_with_impl_count(
        crate::repo_graph::TRAIT_DISPATCH_FANOUT_CAP,
    )]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");

    let dispatch_edges: Vec<_> = graph
        .graph()
        .edges(caller_index)
        .filter(|e| e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall)
        .collect();

    // 1 direct + cap fan-out edges.
    let expected = 1 + crate::repo_graph::TRAIT_DISPATCH_FANOUT_CAP;
    assert_eq!(
        dispatch_edges.len(),
        expected,
        "at exact cap, all fan-out edges should be emitted (expected {expected}, got {})",
        dispatch_edges.len()
    );
}

#[test]
fn trait_dispatch_synthesized_edge_provenance_is_distinguishable_from_extracted_edges() {
    // Consolidation regression: the synthesized trait-dispatch edges
    // must be distinguishable by kind/confidence/reason from the
    // directly-extracted SCIP relationship edges. This pins the full
    // confidence/reason/tier contract in a single test so future
    // refactors that touch `derive_edge_confidence` or `finish()`
    // surface a clear failure if the provenance distinction breaks.
    let graph = RepoDependencyGraph::build(&[trait_dispatch_fanout_index()]);

    let caller_index = graph
        .symbol_node(CALLER_SYMBOL)
        .expect("caller symbol must exist");
    let trait_method_index = graph
        .symbol_node(TRAIT_METHOD_SYMBOL)
        .expect("trait-method symbol must exist");
    let impl_a_index = graph
        .symbol_node(IMPL_A_METHOD_SYMBOL)
        .expect("StructA impl method must exist");

    let dispatch_floor =
        crate::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::TraitDispatchCall);
    let implements_floor = crate::repo_graph::edge_confidence_floor(RepoGraphEdgeKind::Implements);

    // The synthesized TraitDispatchCall confidence floor (0.70) must be
    // strictly below the extracted Implements floor (0.85) so
    // `min_confidence` filtering can separate the two populations.
    assert!(
        dispatch_floor < implements_floor,
        "synthesized dispatch floor ({dispatch_floor}) must be below the extracted Implements floor ({implements_floor})"
    );

    // ── Direct caller → trait-method edge ──────────────────────────
    let direct_edge = graph
        .graph()
        .edges(caller_index)
        .find(|e| {
            e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall
                && e.target() == trait_method_index
        })
        .map(|e| e.weight().clone())
        .expect("direct caller → trait-method TraitDispatchCall edge");
    assert_eq!(
        direct_edge.confidence_tier(),
        EdgeConfidenceTier::Inferred,
        "direct dispatch edge must be Inferred tier"
    );
    assert!(
        (direct_edge.confidence - dispatch_floor).abs() < f64::EPSILON,
        "direct dispatch edge confidence must be at the floor"
    );
    // The direct edge carries provenance via the *kind*, not via the
    // fan-out reason constant.
    assert_ne!(
        direct_edge.reason.as_deref(),
        Some(crate::repo_graph::REASON_TRAIT_DISPATCH_FANOUT),
    );

    // ── Fan-out caller → impl-method edge ──────────────────────────
    let fanout_edge = graph
        .graph()
        .edges(caller_index)
        .find(|e| {
            e.weight().kind == RepoGraphEdgeKind::TraitDispatchCall && e.target() == impl_a_index
        })
        .map(|e| e.weight().clone())
        .expect("caller → StructA fan-out TraitDispatchCall edge");
    assert_eq!(
        fanout_edge.confidence_tier(),
        EdgeConfidenceTier::Inferred,
        "fan-out dispatch edge must be Inferred tier"
    );
    assert!(
        (fanout_edge.confidence - dispatch_floor).abs() < f64::EPSILON,
        "fan-out dispatch edge confidence must be at the floor"
    );
    // The fan-out edge is positively identified by its reason — this
    // is the load-bearing provenance signal for distinguishing direct
    // vs. fan-out dispatch edges.
    assert_eq!(
        fanout_edge.reason.as_deref(),
        Some(crate::repo_graph::REASON_TRAIT_DISPATCH_FANOUT),
        "fan-out edge must carry REASON_TRAIT_DISPATCH_FANOUT"
    );

    // ── Extracted Implements edge (impl → trait method) ────────────
    let implements_edge = graph
        .graph()
        .edges(impl_a_index)
        .find(|e| e.weight().kind == RepoGraphEdgeKind::Implements)
        .map(|e| e.weight().clone())
        .expect("StructA → trait-method Implements edge");
    // The extracted Implements edge retains its confidence at the
    // higher extracted floor, and is NOT classified as Ambiguous.
    assert!(
        (implements_edge.confidence - implements_floor).abs() < f64::EPSILON,
        "extracted Implements edge confidence must be unchanged at its floor, got {}",
        implements_edge.confidence
    );
    assert_ne!(
        implements_edge.confidence_tier(),
        EdgeConfidenceTier::Ambiguous,
        "extracted Implements edge must never be Ambiguous"
    );
    // The Implements edge must NOT carry any synthesized dispatch
    // reason — it is a directly-extracted SCIP relationship.
    assert_ne!(
        implements_edge.reason.as_deref(),
        Some(crate::repo_graph::REASON_TRAIT_DISPATCH_FANOUT),
    );
    assert_ne!(
        implements_edge.reason.as_deref(),
        Some(crate::repo_graph::REASON_TRAIT_DISPATCH_CALL),
    );
}
