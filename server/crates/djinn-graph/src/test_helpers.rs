//! In-crate test helpers — mirrors the sliver of
//! `djinn-server::test_helpers` the canonical-graph / repo-map tests need
//! without dragging the server crate in.
//!
//! Exposed as a top-level module under `#[cfg(test)]` so all siblings can
//! consume `crate::test_helpers::*`.

#![cfg(test)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_db::Database;

use crate::WarmContext;
use crate::communities::Community;
use crate::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
    RepoGraphArtifactSymbolRange, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    edge_confidence_floor, edge_weight_for,
};

/// Open a fresh test database (isolated Dolt branch via
/// `Database::open_in_memory`).
pub(crate) fn create_test_db() -> Database {
    Database::open_in_memory().expect("failed to create test database")
}

/// Create a per-test tempdir rooted under `target/test-tmp` so the tests
/// play nice with our standard clean-up script.
pub(crate) fn workspace_tempdir(prefix: &str) -> tempfile::TempDir {
    let base = std::env::current_dir()
        .expect("current dir")
        .join("target")
        .join("test-tmp");
    std::fs::create_dir_all(&base).expect("create djinn-graph test tempdir base");
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(base)
        .expect("create djinn-graph test tempdir")
}

/// Minimal [`WarmContext`] backed by an in-memory DB + no-op event bus +
/// per-test indexer mutex.  Suitable for unit tests that don't go through
/// the full `AppState` constructor.
pub(crate) struct TestWarmContext {
    db: Database,
    indexer_lock: Arc<tokio::sync::Mutex<()>>,
}

impl TestWarmContext {
    pub(crate) fn new(db: Database) -> Self {
        Self {
            db,
            indexer_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl WarmContext for TestWarmContext {
    fn db(&self) -> &Database {
        &self.db
    }

    fn event_bus(&self) -> EventBus {
        EventBus::noop()
    }

    fn indexer_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.indexer_lock.clone()
    }
}

/// Build the deterministic td55 graph-equivalence fixture and return the
/// serialized repo-graph artifact blob used by
/// [`crate::graph_parity::assert_graph_artifact_blob_parity`].
///
/// The fixture is intentionally in-process only: it constructs a small
/// [`RepoDependencyGraph`] from local artifact structs, derives the deterministic
/// layout sidecar, and serializes with `bincode`. It does **not** invoke Docker,
/// Kubernetes, network services, or real SCIP indexer binaries.
///
/// Shape for cache-reuse parity tests:
/// - two file nodes in distinct synthetic partitions/workspaces;
/// - symbol definitions in both files;
/// - a cross-file `SymbolReference` edge from `src/app.rs::run` to
///   `src/domain/math.rs::double`, plus a `FileReference` edge between the
///   files. A changed-file-only graph assembly that forgets unchanged-file
///   references will therefore fail artifact parity.
pub(crate) fn td55_equivalence_fixture_artifact_blob() -> Vec<u8> {
    let mut graph = td55_equivalence_fixture_graph();
    graph.set_layout_positions(crate::layout::derive_layout_positions(&graph));
    bincode::serialize(&graph.to_artifact()).expect("serialize td55 graph fixture artifact")
}

/// Build the in-memory graph backing [`td55_equivalence_fixture_artifact_blob`].
/// Follow-up td55 tests can call this when they need graph-level inspection
/// before serializing through the normal repo-graph artifact path.
pub(crate) fn td55_equivalence_fixture_graph() -> RepoDependencyGraph {
    RepoDependencyGraph::from_artifact(&td55_equivalence_fixture_artifact())
}

fn td55_equivalence_fixture_artifact() -> RepoGraphArtifact {
    let app_file = "src/app.rs";
    let math_file = "src/domain/math.rs";
    let run_symbol = "scip-rust td55 src/app.rs `run`().";
    let configure_symbol = "scip-rust td55 src/app.rs `configure`().";
    let double_symbol = "scip-rust td55 src/domain/math.rs `double`().";

    RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![
            file_node(app_file, "partition-app"),
            file_node(math_file, "partition-domain"),
            symbol_node(run_symbol, "run", app_file, "partition-app"),
            symbol_node(configure_symbol, "configure", app_file, "partition-app"),
            symbol_node(double_symbol, "double", math_file, "partition-domain"),
        ],
        edges: vec![
            edge(0, 2, RepoGraphEdgeKind::ContainsDefinition),
            edge(2, 0, RepoGraphEdgeKind::DeclaredInFile),
            edge(0, 3, RepoGraphEdgeKind::ContainsDefinition),
            edge(3, 0, RepoGraphEdgeKind::DeclaredInFile),
            edge(1, 4, RepoGraphEdgeKind::ContainsDefinition),
            edge(4, 1, RepoGraphEdgeKind::DeclaredInFile),
            // Cross-file/partition reference: app::run calls domain::double.
            edge(2, 4, RepoGraphEdgeKind::SymbolReference),
            edge(0, 1, RepoGraphEdgeKind::FileReference),
        ],
        symbol_ranges: BTreeMap::from([
            (
                PathBuf::from(app_file),
                vec![
                    RepoGraphArtifactSymbolRange {
                        start_line: 3,
                        end_line: 7,
                        node: 2,
                    },
                    RepoGraphArtifactSymbolRange {
                        start_line: 9,
                        end_line: 11,
                        node: 3,
                    },
                ],
            ),
            (
                PathBuf::from(math_file),
                vec![RepoGraphArtifactSymbolRange {
                    start_line: 1,
                    end_line: 3,
                    node: 4,
                }],
            ),
        ]),
        communities: vec![
            community("td55-app-partition", vec![0, 2, 3]),
            community("td55-domain-partition", vec![1, 4]),
        ],
        processes: Vec::new(),
        route_exclusion_config: Default::default(),
        layout_positions: BTreeMap::new(),
    }
}

fn file_node(path: &str, workspace: &str) -> RepoGraphNode {
    RepoGraphNode {
        id: RepoNodeKey::File(path.into()),
        kind: RepoGraphNodeKind::File,
        display_name: path.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(path.into()),
        symbol: None,
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: None,
        documentation: Vec::new(),
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some(workspace.to_string()),
        route_framework: None,
        route_handler_symbol: None,
    }
}

fn symbol_node(
    symbol: &str,
    display_name: &str,
    file_path: &str,
    workspace: &str,
) -> RepoGraphNode {
    RepoGraphNode {
        id: RepoNodeKey::Symbol(symbol.to_string()),
        kind: RepoGraphNodeKind::Symbol,
        display_name: display_name.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(file_path.into()),
        symbol: Some(symbol.to_string()),
        symbol_kind: None,
        is_external: false,
        visibility: None,
        signature: Some(format!("fn {display_name}()")),
        documentation: Vec::new(),
        signature_parts: None,
        is_test: false,
        complexity: None,
        workspace: Some(workspace.to_string()),
        route_framework: None,
        route_handler_symbol: None,
    }
}

fn edge(source: usize, target: usize, kind: RepoGraphEdgeKind) -> RepoGraphArtifactEdge {
    RepoGraphArtifactEdge {
        source,
        target,
        kind,
        weight: edge_weight_for(kind),
        evidence_count: 1,
        confidence: edge_confidence_floor(kind),
        reason: None,
        step: None,
    }
}

fn community(id: &str, member_ids: Vec<usize>) -> Community {
    Community {
        id: id.to_string(),
        label: id.to_string(),
        symbol_count: member_ids.len(),
        member_ids,
        cohesion: 1.0,
        keywords: Vec::new(),
    }
}
