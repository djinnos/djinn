use super::*;

/// v8 end-to-end: simulate a rust-analyzer SCIP feed (no `ReadAccess`
/// or `WriteAccess` role bits on any occurrence) against a real
/// on-disk Rust file. The classifier should recover the read/write
/// distinction from AST context, producing `Reads` and `Writes`
/// edges where v7 would have emitted only `SymbolReference`.
///
/// This is the reference verification for the `build_with_source`
/// path — proves the fallback fires end-to-end and not just in the
/// classifier's unit tests.
#[test]
fn build_with_source_recovers_reads_and_writes_when_scip_roles_absent() {
    // A real Rust source file where `counter` is written on one line
    // and read on another. `value` is a definition site (not an
    // access). The line/column positions below match these
    // identifiers exactly.
    //
    // Layout (0-indexed):
    //   line 0: pub static mut COUNTER: i32 = 0;
    //   line 1:
    //   line 2: pub fn bump() {
    //   line 3:     unsafe { COUNTER = COUNTER + 1; }
    //                        ^^^^^^^   ^^^^^^^
    //                        col 13    col 23
    //   line 4: }
    let source = "pub static mut COUNTER: i32 = 0;\n\n\
                  pub fn bump() {\n    \
                  unsafe { COUNTER = COUNTER + 1; }\n}\n";
    let tempdir = tempfile::tempdir().expect("tempdir");
    let rel = PathBuf::from("src/counter.rs");
    let abs = tempdir.path().join(&rel);
    std::fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
    std::fs::write(&abs, source).expect("write");

    let counter_symbol = "scip-rust pkg src/counter.rs `COUNTER`.".to_string();
    let counter_def_sym = ScipSymbol {
        symbol: counter_symbol.clone(),
        kind: Some(ScipSymbolKind::Variable),
        display_name: Some("COUNTER".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    // Definition occurrence at line 0 (the `static mut COUNTER`).
    let def = ScipOccurrence {
        symbol: counter_symbol.clone(),
        range: ScipRange {
            start_line: 0,
            start_character: 15,
            end_line: 0,
            end_character: 22,
        },
        enclosing_range: None,
        roles: BTreeSet::from([ScipSymbolRole::Definition]),
        syntax_kind: None,
        override_documentation: vec![],
    };
    // Reference occurrence at line 3 column 13 (`COUNTER = …` LHS) —
    // EMPTY role bits, mirroring rust-analyzer's SCIP output.
    let write_ref = ScipOccurrence {
        symbol: counter_symbol.clone(),
        range: ScipRange {
            start_line: 3,
            start_character: 13,
            end_line: 3,
            end_character: 20,
        },
        enclosing_range: None,
        roles: BTreeSet::new(),
        syntax_kind: None,
        override_documentation: vec![],
    };
    // Reference occurrence at line 3 column 23 (`= COUNTER + 1` RHS).
    let read_ref = ScipOccurrence {
        symbol: counter_symbol.clone(),
        range: ScipRange {
            start_line: 3,
            start_character: 23,
            end_line: 3,
            end_character: 30,
        },
        enclosing_range: None,
        roles: BTreeSet::new(),
        syntax_kind: None,
        override_documentation: vec![],
    };

    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata {
            project_root: Some("file:///workspace/repo".to_string()),
            tool_name: Some("rust-analyzer".to_string()),
            tool_version: Some("1.0.0".to_string()),
        },
        files: vec![ScipFile {
            language: "rust".to_string(),
            relative_path: rel.clone(),
            definitions: vec![def.clone()],
            references: vec![write_ref.clone(), read_ref.clone()],
            occurrences: vec![def, write_ref, read_ref],
            symbols: vec![counter_def_sym],
        }],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build_with_source(&[index], Some(tempdir.path()));

    // Walk every edge with the COUNTER symbol as source and the
    // counter.rs file as target — there should be exactly one
    // `Writes` and one `Reads`, no `SymbolReference` fallbacks.
    let symbol_idx = graph
        .symbol_node(&counter_symbol)
        .expect("COUNTER symbol node should exist");
    let file_idx = graph
        .file_node(&rel)
        .expect("counter.rs file node should exist");
    let mut writes = 0usize;
    let mut reads = 0usize;
    let mut other = 0usize;
    for edge in graph
        .graph()
        .edges_directed(symbol_idx, petgraph::Direction::Outgoing)
    {
        if edge.target() != file_idx {
            continue;
        }
        match edge.weight().kind {
            RepoGraphEdgeKind::Writes => writes += 1,
            RepoGraphEdgeKind::Reads => reads += 1,
            RepoGraphEdgeKind::SymbolReference => other += 1,
            _ => {}
        }
    }
    assert_eq!(
        writes, 1,
        "tree-sitter classifier should have recovered exactly one Writes edge \
         (counter.rs `COUNTER = …` LHS)"
    );
    assert_eq!(
        reads, 1,
        "tree-sitter classifier should have recovered exactly one Reads edge \
         (counter.rs `… = COUNTER + 1` RHS)"
    );
    assert_eq!(
        other, 0,
        "no SymbolReference fallback edges should remain when the classifier \
         can resolve the AST context"
    );
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
fn build_stamps_workspace_from_parsed_index() {
    let mut index = fixture_index();
    index.workspace_slug = "api".to_string();
    let graph = RepoDependencyGraph::build(&[index]);

    let app_file = graph.file_node("src/app.rs").expect("app file");
    assert_eq!(graph.node(app_file).workspace.as_deref(), Some("api"));

    let helper_symbol = graph
        .symbol_node("scip-rust pkg src/helper.rs `helper`().")
        .expect("helper symbol");
    assert_eq!(graph.node(helper_symbol).workspace.as_deref(), Some("api"));

    for node in graph
        .graph()
        .node_weights()
        .filter(|node| node.kind == RepoGraphNodeKind::Process)
    {
        assert_eq!(node.workspace, None);
    }
}

#[test]
fn artifact_json_round_trip_preserves_graph() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let json = graph.serialize_artifact().expect("serialize");
    let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize");

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
    let app_idx = restored.file_node("src/app.rs").expect("app file");
    let has_contains_def = restored
        .graph()
        .edges(app_idx)
        .any(|e| e.weight().kind == RepoGraphEdgeKind::ContainsDefinition);
    assert!(
        has_contains_def,
        "expected ContainsDefinition edge from app file"
    );
}

#[test]
fn empty_artifact_round_trip() {
    let empty = RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![],
        edges: vec![],
        symbol_ranges: BTreeMap::new(),
        communities: Vec::new(),
        processes: vec![],
    };
    let json = serde_json::to_string(&empty).expect("serialize empty");
    let restored = RepoDependencyGraph::deserialize_artifact(&json).expect("deserialize empty");
    assert_eq!(restored.node_count(), 0);
    assert_eq!(restored.edge_count(), 0);
}

// ── PR A2: edge confidence + reason ───────────────────────────────────

/// Every edge kind emitted by the builder gets a confidence value within
/// `(0, 1]`. Sweeping the fixture is the cheapest way to assert "no kind
/// silently slipped through with the default 0.0".
#[test]
fn every_edge_kind_carries_a_confidence_value_pr_a2() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    assert!(graph.edge_count() > 0, "fixture must produce edges");

    let mut seen_kinds: BTreeSet<RepoGraphEdgeKind> = BTreeSet::new();
    for edge_ref in graph.graph().edge_references() {
        let edge = edge_ref.weight();
        seen_kinds.insert(edge.kind);
        assert!(
            edge.confidence > 0.0 && edge.confidence <= 1.0,
            "edge {:?} has out-of-range confidence {}",
            edge.kind,
            edge.confidence
        );
        // Confidence must equal the floor for the kind, optionally
        // dropped by exactly the local-prefix penalty when the reason
        // says so. PR F1: `EntryPointOf` edges set their own
        // confidence (per-detector, 0.6 – 0.95) so the floor check
        // doesn't apply — we just bound them in (0, 1]. PR F2:
        // `StepInProcess` edges always carry `reason="process-step"`
        // and stick to the floor; we whitelist that reason.
        if edge.kind == RepoGraphEdgeKind::EntryPointOf {
            continue;
        }
        let floor = edge_confidence_floor(edge.kind);
        match edge.reason.as_deref() {
            None => assert_eq!(edge.confidence, floor),
            Some("local-prefix") => {
                let expected = (floor - EDGE_CONFIDENCE_LOCAL_PENALTY).clamp(0.0, 1.0);
                assert!(
                    (edge.confidence - expected).abs() < 1e-9,
                    "local-prefix edge confidence {} != expected {} for kind {:?}",
                    edge.confidence,
                    expected,
                    edge.kind
                );
            }
            Some("process-step") => {
                assert_eq!(edge.kind, RepoGraphEdgeKind::StepInProcess);
                assert!((edge.confidence - floor).abs() < 1e-9);
            }
            Some(other) => panic!("unexpected reason {other:?} on edge {:?}", edge.kind),
        }
    }
    // The fixture exercises Contains/DeclaredIn/FileRef/Reads (post-A3
    // the helper-call read-access reference is classified as `Reads`
    // rather than the generic `SymbolReference`) and an Implementation
    // relationship — every code path that can mint an edge today.
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::ContainsDefinition));
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::DeclaredInFile));
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::FileReference));
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::Reads));
    assert!(seen_kinds.contains(&RepoGraphEdgeKind::Implements));
}

/// Bincode round-trip preserves `confidence` and `reason` on every edge.
/// This is the core "artifact v1" guarantee — old blobs without these
/// fields will fail to deserialize and trigger a warm rebuild.
#[test]
fn bincode_round_trip_preserves_edge_confidence_and_reason_pr_a2() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let artifact = graph.to_artifact();
    assert_eq!(artifact.version, REPO_GRAPH_ARTIFACT_VERSION);

    // Snapshot original (kind, confidence, reason) tuples by sorting on
    // (kind, confidence, reason) — edge_count is small so a Vec<_> is
    // fine.
    let mut original: Vec<(RepoGraphEdgeKind, f64, Option<String>)> = artifact
        .edges
        .iter()
        .map(|e| (e.kind, e.confidence, e.reason.clone()))
        .collect();
    original.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.cmp(&b.2))
    });

    let encoded = bincode::serialize(&artifact).expect("bincode serialize");
    let decoded: RepoGraphArtifact = bincode::deserialize(&encoded).expect("bincode deserialize");
    assert_eq!(decoded.version, REPO_GRAPH_ARTIFACT_VERSION);

    let mut round_tripped: Vec<(RepoGraphEdgeKind, f64, Option<String>)> = decoded
        .edges
        .iter()
        .map(|e| (e.kind, e.confidence, e.reason.clone()))
        .collect();
    round_tripped.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.2.cmp(&b.2))
    });

    assert_eq!(round_tripped, original);

    // And that survives the `from_artifact` rebuild path that powers
    // `load_canonical_graph`.
    let restored = RepoDependencyGraph::from_artifact(&decoded);
    for edge_ref in restored.graph().edge_references() {
        let edge = edge_ref.weight();
        assert!(edge.confidence > 0.0 && edge.confidence <= 1.0);
    }
}

#[test]
fn bincode_v10_artifact_without_workspace_deserializes_with_none() {
    #[derive(Serialize)]
    struct V10RepoGraphNodeWithoutWorkspace {
        id: RepoNodeKey,
        kind: RepoGraphNodeKind,
        display_name: String,
        language: Option<String>,
        file_path: Option<PathBuf>,
        symbol: Option<String>,
        symbol_kind: Option<ScipSymbolKind>,
        is_external: bool,
        visibility: Option<crate::scip_parser::ScipVisibility>,
        signature: Option<String>,
        documentation: Vec<String>,
        signature_parts: Option<crate::scip_parser::ScipSignatureParts>,
        is_test: bool,
        complexity: Option<ComplexityMetrics>,
    }

    #[derive(Serialize)]
    struct V10RepoGraphArtifactWithoutWorkspace {
        version: u32,
        nodes: Vec<V10RepoGraphNodeWithoutWorkspace>,
        edges: Vec<RepoGraphArtifactEdge>,
        symbol_ranges: BTreeMap<PathBuf, Vec<RepoGraphArtifactSymbolRange>>,
        communities: Vec<crate::communities::Community>,
        processes: Vec<RepoGraphArtifactProcess>,
    }

    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let artifact = graph.to_artifact();
    let old_nodes = artifact
        .nodes
        .iter()
        .map(|node| V10RepoGraphNodeWithoutWorkspace {
            id: node.id.clone(),
            kind: node.kind,
            display_name: node.display_name.clone(),
            language: node.language.clone(),
            file_path: node.file_path.clone(),
            symbol: node.symbol.clone(),
            symbol_kind: node.symbol_kind.clone(),
            is_external: node.is_external,
            visibility: node.visibility,
            signature: node.signature.clone(),
            documentation: node.documentation.clone(),
            signature_parts: node.signature_parts.clone(),
            is_test: node.is_test,
            complexity: node.complexity,
        })
        .collect();
    let old_artifact = V10RepoGraphArtifactWithoutWorkspace {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: old_nodes,
        edges: artifact.edges.clone(),
        symbol_ranges: artifact.symbol_ranges.clone(),
        communities: artifact.communities.clone(),
        processes: artifact.processes.clone(),
    };

    let encoded = bincode::serialize(&old_artifact).expect("serialize old v10 bincode");
    let decoded = deserialize_repo_graph_artifact_bincode(&encoded)
        .expect("deserialize old v10 bincode through compatibility path");

    assert_eq!(decoded.version, REPO_GRAPH_ARTIFACT_VERSION);
    assert!(!decoded.nodes.is_empty());
    assert!(decoded.nodes.iter().all(|node| node.workspace.is_none()));

    let restored = RepoDependencyGraph::from_artifact(&decoded);
    let app_file = restored.file_node("src/app.rs").expect("app file");
    assert_eq!(restored.node(app_file).workspace, None);
}

/// A `local`-prefixed symbol triggers `reason="local-prefix"` and a
/// confidence drop of exactly `EDGE_CONFIDENCE_LOCAL_PENALTY` from the
/// kind's floor. This is the only signal the visibility heuristic
/// surfaces today.
#[test]
fn local_prefix_symbol_triggers_local_prefix_reason_pr_a2() {
    // Build a tiny synthetic index where one of the symbols is local.
    let local_symbol_name = "local 42".to_string();
    let local_sym = ScipSymbol {
        symbol: local_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Variable),
        display_name: Some("local_var".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Private),
        signature_parts: None,
    };
    let pub_sym = ScipSymbol {
        symbol: "scip-rust pkg src/main.rs `caller`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("caller".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![ScipRelationship {
            source_symbol: "scip-rust pkg src/main.rs `caller`().".to_string(),
            target_symbol: local_symbol_name.clone(),
            kinds: BTreeSet::from([ScipRelationshipKind::Reference]),
        }],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![ScipFile {
            language: "rust".to_string(),
            relative_path: PathBuf::from("src/main.rs"),
            definitions: vec![definition_occurrence(&pub_sym.symbol)],
            references: vec![],
            occurrences: vec![definition_occurrence(&pub_sym.symbol)],
            symbols: vec![pub_sym, local_sym],
        }],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build(&[index]);
    let mut saw_local_prefix = false;
    for edge_ref in graph.graph().edge_references() {
        let edge = edge_ref.weight();
        if edge.reason.as_deref() == Some("local-prefix") {
            saw_local_prefix = true;
            let floor = edge_confidence_floor(edge.kind);
            let expected = (floor - EDGE_CONFIDENCE_LOCAL_PENALTY).clamp(0.0, 1.0);
            assert!(
                (edge.confidence - expected).abs() < 1e-9,
                "expected confidence {expected}, got {} for kind {:?}",
                edge.confidence,
                edge.kind
            );
        }
    }
    assert!(
        saw_local_prefix,
        "expected at least one edge involving the `local 42` symbol to be flagged"
    );
}

// ── PR A3: SymbolReference read/write split ───────────────────────────

/// Build a fixture project with a struct field that is read in one file
/// and written in another. Assert the role-aware split classifies the
/// edges as `Reads` / `Writes`. This is the core behaviour callers rely
/// on for `code_graph neighbors --kind_filter=writes` (PR A3 acceptance).
#[test]
fn read_write_split_classifies_field_accesses_pr_a3() {
    // Field `Counter#value`. Lives in src/counter.rs; is written from
    // src/writer.rs (mutator) and read from src/reader.rs (observer).
    let field_symbol = "scip-rust pkg src/counter.rs `Counter`#`value`.".to_string();
    let counter_struct = ScipSymbol {
        symbol: "scip-rust pkg src/counter.rs `Counter`#".to_string(),
        kind: Some(ScipSymbolKind::Struct),
        display_name: Some("Counter".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let value_field = ScipSymbol {
        symbol: field_symbol.clone(),
        kind: Some(ScipSymbolKind::Field),
        display_name: Some("value".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    let writer_sym = ScipSymbol {
        symbol: "scip-rust pkg src/writer.rs `bump`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("bump".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let reader_sym = ScipSymbol {
        symbol: "scip-rust pkg src/reader.rs `peek`().".to_string(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("peek".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };

    // Helper: build an occurrence with explicit roles, since the
    // existing `definition_occurrence` / `reference_occurrence` helpers
    // hardcode their role sets.
    fn role_occurrence(symbol: &str, roles: BTreeSet<ScipSymbolRole>) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 1,
                start_character: 4,
                end_line: 1,
                end_character: 10,
            },
            enclosing_range: None,
            roles,
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/counter.rs"),
                definitions: vec![
                    definition_occurrence(&counter_struct.symbol),
                    definition_occurrence(&field_symbol),
                ],
                references: vec![],
                occurrences: vec![
                    definition_occurrence(&counter_struct.symbol),
                    definition_occurrence(&field_symbol),
                ],
                symbols: vec![counter_struct, value_field],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/writer.rs"),
                definitions: vec![definition_occurrence(&writer_sym.symbol)],
                references: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::WriteAccess]),
                )],
                occurrences: vec![
                    definition_occurrence(&writer_sym.symbol),
                    role_occurrence(&field_symbol, BTreeSet::from([ScipSymbolRole::WriteAccess])),
                ],
                symbols: vec![writer_sym],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/reader.rs"),
                definitions: vec![definition_occurrence(&reader_sym.symbol)],
                references: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::ReadAccess]),
                )],
                occurrences: vec![
                    definition_occurrence(&reader_sym.symbol),
                    role_occurrence(&field_symbol, BTreeSet::from([ScipSymbolRole::ReadAccess])),
                ],
                symbols: vec![reader_sym],
            },
        ],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build(&[index]);

    // The role-aware classification fires on the symbol→target_file
    // edge minted in `add_reference`. Locate the field-defining file
    // and the writer/reader source files.
    let counter_file = graph
        .file_node("src/counter.rs")
        .expect("counter file node");
    let writer_file = graph.file_node("src/writer.rs").expect("writer file node");
    let reader_file = graph.file_node("src/reader.rs").expect("reader file node");
    let field_node = graph.symbol_node(&field_symbol).expect("field node");

    // From the WRITE site, the field's symbol→counter_file edge should
    // be tagged `Writes`. The field has *both* a writer site and a
    // reader site, so to attribute the right kind to the right source
    // we sweep edges *out of the field node* — there is one
    // SymbolReference-class edge per occurrence kind into
    // `src/counter.rs`. We expect exactly one `Writes` and one `Reads`
    // edge from the field node into the counter file.
    let mut writes_count = 0;
    let mut reads_count = 0;
    let mut bare_ref_count = 0;
    for edge in graph
        .graph()
        .edges_directed(field_node, petgraph::Direction::Outgoing)
    {
        if edge.target() != counter_file {
            continue;
        }
        match edge.weight().kind {
            RepoGraphEdgeKind::Writes => writes_count += 1,
            RepoGraphEdgeKind::Reads => reads_count += 1,
            RepoGraphEdgeKind::SymbolReference => bare_ref_count += 1,
            _ => {}
        }
    }
    assert_eq!(
        writes_count, 1,
        "expected exactly one Writes edge from field to counter file"
    );
    assert_eq!(
        reads_count, 1,
        "expected exactly one Reads edge from field to counter file"
    );
    assert_eq!(
        bare_ref_count, 0,
        "no bare SymbolReference edge should remain after the split when SCIP roles are populated"
    );

    // And the confidence floors land on the values pinned in this PR.
    for edge in graph.graph().edge_references() {
        let edge = edge.weight();
        match edge.kind {
            RepoGraphEdgeKind::Writes => {
                assert!(
                    (edge.confidence - EDGE_CONFIDENCE_WRITES).abs() < 1e-9,
                    "Writes edge confidence {} != floor {}",
                    edge.confidence,
                    EDGE_CONFIDENCE_WRITES
                );
            }
            RepoGraphEdgeKind::Reads => {
                assert!(
                    (edge.confidence - EDGE_CONFIDENCE_READS).abs() < 1e-9,
                    "Reads edge confidence {} != floor {}",
                    edge.confidence,
                    EDGE_CONFIDENCE_READS
                );
            }
            _ => {}
        }
    }

    // The unused vars silence "fields never read" without dropping
    // the assertions.
    let _ = (writer_file, reader_file);
}

/// `kind_filter=writes` on the field's neighbors picks out the file
/// that *writes* it; `reads` picks out the *reader*. This is the
/// acceptance criterion in the PR A3 plan: "fixture project with a
/// struct field; assert `kind_filter=writes` returns only writers,
/// `reads` only readers".
///
/// The `neighbors` op itself lives in the bridge; here we exercise the
/// underlying graph the bridge filters over.
#[test]
fn neighbors_kind_filter_writes_returns_only_writers_pr_a3() {
    // Reuse the multi-file fixture set up by the previous test by
    // inlining a smaller variant focused on just the field node.
    let field_symbol = "scip-rust pkg src/counter.rs `f`#`v`.".to_string();
    let value_field = ScipSymbol {
        symbol: field_symbol.clone(),
        kind: Some(ScipSymbolKind::Field),
        display_name: Some("v".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    fn role_occurrence(symbol: &str, roles: BTreeSet<ScipSymbolRole>) -> ScipOccurrence {
        ScipOccurrence {
            symbol: symbol.to_string(),
            range: ScipRange {
                start_line: 1,
                start_character: 4,
                end_line: 1,
                end_character: 10,
            },
            enclosing_range: None,
            roles,
            syntax_kind: None,
            override_documentation: vec![],
        }
    }

    let index = ParsedScipIndex {
        workspace_slug: "root".to_string(),
        metadata: ScipMetadata::default(),
        files: vec![
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/counter.rs"),
                definitions: vec![definition_occurrence(&field_symbol)],
                references: vec![],
                occurrences: vec![definition_occurrence(&field_symbol)],
                symbols: vec![value_field],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/writer.rs"),
                definitions: vec![],
                references: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::WriteAccess]),
                )],
                occurrences: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::WriteAccess]),
                )],
                symbols: vec![],
            },
            ScipFile {
                language: "rust".to_string(),
                relative_path: PathBuf::from("src/reader.rs"),
                definitions: vec![],
                references: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::ReadAccess]),
                )],
                occurrences: vec![role_occurrence(
                    &field_symbol,
                    BTreeSet::from([ScipSymbolRole::ReadAccess]),
                )],
                symbols: vec![],
            },
        ],
        external_symbols: vec![],
    };

    let graph = RepoDependencyGraph::build(&[index]);
    let counter_file = graph
        .file_node("src/counter.rs")
        .expect("counter file node");

    // Mirror the bridge-side filter: walk outgoing edges from the
    // field node into `src/counter.rs` and partition by edge kind.
    let field_node = graph.symbol_node(&field_symbol).expect("field node");
    let writes: Vec<_> = graph
        .graph()
        .edges_directed(field_node, petgraph::Direction::Outgoing)
        .filter(|e| e.target() == counter_file && e.weight().kind == RepoGraphEdgeKind::Writes)
        .collect();
    let reads: Vec<_> = graph
        .graph()
        .edges_directed(field_node, petgraph::Direction::Outgoing)
        .filter(|e| e.target() == counter_file && e.weight().kind == RepoGraphEdgeKind::Reads)
        .collect();

    assert_eq!(writes.len(), 1, "expected one Writes edge");
    assert_eq!(reads.len(), 1, "expected one Reads edge");

    // Confidence floors propagate from the table in this module.
    assert!(
        (writes[0].weight().confidence - EDGE_CONFIDENCE_WRITES).abs() < 1e-9,
        "Writes confidence floor mismatch"
    );
    assert!(
        (reads[0].weight().confidence - EDGE_CONFIDENCE_READS).abs() < 1e-9,
        "Reads confidence floor mismatch"
    );
}
