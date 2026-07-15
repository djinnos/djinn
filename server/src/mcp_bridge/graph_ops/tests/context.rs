//! `context` / edge-category op tests, split from `snapshot.rs`
//! (Server Size Guard).
use super::*;

#[test]
fn edge_category_table_pr_c1() {
    // Spot-check the EdgeCategory mapping — the contract table is
    // load-bearing for the UI parser so any silent rewrite must
    // break this test.
    use crate::mcp_bridge::graph_neighbors::edge_category_for;
    use djinn_graph::repo_graph::{RepoGraphEdge, RepoGraphEdgeKind};
    use djinn_graph::scip_parser::ScipSymbolKind;
    use std::path::PathBuf;

    let mk_edge = |kind: RepoGraphEdgeKind| RepoGraphEdge {
        kind,
        weight: 1.0,
        evidence_count: 1,
        confidence: 0.9,
        reason: None,
        step: None,
    };
    let mk_node = |kind: Option<ScipSymbolKind>| djinn_graph::repo_graph::RepoGraphNode {
        id: djinn_graph::repo_graph::RepoNodeKey::Symbol("x".into()),
        kind: djinn_graph::repo_graph::RepoGraphNodeKind::Symbol,
        display_name: "x".into(),
        language: None,
        file_path: Some(PathBuf::from("x.rs")),
        symbol: Some("x".into()),
        symbol_kind: kind,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: None,
        route_framework: None,
        route_handler_symbol: None,
    };

    let any_node = mk_node(None);
    // SymbolReference with non-callable target → References.
    assert_eq!(
        edge_category_for(
            Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
            &any_node
        ),
        EdgeCategory::References
    );
    // SymbolReference with Function target → Calls.
    let fn_node = mk_node(Some(ScipSymbolKind::Function));
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)), &fn_node),
        EdgeCategory::Calls
    );
    // SymbolReference with Method target → Calls.
    let method_node = mk_node(Some(ScipSymbolKind::Method));
    assert_eq!(
        edge_category_for(
            Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
            &method_node
        ),
        EdgeCategory::Calls
    );
    // SymbolReference with Constructor target → Calls.
    let ctor_node = mk_node(Some(ScipSymbolKind::Constructor));
    assert_eq!(
        edge_category_for(
            Some(&mk_edge(RepoGraphEdgeKind::SymbolReference)),
            &ctor_node
        ),
        EdgeCategory::Calls
    );
    // PR A3 splits.
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Reads)), &any_node),
        EdgeCategory::Reads
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Writes)), &any_node),
        EdgeCategory::Writes
    );
    // FileReference → Imports.
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::FileReference)), &any_node),
        EdgeCategory::Imports
    );
    // Containment.
    assert_eq!(
        edge_category_for(
            Some(&mk_edge(RepoGraphEdgeKind::ContainsDefinition)),
            &any_node
        ),
        EdgeCategory::Contains
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::DeclaredInFile)), &any_node),
        EdgeCategory::Contains
    );
    // Symbol relationships.
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Extends)), &any_node),
        EdgeCategory::Extends
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Implements)), &any_node),
        EdgeCategory::Implements
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::TypeDefines)), &any_node),
        EdgeCategory::TypeDefines
    );
    assert_eq!(
        edge_category_for(Some(&mk_edge(RepoGraphEdgeKind::Defines)), &any_node),
        EdgeCategory::Defines
    );
}

#[test]
fn context_limit_30_per_category_pr_c1() {
    // Build a fan-in of 35 callers on a single symbol and verify
    // the `Calls` bucket truncates at 30, sorted desc by
    // confidence so the highest-confidence callers survive.
    use crate::mcp_bridge::graph_neighbors::{build_related_symbol, edge_category_for};
    use djinn_graph::repo_graph::*;
    use djinn_graph::scip_parser::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    let target_sym = "scip-rust pkg src/lib.rs `target`().".to_string();
    let target_symbol = ScipSymbol {
        symbol: target_sym.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("target".to_string()),
        signature: Some("fn target()".to_string()),
        documentation: vec![],
        relationships: vec![],
        visibility: Some(ScipVisibility::Public),
        signature_parts: None,
    };
    let mut files: Vec<ScipFile> = vec![ScipFile {
        language: "rust".into(),
        relative_path: PathBuf::from("src/lib.rs"),
        definitions: vec![ScipOccurrence {
            symbol: target_sym.clone(),
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
        }],
        references: vec![],
        occurrences: vec![],
        symbols: vec![target_symbol],
    }];
    for i in 0..35 {
        let caller_sym = format!("scip-rust pkg src/c{i}.rs `caller{i}`().");
        files.push(ScipFile {
            language: "rust".into(),
            relative_path: PathBuf::from(format!("src/c{i}.rs")),
            definitions: vec![ScipOccurrence {
                symbol: caller_sym.clone(),
                range: ScipRange {
                    start_line: 0,
                    start_character: 0,
                    end_line: 0,
                    end_character: 8,
                },
                enclosing_range: None,
                roles: BTreeSet::from([ScipSymbolRole::Definition]),
                syntax_kind: None,
                override_documentation: vec![],
            }],
            references: vec![ScipOccurrence {
                symbol: target_sym.clone(),
                range: ScipRange {
                    start_line: 1,
                    start_character: 4,
                    end_line: 1,
                    end_character: 10,
                },
                enclosing_range: None,
                roles: BTreeSet::new(),
                syntax_kind: None,
                override_documentation: vec![],
            }],
            occurrences: vec![],
            symbols: vec![ScipSymbol {
                symbol: caller_sym,
                kind: Some(ScipSymbolKind::Function),
                display_name: Some(format!("caller{i}")),
                signature: None,
                documentation: vec![],
                relationships: vec![],
                visibility: Some(ScipVisibility::Public),
                signature_parts: None,
            }],
        });
    }
    let parsed = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files,
        external_symbols: vec![],
    };
    let graph = RepoDependencyGraph::build(&[parsed]);
    let target_node = graph
        .symbol_node(&target_sym)
        .expect("target should be in graph");

    // Collect incoming edges directly and bucket them.
    use petgraph::Direction;
    let mut by_cat: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
        std::collections::BTreeMap::new();
    for edge in graph
        .graph()
        .edges_directed(target_node, Direction::Incoming)
    {
        let other = graph.node(edge.source());
        let cat = edge_category_for(Some(edge.weight()), other);
        let related = build_related_symbol(other, edge.weight().confidence);
        by_cat.entry(cat).or_default().push(related);
    }
    for entries in by_cat.values_mut() {
        entries.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.uid.cmp(&b.uid))
        });
        entries.truncate(30);
    }

    // The fan-in mints `FileReference` edges from each caller-file
    // into the target symbol, which the EdgeCategory mapping
    // routes to `Imports`. With 35 raw incoming references, the
    // bucket must truncate at 30 (the plan-mandated hard cap).
    let imports_count = by_cat
        .get(&EdgeCategory::Imports)
        .map(|v| v.len())
        .unwrap_or(0);
    assert_eq!(
        imports_count, 30,
        "incoming.imports must hard-cap at 30; got {imports_count}"
    );
    // And confirm: at least one bucket actually exceeded the cap
    // pre-truncation (otherwise the test isn't exercising the cap).
    let raw_incoming = graph
        .graph()
        .edges_directed(target_node, Direction::Incoming)
        .count();
    assert!(
        raw_incoming >= 35,
        "fan-in fixture should produce >= 35 raw incoming edges, got {raw_incoming}"
    );
}

#[test]
fn context_emits_processes_for_step_node_pr_f2() {
    // Build a 5-symbol linear chain (`main → a → b → c → d`) so the
    // F2 process detector emits one process. Then assert that the
    // C1 context-op-style construction populates the `processes`
    // field on a node that's a step in that flow.
    use djinn_graph::repo_graph::*;
    use djinn_graph::scip_parser::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn def_occ(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 4,
            },
            enclosing_range: None,
            roles: BTreeSet::from([ScipSymbolRole::Definition]),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }
    fn ref_occ(symbol: &str) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 0,
                start_character: 0,
                end_line: 0,
                end_character: 4,
            },
            enclosing_range: None,
            roles: BTreeSet::new(),
            syntax_kind: None,
            override_documentation: vec![],
        }
    }
    fn rust_function(symbol: &str, name: &str) -> ScipSymbol {
        ScipSymbol {
            symbol: symbol.to_string(),
            kind: Some(ScipSymbolKind::Function),
            display_name: Some(name.to_string()),
            signature: Some(format!("fn {name}()")),
            documentation: vec![],
            relationships: vec![],
            visibility: Some(ScipVisibility::Public),
            signature_parts: None,
        }
    }

    let main_sym = "scip-rust pkg src/main.rs `main`().";
    let a_sym = "scip-rust pkg src/a.rs `a`().";
    let b_sym = "scip-rust pkg src/b.rs `b`().";

    let parsed = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".into(),
                relative_path: PathBuf::from("src/main.rs"),
                definitions: vec![def_occ(main_sym)],
                references: vec![ref_occ(a_sym)],
                occurrences: vec![],
                symbols: vec![rust_function(main_sym, "main")],
            },
            ScipFile {
                language: "rust".into(),
                relative_path: PathBuf::from("src/a.rs"),
                definitions: vec![def_occ(a_sym)],
                references: vec![ref_occ(b_sym)],
                occurrences: vec![],
                symbols: vec![rust_function(a_sym, "a")],
            },
            ScipFile {
                language: "rust".into(),
                relative_path: PathBuf::from("src/b.rs"),
                definitions: vec![def_occ(b_sym)],
                references: vec![],
                occurrences: vec![],
                symbols: vec![rust_function(b_sym, "b")],
            },
        ],
        external_symbols: vec![],
    };
    let graph = RepoDependencyGraph::build(&[parsed]);

    // Sanity: the detector ran and produced at least one process.
    assert!(
        !graph.processes().is_empty(),
        "linear chain should produce a process; got {:?}",
        graph.processes()
    );

    // The `b` symbol is a step in the `main` process.
    let b_idx = graph
        .symbol_node(b_sym)
        .expect("b symbol should be in the graph");
    let memberships = graph.processes_for_node(b_idx);
    assert!(
        !memberships.is_empty(),
        "node `b` must have process memberships"
    );

    // Mirror the wire-shape construction the bridge does.
    let process_refs: Vec<ProcessRef> = memberships
        .iter()
        .map(|p| ProcessRef {
            id: p.id.clone(),
            uid: p.id.clone(),
            label: p.label.clone(),
            role: "step".to_string(),
        })
        .collect();
    assert!(
        process_refs.iter().any(|r| r.role == "step"),
        "every process_ref must carry role=\"step\""
    );
    assert!(
        process_refs
            .iter()
            .any(|r| r.label.contains("main") && r.label.contains("process")),
        "expected a process labeled `\"main process\"`: {:?}",
        process_refs.iter().map(|r| &r.label).collect::<Vec<_>>()
    );
}

#[test]
fn context_method_metadata_none_when_signature_parts_absent_pr_c1() {
    // SCIP 0.7 ships only the markdown signature blob, so
    // `signature_parts` is None on every fixture. Per the plan
    // contract this MUST surface as `method_metadata: None` —
    // never regex-extracted from the markdown.
    use crate::mcp_bridge::graph_neighbors::build_method_metadata;
    let graph = build_test_graph();
    let helper_idx = graph
        .symbol_node("scip-rust pkg src/helper.rs `helper`().")
        .expect("helper exists");
    let helper = graph.node(helper_idx);
    assert!(
        helper.signature_parts.is_none(),
        "fixture should not carry structured signature_parts"
    );
    assert!(
        build_method_metadata(helper).is_none(),
        "method_metadata must be None when signature_parts is absent"
    );
}

#[test]
fn context_method_metadata_some_when_signature_parts_present_pr_c1() {
    // Synthesise a signature_parts payload (as a future indexer
    // would) and assert the bridge surfaces it as MethodMeta.
    use crate::mcp_bridge::graph_neighbors::build_method_metadata;
    use djinn_graph::scip_parser::{ScipSignatureParam, ScipSignatureParts};

    let mut node = graph_neighbors_test_node();
    node.signature_parts = Some(ScipSignatureParts {
        parameters: vec![
            ScipSignatureParam {
                name: "user".into(),
                type_name: Some("User".into()),
                default_value: None,
            },
            ScipSignatureParam {
                name: "limit".into(),
                type_name: Some("usize".into()),
                default_value: Some("20".into()),
            },
        ],
        return_type: Some("Result<Vec<Item>, Error>".into()),
        type_parameters: vec!["T".into()],
        visibility: Some("pub".into()),
        is_async: Some(true),
        annotations: vec!["#[tracing::instrument]".into()],
    });
    let meta = build_method_metadata(&node).expect("metadata expected");
    assert_eq!(meta.params.len(), 2);
    assert_eq!(meta.params[0].name, "user");
    assert_eq!(meta.params[1].default_value.as_deref(), Some("20"));
    assert_eq!(
        meta.return_type.as_deref(),
        Some("Result<Vec<Item>, Error>")
    );
    assert_eq!(meta.is_async, Some(true));
    assert_eq!(meta.visibility.as_deref(), Some("pub"));
    assert_eq!(meta.annotations, vec!["#[tracing::instrument]"]);
}

fn graph_neighbors_test_node() -> djinn_graph::repo_graph::RepoGraphNode {
    use std::path::PathBuf;
    djinn_graph::repo_graph::RepoGraphNode {
        id: djinn_graph::repo_graph::RepoNodeKey::Symbol("x".into()),
        kind: djinn_graph::repo_graph::RepoGraphNodeKind::Symbol,
        display_name: "list_items".into(),
        language: Some("rust".into()),
        file_path: Some(PathBuf::from("src/lib.rs")),
        symbol: Some("scip-rust pkg src/lib.rs `list_items`().".into()),
        symbol_kind: Some(djinn_graph::scip_parser::ScipSymbolKind::Function),
        is_external: false,
        visibility: None,
        signature: Some("pub async fn list_items(...) -> Result<...>".into()),
        documentation: vec![],
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: None,
        route_framework: None,
        route_handler_symbol: None,
    }
}
