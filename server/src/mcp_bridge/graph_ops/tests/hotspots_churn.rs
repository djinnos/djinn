use super::*;
use crate::mcp_bridge::graph_ops::RepoGraphBridge;
use djinn_control_plane::bridge::ProjectCtx;
use djinn_control_plane::test_support::McpTestHarness;
use djinn_db::CommitFileChangeRepository;
use djinn_graph::canonical_graph::{
    CachedGraph, GRAPH_CACHE, derive_graph_caches, normalize_graph_query_paths,
};
use djinn_graph::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphArtifactEdge,
    RepoGraphEdgeKind, RepoGraphNode, RepoGraphNodeKind, RepoNodeKey, RouteExclusionConfig,
};
use djinn_graph::scip_parser::ScipVisibility;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

/// Serialize tests that mutate the process-global canonical graph cache
/// and seeded DB state. Hotspots loads the graph via
/// `load_canonical_graph`, so every test that installs a warmed fixture
/// must hold this lock to avoid races with parallel graph-op tests.
static HOTSPOTS_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

const HOTSPOTS_TEST_PROJECT_ID: &str = "hotspots-test-project";
const HOTSPOTS_TEST_CLONE_PATH: &str = "/workspace/hotspots-test-repo";

/// Build a minimal graph with three files: `src/hot.rs`, `src/central.rs`,
/// and `src/cold.rs`. `central.rs` is the highest-centrality file because
/// it owns a symbol referenced by both other files. `hot.rs` and
/// `cold.rs` own symbols with only inbound references.
fn hotspots_test_graph() -> RepoDependencyGraph {
    let mk_symbol =
        |symbol: &str, display_name: &str, file_path: &str| RepoGraphNode {
            id: RepoNodeKey::Symbol(symbol.to_string()),
            kind: RepoGraphNodeKind::Symbol,
            display_name: display_name.to_string(),
            language: Some("rust".to_string()),
            file_path: Some(PathBuf::from(file_path)),
            symbol: Some(symbol.to_string()),
            symbol_kind: Some(djinn_graph::scip_parser::ScipSymbolKind::Function),
            is_external: false,
            visibility: Some(ScipVisibility::Public),
            signature: None,
            documentation: vec![],
            signature_parts: None,
            is_test: false,
            complexity: None,
            workspace: None,
            route_framework: None,
            route_handler_symbol: None,
        };
    let mk_file = |file_path: &str| RepoGraphNode {
        id: RepoNodeKey::File(PathBuf::from(file_path)),
        kind: RepoGraphNodeKind::File,
        display_name: file_path.to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from(file_path)),
        symbol: None,
        symbol_kind: None,
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

    // Node indices:
    // 0: file src/hot.rs
    // 1: symbol hot_fn (in src/hot.rs)
    // 2: file src/central.rs
    // 3: symbol central_fn (in src/central.rs)
    // 4: file src/cold.rs
    // 5: symbol cold_fn (in src/cold.rs)
    let nodes = vec![
        mk_file("src/hot.rs"),
        mk_symbol(
            "scip-rust pkg src/hot.rs `hot_fn`().",
            "hot_fn",
            "src/hot.rs",
        ),
        mk_file("src/central.rs"),
        mk_symbol(
            "scip-rust pkg src/central.rs `central_fn`().",
            "central_fn",
            "src/central.rs",
        ),
        mk_file("src/cold.rs"),
        mk_symbol(
            "scip-rust pkg src/cold.rs `cold_fn`().",
            "cold_fn",
            "src/cold.rs",
        ),
    ];
    let edges = vec![
        // hot_fn and cold_fn reference central_fn, giving central.rs
        // the highest per-file centrality.
        RepoGraphArtifactEdge {
            source: 1,
            target: 3,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: 1.0,
            evidence_count: 1,
        },
        RepoGraphArtifactEdge {
            source: 5,
            target: 3,
            kind: RepoGraphEdgeKind::SymbolReference,
            weight: 1.0,
            evidence_count: 1,
        },
    ];
    RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes,
        edges,
        symbol_ranges: BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: RouteExclusionConfig::default(),
        layout_positions: BTreeMap::new(),
    })
}

async fn install_hotspots_cached_graph(graph: RepoDependencyGraph) {
    let (_project_root, index_tree_path) =
        normalize_graph_query_paths(HOTSPOTS_TEST_CLONE_PATH);
    let (pagerank, sccs, layout_positions, _) =
        derive_graph_caches(&graph, Path::new("/var/tmp/djinn-hotspots-test"));
    let mut cache = GRAPH_CACHE.write().await;
    *cache = Some(CachedGraph {
        graph,
        project_path: index_tree_path,
        git_head: "test-head".to_string(),
        pagerank,
        sccs,
        layout_positions,
        crate_map: Arc::new(BTreeMap::new()),
    });
}

fn hotspots_ctx() -> ProjectCtx {
    ProjectCtx {
        id: HOTSPOTS_TEST_PROJECT_ID.to_string(),
        clone_path: HOTSPOTS_TEST_CLONE_PATH.to_string(),
        workspace: None,
        sub_path: None,
    }
}

/// Return a timestamp `days` before now in ISO-8601 UTC format. The
/// `commit_file_changes` repository uses lexical string comparison on
/// `committed_at`, so the fractional-second precision matters.
fn days_ago(days: i64) -> String {
    let now = time::OffsetDateTime::now_utc();
    (now - time::Duration::days(days))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

fn commit_row(file_path: &str, sha: &str, committed_at: &str) -> djinn_db::CommitFileChange {
    djinn_db::CommitFileChange {
        project_id: HOTSPOTS_TEST_PROJECT_ID.to_string(),
        commit_sha: sha.to_string(),
        file_path: file_path.to_string(),
        change_kind: "M".to_string(),
        committed_at: committed_at.to_string(),
        author_email: "test@example.com".to_string(),
        insertions: 1,
        deletions: 0,
        old_path: None,
    }
}

async fn seed_hotspots_commits(db: &djinn_db::Database) {
    let repo = CommitFileChangeRepository::new(db.clone());
    // Three distinct in-window commits for src/hot.rs.
    let hot_at = days_ago(1);
    let hot_at_2 = days_ago(5);
    let hot_at_3 = days_ago(10);
    // One in-window commit for src/central.rs.
    let central_at = days_ago(3);
    // One out-of-window commit for src/cold.rs (beyond the 30-day window).
    let cold_at = days_ago(45);

    let rows = vec![
        commit_row("src/hot.rs", "hotsha1", &hot_at),
        commit_row("src/hot.rs", "hotsha2", &hot_at_2),
        commit_row("src/hot.rs", "hotsha3", &hot_at_3),
        commit_row("src/central.rs", "centralsha1", &central_at),
        commit_row("src/cold.rs", "coldsha1", &cold_at),
    ];
    repo.upsert_batch(&rows)
        .await
        .expect("seed commit_file_changes rows");
}

async fn setup_hotspots_fixtures(db: &djinn_db::Database) {
    install_hotspots_cached_graph(hotspots_test_graph()).await;
    seed_hotspots_commits(db).await;
}

#[tokio::test(flavor = "current_thread")]
async fn hotspots_returns_db_churn_counts_and_excludes_out_of_window() {
    let _guard = HOTSPOTS_TEST_LOCK.lock().await;
    let harness = McpTestHarness::new().await;
    setup_hotspots_fixtures(harness.db()).await;

    let bridge = RepoGraphBridge::new(harness.state().clone());
    let result = bridge
        .hotspots(&hotspots_ctx(), 30, None, 10)
        .await
        .expect("hotspots should succeed with seeded DB churn");

    // src/cold.rs has only out-of-window churn and must be excluded.
    assert!(
        result.iter().all(|e| e.file != "src/cold.rs"),
        "src/cold.rs should be excluded for the 30-day window: {result:?}"
    );

    // Exactly the two churned files should be returned (churn-led semantics).
    let files: Vec<String> = result.iter().map(|e| e.file.clone()).collect();
    assert_eq!(files, vec!["src/central.rs", "src/hot.rs"]);

    let hot = result.iter().find(|e| e.file == "src/hot.rs").unwrap();
    let central = result.iter().find(|e| e.file == "src/central.rs").unwrap();

    assert_eq!(hot.churn, 3, "src/hot.rs should report 3 distinct commits");
    assert_eq!(
        central.churn, 1,
        "src/central.rs should report 1 distinct commit"
    );

    // Sort order: composite_score descending, then file-path tie-break.
    // central.rs has higher centrality (referenced by hot and cold),
    // so centrality * 1 > centrality * 3 for hot.rs is expected because
    // the central file's summed PageRank dominates the churn multiplier.
    assert!(
        result[0].composite_score >= result[1].composite_score,
        "result should be sorted by composite_score descending: {result:?}"
    );
    if (result[0].composite_score - result[1].composite_score).abs() < f64::EPSILON {
        assert!(
            result[0].file <= result[1].file,
            "file-path tie-break should be ascending: {result:?}"
        );
    }
    assert_eq!(result[0].file, "src/central.rs");
    assert_eq!(result[1].file, "src/hot.rs");

    // Churn-led semantics: no centrality-only entries.
    assert_eq!(result.len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn hotspots_file_glob_excludes_all_churned_files_returns_empty() {
    let _guard = HOTSPOTS_TEST_LOCK.lock().await;
    let harness = McpTestHarness::new().await;
    setup_hotspots_fixtures(harness.db()).await;

    let bridge = RepoGraphBridge::new(harness.state().clone());
    let result = bridge
        .hotspots(&hotspots_ctx(), 30, Some("src/nowhere/**"), 10)
        .await
        .expect("hotspots should return Ok when glob filters every churned file");

    assert!(result.is_empty(), "glob excluding all churned files should yield []: {result:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn hotspots_churn_led_no_db_row_returns_empty() {
    let _guard = HOTSPOTS_TEST_LOCK.lock().await;
    let harness = McpTestHarness::new().await;
    install_hotspots_cached_graph(hotspots_test_graph()).await;

    // Seed churn only for src/hot.rs; src/cold.rs exists in the graph
    // but has no DB row and must not be returned as a centrality-only
    // hotspot.
    let repo = CommitFileChangeRepository::new(harness.db().clone());
    let at = days_ago(1);
    repo.upsert_batch(&[commit_row("src/hot.rs", "hotsha1", &at)])
        .await
        .expect("seed one churn row");

    let bridge = RepoGraphBridge::new(harness.state().clone());
    let result = bridge
        .hotspots(&hotspots_ctx(), 30, None, 10)
        .await
        .expect("hotspots should succeed");

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].file, "src/hot.rs");
}
