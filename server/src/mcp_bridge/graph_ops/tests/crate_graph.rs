use super::*;
use crate::mcp_bridge::graph_ops::query_helpers::crate_graph_from_warmed_cache;
use djinn_graph::canonical_graph::{CachedGraph, GRAPH_CACHE, derive_graph_caches};
use djinn_graph::repo_graph::{
    REPO_GRAPH_ARTIFACT_VERSION, RepoDependencyGraph, RepoGraphArtifact, RepoGraphNode,
    RepoGraphNodeKind, RepoNodeKey, RouteExclusionConfig,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

static GRAPH_CACHE_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

fn crate_graph_ctx(clone_path: &str) -> ProjectCtx {
    ProjectCtx {
        id: "crate-graph-test-project".to_string(),
        clone_path: clone_path.to_string(),
        workspace: None,
        sub_path: None,
    }
}

fn one_crate_graph() -> RepoDependencyGraph {
    let node = RepoGraphNode {
        id: RepoNodeKey::File(PathBuf::from("crate-a/src/lib.rs")),
        kind: RepoGraphNodeKind::File,
        display_name: "crate-a/src/lib.rs".to_string(),
        language: Some("rust".to_string()),
        file_path: Some(PathBuf::from("crate-a/src/lib.rs")),
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
    RepoDependencyGraph::from_artifact(&RepoGraphArtifact {
        version: REPO_GRAPH_ARTIFACT_VERSION,
        nodes: vec![node],
        edges: vec![],
        symbol_ranges: BTreeMap::new(),
        communities: vec![],
        processes: vec![],
        route_exclusion_config: RouteExclusionConfig::default(),
        layout_positions: BTreeMap::new(),
        galaxy_positions: BTreeMap::new(),
        galaxy_degrees: BTreeMap::new(),
    })
}

async fn install_cached_graph(
    clone_path: &str,
    graph: RepoDependencyGraph,
    crate_map: BTreeMap<PathBuf, String>,
) {
    let (_project_root, index_tree_path) =
        djinn_graph::canonical_graph::normalize_graph_query_paths(clone_path);
    let (pagerank, sccs, layout_positions, _) =
        derive_graph_caches(&graph, Path::new("/var/tmp/djinn-crate-graph-test"));
    let mut cache = GRAPH_CACHE.write().await;
    *cache = Some(CachedGraph {
        graph: Arc::new(graph),
        project_path: index_tree_path,
        git_head: "test-head".to_string(),
        pagerank,
        sccs,
        layout_positions,
        crate_map: Arc::new(crate_map),
    });
}

#[tokio::test(flavor = "current_thread")]
async fn crate_graph_returns_message_when_graph_cache_is_empty() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().await;
    *GRAPH_CACHE.write().await = None;

    let response = crate_graph_from_warmed_cache(&crate_graph_ctx("/workspace/no-cache"))
        .await
        .expect("crate_graph should return a non-error empty response");

    assert!(response.crates.is_empty());
    assert!(response.edges.is_empty());
    assert_eq!(
        response.message.as_deref(),
        Some("Graph not warmed for this workspace.")
    );

    *GRAPH_CACHE.write().await = None;
}

#[tokio::test(flavor = "current_thread")]
async fn crate_graph_returns_message_when_crate_map_is_empty() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().await;
    let clone_path = "/workspace/empty-crate-map";
    install_cached_graph(clone_path, one_crate_graph(), BTreeMap::new()).await;

    let response = crate_graph_from_warmed_cache(&crate_graph_ctx(clone_path))
        .await
        .expect("crate_graph should return a non-error empty response");

    assert!(response.crates.is_empty());
    assert!(response.edges.is_empty());
    assert_eq!(
        response.message.as_deref(),
        Some("No crate mapping found — not a Rust workspace or workspace not yet warmed.")
    );

    *GRAPH_CACHE.write().await = None;
}

#[tokio::test(flavor = "current_thread")]
async fn crate_graph_maps_warmed_crate_graph_to_bridge_response() {
    let _guard = GRAPH_CACHE_TEST_LOCK.lock().await;
    let clone_path = "/workspace/warmed-crate-map";
    let crate_map = BTreeMap::from([(PathBuf::from("crate-a"), "crate-a".to_string())]);
    install_cached_graph(clone_path, one_crate_graph(), crate_map).await;

    let response = crate_graph_from_warmed_cache(&crate_graph_ctx(clone_path))
        .await
        .expect("crate_graph should aggregate the warmed graph");

    assert_eq!(response.message, None);
    assert_eq!(response.crates.len(), 1);
    assert_eq!(response.crates[0].name, "crate-a");
    assert_eq!(response.crates[0].manifest_path, "crate-a");
    assert_eq!(response.crates[0].node_count, 1);
    assert!(response.edges.is_empty());

    *GRAPH_CACHE.write().await = None;
}
