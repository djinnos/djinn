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
    ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipSymbol, ScipSymbolKind,
    ScipSymbolRole,
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
        .find(|f| f.relative_path == PathBuf::from("src/main.rs"))
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
        .find(|f| f.relative_path == PathBuf::from("src/main.rs"))
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
        .find(|f| f.relative_path == PathBuf::from("src/traits.rs"))
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
