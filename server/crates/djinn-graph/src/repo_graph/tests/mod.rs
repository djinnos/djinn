// Test module for the `repo_graph` submodule.
//
// Split out of the oversized `repo_graph/tests.rs` module into focused
// sibling test modules under `repo_graph/tests/`. The parent
// `repo_graph/mod.rs` still declares this as `#[cfg(test)] mod tests;`, so
// the test path remains rooted at `repo_graph::tests` while Rust resolves this
// directory-form module via `tests/mod.rs`.
//
// No logic changes — test bodies remain byte-for-byte identical to the
// pre-split tree; this shim only declares the focused modules and keeps shared
// fixture helpers available to those siblings.

mod artifact;
mod build;
mod graph_queries;
mod ranking;
mod salvage;
mod scip_file_iter;
mod stable_uid;
mod symbols_complexity;
mod synthetic_dedup;
mod trait_dispatch;

use std::path::{Path, PathBuf};

use petgraph::visit::EdgeRef;
use serde::Serialize;

use super::*;
use crate::complexity::ComplexityMetrics;
use crate::scip_parser::{
    ParsedScipIndex, ScipFile, ScipMetadata, ScipOccurrence, ScipRange, ScipRelationship,
    ScipRelationshipKind, ScipSymbol, ScipSymbolKind, ScipSymbolRole,
};

pub(super) fn fixture_index() -> ParsedScipIndex {
    let helper_symbol_name = "scip-rust pkg src/helper.rs `helper`().".to_string();
    let helper_symbol = ScipSymbol {
        symbol: helper_symbol_name.clone(),
        kind: Some(ScipSymbolKind::Function),
        display_name: Some("helper".to_string()),
        signature: Some("fn helper()".to_string()),
        documentation: vec!["returns a value".to_string()],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
    };
    let trait_symbol = ScipSymbol {
        symbol: "scip-rust pkg src/types.rs `HelperTrait`#".to_string(),
        kind: Some(ScipSymbolKind::Type),
        display_name: Some("HelperTrait".to_string()),
        signature: None,
        documentation: vec![],
        relationships: vec![],
        visibility: Some(crate::scip_parser::ScipVisibility::Public),
        signature_parts: None,
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

pub(super) fn definition_occurrence(symbol: &str) -> ScipOccurrence {
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

pub(super) fn reference_occurrence(symbol: &str) -> ScipOccurrence {
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
