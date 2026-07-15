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
use protobuf::{EnumOrUnknown, Message};
use scip::types::{Document, Index, Occurrence, SymbolInformation, symbol_information};

use crate::WarmContext;
use crate::communities::Community;
use crate::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
    RepoGraphArtifactSymbolRange, RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey,
    edge_confidence_floor, edge_weight_for,
};

/// Process-global lock serializing every test that mutates process-wide
/// environment variables consumed by the SCIP indexer / canonical-graph
/// pipeline — `PATH`, `DJINN_TEST_SCIP_FIXTURE`, `DJINN_GRAPH_OUT_OF_CORE*`,
/// the `*CACHE_REUSE_ENABLED` toggles, and `DJINN_SCIP_CACHE_DIR`.
///
/// `std::env::{set_var,remove_var}` mutate shared process state, and the
/// indexer spawns subprocesses that inherit `PATH`. Under Cargo's parallel
/// test threads, two such tests can clobber each other's env mid-flight:
/// one test's `PATH` restore drops the fake-`rust-analyzer` bin dir while a
/// sibling's indexer is still resolving it, so the spawn falls back to the
/// real (or missing) binary and the pipeline reports "no index produced".
/// Every test that touches any of these vars MUST hold this single lock for
/// its whole body so the mutations are fully serialized (a per-var or
/// per-module lock is NOT enough — the vars are shared across modules).
pub(crate) static PIPELINE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire [`PIPELINE_ENV_LOCK`], transparently recovering from a poisoned
/// mutex so one panicking env-mutating test does not cascade into spurious
/// failures across every other test that shares the lock.
pub(crate) fn lock_pipeline_env() -> std::sync::MutexGuard<'static, ()> {
    PIPELINE_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
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

// ---------------------------------------------------------------------------
// td55 cold-vs-cache-reuse fixture: on-disk SCIP artifacts
// ---------------------------------------------------------------------------

/// Handle to the on-disk synthetic SCIP artifact set used by the td55
/// cold-vs-cache-reuse parity test.
///
/// The `tempdir` keeps the written `.scip` files alive for the duration of
/// the parse/build phases. Dropping it cleans up automatically.
pub(crate) struct Td55ScipFixture {
    pub artifacts: Vec<crate::scip_indexer::ScipArtifact>,
    _tempdir: tempfile::TempDir,
}

/// Build the td55 cold-vs-cache-reuse fixture as a set of on-disk SCIP
/// protobuf artifact files.
///
/// The fixture mirrors [`td55_equivalence_fixture_artifact_blob`] but goes
/// through the **real parse/build path**: two synthetic SCIP indexes are
/// written to temp `.scip` files (one per workspace/partition) with a
/// cross-file `SymbolReference` edge from `app::run` to `domain::double`.
///
/// Each [`ScipArtifact`] points at the temp file so
/// [`crate::scip_parser::parse_scip_artifacts_with_cache_store`] (and the
/// cache-reuse variant) read and parse real protobuf bytes — no real
/// indexer binaries, Docker, or network are involved.
///
/// The returned `Vec` always contains the **complete** fixture artifact
/// set. Callers must pass the whole set to both the cold and cache-reuse
/// phases so the graph is built from every artifact, not a changed subset.
pub(crate) fn td55_cache_reuse_scip_fixture() -> Td55ScipFixture {
    let tempdir = workspace_tempdir("td55-cache-reuse-scip-");

    // Write one SCIP index per workspace. The domain artifact defines
    // `double` and the app artifact defines `run` and references `double`,
    // establishing a cross-file/partition symbol reference edge.  Both
    // artifacts together form the complete fixture set.
    //
    // The domain artifact is placed first in the artifact list so the graph
    // builder registers `double`'s file before the app's reference resolves
    // the cross-file edge.
    let domain_bytes = synthetic_scip_index_bytes(
        "src/domain/math.rs",
        &[("double", "scip-rust . . . domain double().")],
        &[],
    );
    let app_bytes = synthetic_scip_index_bytes(
        "src/app.rs",
        &[("run", "scip-rust . . . app run().")],
        &[("scip-rust . . . domain double().", 8)],
    );

    let domain_path = tempdir.path().join("domain.scip");
    let app_path = tempdir.path().join("app.scip");
    std::fs::write(&domain_path, &domain_bytes).expect("write domain SCIP fixture");
    std::fs::write(&app_path, &app_bytes).expect("write app SCIP fixture");

    let artifacts = vec![
        crate::scip_indexer::ScipArtifact {
            path: domain_path,
            indexer: Some(crate::scip_indexer::SupportedIndexer::RustAnalyzer),
            workspace_slug: "domain".to_string(),
            workspace_root: PathBuf::new(),
        },
        crate::scip_indexer::ScipArtifact {
            path: app_path,
            indexer: Some(crate::scip_indexer::SupportedIndexer::RustAnalyzer),
            workspace_slug: "app".to_string(),
            workspace_root: PathBuf::new(),
        },
    ];

    Td55ScipFixture {
        artifacts,
        _tempdir: tempdir,
    }
}

/// Encode a minimal synthetic SCIP `Index` protobuf for a single document.
///
/// `definitions` are `(display_name, symbol)` tuples that become symbol
/// definitions (role bit = Definition).  `references` are `(symbol,
/// symbol_roles)` occurrences without the definition bit so they are
/// treated as references/calls — used to establish cross-file edges.
///
/// Build a td55 incremental-shaped fixture where one partition's SCIP
/// content is deterministically changed, producing a different cache key
/// for that partition while the other partition remains byte-identical.
///
/// The `changed_workspace` argument selects which partition to mutate:
/// - `"app"` — the app artifact gains an extra symbol `helper` and an extra
///   reference occurrence, changing the SCIP bytes.
/// - `"domain"` — the domain artifact gains an extra symbol `triple`.
///
/// The unchanged partition stays exactly the same as
/// [`td55_cache_reuse_scip_fixture`] so the SCIP parse cache hits for it.
pub(crate) fn td55_incremental_scip_fixture(changed_workspace: &str) -> Td55ScipFixture {
    let tempdir = workspace_tempdir("td55-incremental-scip-");

    let (domain_bytes, app_bytes) = if changed_workspace == "domain" {
        let domain = synthetic_scip_index_bytes(
            "src/domain/math.rs",
            &[
                ("double", "scip-rust . . . domain double()."),
                ("triple", "scip-rust . . . domain triple()."),
            ],
            &[],
        );
        let app = synthetic_scip_index_bytes(
            "src/app.rs",
            &[("run", "scip-rust . . . app run().")],
            &[("scip-rust . . . domain double().", 8)],
        );
        (domain, app)
    } else {
        // default / "app" changed
        let domain = synthetic_scip_index_bytes(
            "src/domain/math.rs",
            &[("double", "scip-rust . . . domain double().")],
            &[],
        );
        let app = synthetic_scip_index_bytes(
            "src/app.rs",
            &[
                ("run", "scip-rust . . . app run()."),
                ("helper", "scip-rust . . . app helper()."),
            ],
            &[
                ("scip-rust . . . domain double().", 8),
                ("scip-rust . . . app helper().", 8),
            ],
        );
        (domain, app)
    };

    let domain_path = tempdir.path().join("domain.scip");
    let app_path = tempdir.path().join("app.scip");
    std::fs::write(&domain_path, &domain_bytes).expect("write domain SCIP fixture");
    std::fs::write(&app_path, &app_bytes).expect("write app SCIP fixture");

    let artifacts = vec![
        crate::scip_indexer::ScipArtifact {
            path: domain_path,
            indexer: Some(crate::scip_indexer::SupportedIndexer::RustAnalyzer),
            workspace_slug: "domain".to_string(),
            workspace_root: PathBuf::new(),
        },
        crate::scip_indexer::ScipArtifact {
            path: app_path,
            indexer: Some(crate::scip_indexer::SupportedIndexer::RustAnalyzer),
            workspace_slug: "app".to_string(),
            workspace_root: PathBuf::new(),
        },
    ];

    Td55ScipFixture {
        artifacts,
        _tempdir: tempdir,
    }
}

fn synthetic_scip_index_bytes(
    relative_path: &str,
    definitions: &[(&str, &str)],
    references: &[(&str, i32)],
) -> Vec<u8> {
    let mut doc = Document::new();
    doc.language = "rust".to_string();
    doc.relative_path = relative_path.to_string();

    let mut occurrences = Vec::new();
    let mut symbols = Vec::new();
    for &(display_name, symbol) in definitions {
        occurrences.push(Occurrence {
            range: vec![0, 0, 5],
            symbol: symbol.to_string(),
            symbol_roles: 1, // Definition role bit
            ..Occurrence::new()
        });
        symbols.push(SymbolInformation {
            symbol: symbol.to_string(),
            display_name: display_name.to_string(),
            kind: EnumOrUnknown::new(symbol_information::Kind::Function),
            ..SymbolInformation::new()
        });
    }
    for &(symbol, roles) in references {
        occurrences.push(Occurrence {
            range: vec![1, 0, 5],
            symbol: symbol.to_string(),
            symbol_roles: roles,
            ..Occurrence::new()
        });
    }
    doc.occurrences = occurrences;
    doc.symbols = symbols;

    let mut index = Index::new();
    index.documents = vec![doc];
    index.write_to_bytes().expect("encode synthetic SCIP index")
}
