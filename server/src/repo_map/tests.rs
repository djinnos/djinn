use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::repo_graph::{RepoDependencyGraph, RepoGraphNodeKind};
use crate::repo_map_personalization::{
    RepoMapNoteHint, RepoMapPersonalizationInput, extract_identifier_candidates,
};
use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
    ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
};

use super::{
    RepoMapPersonalizationRequest, RepoMapRenderError, RepoMapRenderOptions,
    personalized_repo_map_ranking, render_repo_map, repo_map_note_spec,
};

#[test]
fn render_repo_map_is_deterministic_and_budget_aware() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();
    let options = RepoMapRenderOptions::new(120);

    let first = render_repo_map(&graph, &ranking, &options).expect("render succeeds");
    let second = render_repo_map(&graph, &ranking, &options).expect("render succeeds");

    assert_eq!(first, second);
    assert!(first.token_estimate <= options.token_budget);
    assert!(first.content.contains("# Repository Map"));
    assert!(first.content.contains("src/helper.rs") || first.content.contains("src/app.rs"));
}

#[test]
fn render_repo_map_shrinks_with_budget_using_bounded_search() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();

    let roomy = render_repo_map(&graph, &ranking, &RepoMapRenderOptions::new(300))
        .expect("roomy render succeeds");
    let tight = render_repo_map(
        &graph,
        &ranking,
        &RepoMapRenderOptions {
            token_budget: 90,
            max_files: 1,
            max_symbols_per_file: 1,
            max_relationships_per_file: 1,
        },
    )
    .expect("tight render succeeds");

    assert!(roomy.included_entries > tight.included_entries);
    assert!(tight.token_estimate <= 90);
}

#[test]
fn render_repo_map_reports_when_minimal_representation_cannot_fit() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();

    let err = render_repo_map(&graph, &ranking, &RepoMapRenderOptions::new(10))
        .expect_err("budget should be too small");

    assert!(matches!(
        err,
        RepoMapRenderError::MinimalRepresentationExceedsBudget { .. }
    ));
}

#[test]
fn repo_map_note_spec_uses_repo_map_folder_and_stable_permalink() {
    let spec = repo_map_note_spec("abcdef1234567890");
    assert_eq!(spec.title, "Repository Map abcdef123456");
    assert_eq!(spec.permalink, "reference/repo-maps/abcdef123456");
    assert!(spec.tags_json.contains("repo-map"));
    assert!(spec.tags_json.contains("abcdef123456"));
}

#[test]
fn personalized_identifier_extraction_filters_low_signal_tokens() {
    let memory_refs = vec![
        "decisions/adr-043-repository-map-scip-powered-structural-context-for-agent-sessions"
            .to_string(),
        "notes/RepoMapQueryHelper".to_string(),
    ];
    let identifiers = extract_identifier_candidates(&RepoMapPersonalizationInput {
        title: Some("Phase 2: Extract task-aware identifiers for RepoMapQueryHelper"),
        description: Some(
            "Parse session title/description/design text and prefer repo_map.rs plus TaskSession42.",
        ),
        design: Some(
            "Bias selection toward repo_map_personalization.rs and relationship display text like symbol-ref repo_map.rs",
        ),
        memory_refs: &memory_refs,
    });

    assert!(identifiers.contains(&"repomapqueryhelper".to_string()));
    assert!(identifiers.contains(&"tasksession42".to_string()));
    assert!(identifiers.contains(&"repository".to_string()));
    assert!(identifiers.contains(&"scip".to_string()));
    assert!(!identifiers.contains(&"task".to_string()));
    assert!(!identifiers.contains(&"map".to_string()));
    assert!(!identifiers.contains(&"043".to_string()));
}

#[test]
fn personalized_repo_map_ranking_boosts_matching_file_symbol_and_relationship_entries() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();
    let memory_refs = vec!["docs/other-note".to_string()];
    let note_hints = vec![RepoMapNoteHint {
        permalink: "docs/helpertrait".to_string(),
        title: "HelperTrait notes".to_string(),
        snippet: "src/helper.rs helper HelperTrait symbol-ref src/helper.rs".to_string(),
        normalized_tokens: vec![
            "helpertrait".to_string(),
            "helper".to_string(),
            "src".to_string(),
        ],
    }];

    let personalized = personalized_repo_map_ranking(
        &graph,
        &RepoMapPersonalizationRequest {
            ranked_nodes: &ranking.nodes,
            title: Some("Investigate implementation details"),
            description: Some("Need note-linked helper concepts"),
            design: Some("Prefer note-linked relationship display text"),
            memory_refs: &memory_refs,
            note_hints: &note_hints,
        },
    );

    let personalized_files = personalized
        .iter()
        .filter(|node| node.kind == RepoGraphNodeKind::File)
        .map(|node| graph.node(node.node_index).display_name.clone())
        .collect::<Vec<_>>();

    assert_eq!(personalized_files[0], "src/app.rs");
    assert_eq!(personalized_files[1], "src/helper.rs");
}

#[test]
fn personalized_repo_map_ranking_preserves_baseline_order_without_note_hints() {
    let graph = RepoDependencyGraph::build(&[fixture_index()]);
    let ranking = graph.rank();

    let personalized = personalized_repo_map_ranking(
        &graph,
        &RepoMapPersonalizationRequest {
            ranked_nodes: &ranking.nodes,
            title: None,
            description: None,
            design: None,
            memory_refs: &[],
            note_hints: &[],
        },
    );

    assert_eq!(personalized, ranking.nodes);
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
        visibility: None,
    };
    let trait_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("HelperTrait".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: None,
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
        visibility: None,
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
