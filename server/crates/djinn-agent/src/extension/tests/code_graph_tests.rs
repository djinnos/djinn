// djinn:allow-oversize
use super::*;

// -----------------------------------------------------------------------
// code_graph dispatch tests
// -----------------------------------------------------------------------

/// Helper to invoke the `code_graph` tool through the public `call_tool` boundary.
async fn code_graph_tool(
    state: &AgentContext,
    args: serde_json::Value,
    worktree: &Path,
) -> Result<serde_json::Value, String> {
    call_tool(
        state,
        &crate::test_helpers::test_services(),
        "code_graph",
        args.as_object()
            .expect("code_graph args must be an object")
            .clone()
            .into(),
        worktree,
        None,
        None,
        None,
        None,
        &crate::extension::ToolCancellation::never(),
    )
    .await
    .into_test_result()
}

#[derive(Clone)]
struct TraversalDispatchStub {
    neighbors: djinn_control_plane::bridge::NeighborsResult,
    impact: djinn_control_plane::bridge::ImpactResult,
}

fn detailed_neighbors() -> djinn_control_plane::bridge::NeighborsResult {
    djinn_control_plane::bridge::NeighborsResult::Detailed(vec![
        neighbor("symbol:a"),
        neighbor("symbol:b"),
        neighbor("symbol:c"),
        neighbor("symbol:d"),
    ])
}

fn neighbor(key: &str) -> djinn_control_plane::bridge::GraphNeighbor {
    djinn_control_plane::bridge::GraphNeighbor {
        key: key.to_string(),
        uid: key.to_string(),
        kind: "symbol".to_string(),
        display_name: key.trim_start_matches("symbol:").to_string(),
        edge_kind: "Calls".to_string(),
        edge_weight: 1.0,
        direction: "outgoing".to_string(),
    }
}

fn detailed_impact() -> djinn_control_plane::bridge::ImpactResult {
    djinn_control_plane::bridge::ImpactResult::Detailed(vec![
        impact_entry("symbol:a", 1),
        impact_entry("symbol:b", 2),
        impact_entry("symbol:c", 2),
        impact_entry("symbol:d", 3),
    ])
}

fn impact_entry(key: &str, depth: usize) -> djinn_control_plane::bridge::ImpactEntry {
    djinn_control_plane::bridge::ImpactEntry {
        key: key.to_string(),
        uid: key.to_string(),
        depth,
        file_path: Some(format!("src/{}.rs", key.trim_start_matches("symbol:"))),
        confidence_tier: None,
        exclusion_reason: None,
    }
}

async fn dispatch_traversal_stub(
    mut params: CodeGraphParams,
    stub: TraversalDispatchStub,
) -> serde_json::Value {
    params.normalize();
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: "project-1".to_string(),
        clone_path: "/repo".to_string(),
        workspace: None,
        sub_path: None,
    };
    call_code_graph_inner(&state, &mut params, &ctx, &stub)
        .await
        .expect("code_graph traversal dispatch should serialize")
}

#[async_trait::async_trait]
impl djinn_control_plane::bridge::RepoGraphOps for TraversalDispatchStub {
    async fn neighbors(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<djinn_control_plane::bridge::NeighborsResult, String> {
        Ok(self.neighbors.clone())
    }

    async fn impact(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: &str,
        _: usize,
        _: Option<&str>,
        _: Option<f64>,
    ) -> Result<djinn_control_plane::bridge::ImpactResult, String> {
        Ok(self.impact.clone())
    }

    async fn ranked(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::RankedNode>, String> {
        Err("not used".into())
    }
    async fn implementations(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
    ) -> Result<Vec<String>, String> {
        Err("not used".into())
    }
    async fn search(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::SearchHit>, String> {
        Err("not used".into())
    }
    async fn cycles(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CycleGroup>, String> {
        Err("not used".into())
    }
    async fn orphans(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::OrphanEntry>, String> {
        Err("not used".into())
    }
    async fn path(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: &str,
        _: &str,
        _: Option<usize>,
    ) -> Result<Option<djinn_control_plane::bridge::PathResult>, String> {
        Err("not used".into())
    }
    async fn edges(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::EdgeEntry>, String> {
        Err("not used".into())
    }
    async fn describe(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
    ) -> Result<Option<djinn_control_plane::bridge::SymbolDescription>, String> {
        Err("not used".into())
    }
    async fn context(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: bool,
    ) -> Result<Option<djinn_control_plane::bridge::SymbolContext>, String> {
        Err("not used".into())
    }
    async fn status(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
    ) -> Result<djinn_control_plane::bridge::GraphStatus, String> {
        Err("not used".into())
    }
    async fn snapshot(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: djinn_control_plane::bridge::SnapshotLevel,
        _: usize,
        _: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    ) -> Result<djinn_control_plane::bridge::SnapshotPayload, String> {
        Err("not used".into())
    }
    async fn symbols_at(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: u32,
        _: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::SymbolAtHit>, String> {
        Err("not used".into())
    }
    async fn diff_touches(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &[djinn_control_plane::bridge::ChangedRange],
    ) -> Result<djinn_control_plane::bridge::DiffTouchesResult, String> {
        Err("not used".into())
    }
    async fn detect_changes(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: &[String],
    ) -> Result<djinn_control_plane::bridge::DetectedChangesResult, String> {
        Err("not used".into())
    }
    async fn api_surface(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::ApiSurfaceEntry>, String> {
        Err("not used".into())
    }
    async fn boundary_check(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &[djinn_control_plane::bridge::BoundaryRule],
        _: &str,
    ) -> Result<Vec<djinn_control_plane::bridge::BoundaryViolation>, String> {
        Err("not used".into())
    }
    async fn hotspots(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: u32,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::HotspotEntry>, String> {
        Err("not used".into())
    }
    async fn complexity(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<djinn_control_plane::bridge::ComplexityResult, String> {
        Err("not used".into())
    }
    async fn refactor_candidates(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<u32>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::RefactorCandidate>, String> {
        Err("not used".into())
    }
    async fn metrics_at(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
    ) -> Result<djinn_control_plane::bridge::MetricsAtResult, String> {
        Err("not used".into())
    }
    async fn dead_symbols(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::DeadSymbolEntry>, String> {
        Err("not used".into())
    }
    async fn deprecated_callers(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::DeprecatedHit>, String> {
        Err("not used".into())
    }
    async fn touches_hot_path(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: &[String],
        _: &[String],
        _: &[String],
    ) -> Result<Vec<djinn_control_plane::bridge::HotPathHit>, String> {
        Err("not used".into())
    }
    async fn coupling(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingEntry>, String> {
        Err("not used".into())
    }
    async fn churn(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::ChurnEntry>, String> {
        Err("not used".into())
    }
    async fn coupling_hotspots(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CoupledPairEntry>, String> {
        Err("not used".into())
    }
    async fn coupling_hubs(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingHubEntry>, String> {
        Err("not used".into())
    }
    async fn resolve(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
    ) -> Result<djinn_control_plane::bridge::ResolveOutcome, String> {
        Err("not used".into())
    }
}

fn traversal_stub() -> TraversalDispatchStub {
    TraversalDispatchStub {
        neighbors: detailed_neighbors(),
        impact: detailed_impact(),
    }
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_applies_offset_page_limit_metadata() {
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "symbol:root",
        "offset": 1,
        "pageLimit": 2
    }))
    .expect("neighbors params parse");

    let value = dispatch_traversal_stub(params, traversal_stub()).await;

    assert_eq!(value["key"], "symbol:root");
    assert_eq!(value["total"], 4);
    assert_eq!(value["offset"], 1);
    assert_eq!(value["limit"], 2);
    assert_eq!(value["has_more"], true);
    let neighbors = value["neighbors"].as_array().expect("neighbors array");
    assert_eq!(neighbors.len(), 2);
    assert_eq!(neighbors[0]["key"], "symbol:b");
    assert_eq!(neighbors[1]["key"], "symbol:c");
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_summary_only_omits_neighbors() {
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "symbol:root",
        "summaryOnly": true
    }))
    .expect("neighbors params parse");

    let value = dispatch_traversal_stub(params, traversal_stub()).await;

    assert_eq!(value["key"], "symbol:root");
    assert_eq!(value["summary_only"], true);
    assert_eq!(value["total"], 4);
    assert!(
        value.get("neighbors").is_none(),
        "summaryOnly must omit neighbors from serialized dispatch response: {value}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_full_page_has_more_false() {
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "symbol:root",
        "pageLimit": 4
    }))
    .expect("neighbors params parse");

    let value = dispatch_traversal_stub(params, traversal_stub()).await;

    let neighbors = value["neighbors"].as_array().expect("neighbors array");
    assert_eq!(neighbors.len(), 4);
    assert!(
        value.get("has_more").is_none(),
        "full pages should not emit has_more metadata: {value}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_impact_by_depth_counts_keeps_entries() {
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "impact",
        "key": "symbol:root",
        "limit": 3,
        "byDepthCounts": true,
        "pageLimit": 3
    }))
    .expect("impact params parse");

    let value = dispatch_traversal_stub(params, traversal_stub()).await;

    let impact = value["impact"].as_array().expect("impact array");
    assert_eq!(impact.len(), 3);
    assert_eq!(value["by_depth_counts"]["1"], 1);
    assert_eq!(value["by_depth_counts"]["2"], 2);
    assert_eq!(value["by_depth_counts"]["3"], 1);
    assert_eq!(value["has_more"], true);
}

#[tokio::test]
async fn code_graph_dispatch_impact_summary_only_keeps_depth_counts_omits_impact() {
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "impact",
        "key": "symbol:root",
        "summaryOnly": true,
        "byDepthCounts": true,
        "pageLimit": 2
    }))
    .expect("impact params parse");

    let value = dispatch_traversal_stub(params, traversal_stub()).await;

    assert_eq!(value["summary_only"], true);
    assert_eq!(value["by_depth_counts"]["1"], 1);
    assert_eq!(value["by_depth_counts"]["2"], 2);
    assert_eq!(value["by_depth_counts"]["3"], 1);
    assert!(
        value.get("impact").is_none(),
        "summaryOnly must omit impact from serialized dispatch response: {value}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-neighbors-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "neighbors",
            "project_path": worktree.path().to_string_lossy(),
            "key": "src/lib.rs",
            "direction": "outgoing"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    // The agent bridge stub rejects with a known message.
    assert!(
        err.contains("code_graph not available"),
        "neighbors should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_ranked_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-ranked-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "ranked",
            "project_path": worktree.path().to_string_lossy(),
            "kind_filter": "file",
            "limit": 10
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("code_graph not available"),
        "ranked should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_impact_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-impact-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "impact",
            "project_path": worktree.path().to_string_lossy(),
            "key": "rust-analyzer cargo . MyStruct#",
            "limit": 5
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("code_graph not available"),
        "impact should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_implementations_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-impls-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "implementations",
            "project_path": worktree.path().to_string_lossy(),
            "key": "rust-analyzer cargo . MyTrait#"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("code_graph not available"),
        "implementations should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_rejects_unknown_operation() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-unknown-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "shortest_path",
            "project_path": worktree.path().to_string_lossy(),
            "key": "src/lib.rs"
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("unknown code_graph operation 'shortest_path'"),
        "expected unknown-operation error, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_requires_key() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-no-key-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "neighbors",
            "project_path": worktree.path().to_string_lossy()
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("'key' is required"),
        "neighbors without key should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_impact_requires_key() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-impact-no-key-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "impact",
            "project_path": worktree.path().to_string_lossy()
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("'key' is required"),
        "impact without key should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_implementations_requires_key() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-impls-no-key-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "implementations",
            "project_path": worktree.path().to_string_lossy()
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();

    assert!(
        err.contains("'key' is required"),
        "implementations without key should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_search_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-search-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "search",
            "project_path": worktree.path().to_string_lossy(),
            "query": "AgentSession",
            "limit": 5,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "search should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_search_requires_query() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-search-no-query-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "search",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'query' is required"),
        "search without query should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_query_subgraph_reaches_graph_ops_with_filters() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-query-subgraph-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "query_subgraph",
            "project_path": worktree.path().to_string_lossy(),
            "workspace": "default",
            "query": "How does auth routing reach middleware?",
            "context_filter": " auth ",
            "file_filter": "src/auth",
            "kind_filter": "symbol",
            "edge_filters": [" Calls ", "IMPORTS"],
            "token_budget": 2048,
            "max_depth": 2,
            "max_seeds": 4,
        }),
        worktree.path(),
    )
    .await
    .expect("query_subgraph should dispatch through graph ops");
    assert_eq!(
        result["query_subgraph"]["query"],
        "How does auth routing reach middleware?"
    );
    assert!(
        result["query_subgraph"]["budget"].is_object(),
        "query_subgraph response should expose budget/truncation state: {result}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_query_subgraph_requires_nonblank_query() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-query-subgraph-no-query-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "query_subgraph",
            "project_path": worktree.path().to_string_lossy(),
            "query": "   ",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'query' is required for operation 'query_subgraph'"),
        "query_subgraph without nonblank query should fail, got: {err}"
    );
}

// -----------------------------------------------------------------------
// wave-1 cross-layer regression coverage. The unit-level tests above
// already assert dispatch reachability + missing/blank query rejection.
// The tests below extend the public response surface with the
// agent-safety properties the spec calls out: bounded payload shape,
// seed debug metadata, narrowing hints, and stable-UID follow-up
// compatibility.
//
// The agent-side path uses `agent_context_from_db`, which wires the
// `StubRepoGraphOps` from `context.rs` — its default `query_subgraph`
// returns a well-formed empty `QuerySubgraphResult`. This is the
// right level for an end-to-end shape test: we are not re-testing
// graph-layer behaviour (covered by the fixture tests in
// `djinn-graph`), we are locking down what the agent extension
// surfaces to MCP/chat clients when the real bridge is offline.
// -----------------------------------------------------------------------

/// Acceptance criterion #5 (agent side) — the public response shape
/// always carries seed debug metadata scaffolding even when the
/// bridge returns an empty result. Agents rely on
/// `query_subgraph.seeds` being an array (possibly empty) so they
/// can iterate without null checks; a regression that omits the
/// field would break the contract.
#[tokio::test]
async fn code_graph_dispatch_query_subgraph_response_carries_seed_metadata_scaffold() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-query-subgraph-seeds-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "query_subgraph",
            "project_path": worktree.path().to_string_lossy(),
            "query": "what does the auth subsystem touch",
        }),
        worktree.path(),
    )
    .await
    .expect("query_subgraph should dispatch through graph ops");
    let payload = result
        .get("query_subgraph")
        .and_then(|v| v.as_object())
        .expect("query_subgraph discriminator object present in agent response");
    for field in [
        "query",
        "nodes",
        "edges",
        "seeds",
        "inferred_edge_kinds",
        "budget",
        "traversal",
        "narrowing_hints",
    ] {
        assert!(
            payload.contains_key(field),
            "query_subgraph response missing required public field {field}: {payload:?}"
        );
    }
    // `seeds` must always be an array (possibly empty) so the agent
    // can iterate. The empty stub path is what the agent sees when
    // the bridge is unavailable, so this is the right place to lock
    // the contract.
    assert!(
        payload["seeds"].is_array(),
        "query_subgraph response `seeds` field must be an array, got {:?}",
        payload["seeds"]
    );
    assert!(
        payload["nodes"].is_array(),
        "query_subgraph response `nodes` field must be an array"
    );
    assert!(
        payload["edges"].is_array(),
        "query_subgraph response `edges` field must be an array"
    );
    assert!(
        payload["inferred_edge_kinds"].is_array(),
        "query_subgraph response `inferred_edge_kinds` field must be an array"
    );
    assert!(
        payload["narrowing_hints"].is_array(),
        "query_subgraph response `narrowing_hints` field must be an array"
    );
    // `budget` and `traversal` are required debug objects — a
    // regression that returns them as null or omits them would
    // strip the agent of the source-level signal it needs to
    // decide whether to retry with a narrower question.
    assert!(
        payload["budget"].is_object(),
        "query_subgraph response `budget` must be an object, got {:?}",
        payload["budget"]
    );
    assert!(
        payload["traversal"].is_object(),
        "query_subgraph response `traversal` must be an object, got {:?}",
        payload["traversal"]
    );
}

/// Acceptance criterion #1 (agent side) — the `budget` object
/// always carries `truncated` / `omitted_nodes` / `omitted_edges`
/// fields, even when the bridge returns the empty default. The
/// flag trio is what agents read to decide "do I need to retry
/// with a tighter filter" — missing fields would force a special
/// case that drifts out of sync with the control-plane snapshot.
#[tokio::test]
async fn code_graph_dispatch_query_subgraph_budget_block_carries_truncation_state() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-query-subgraph-budget-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "query_subgraph",
            "project_path": worktree.path().to_string_lossy(),
            "query": "broad auth question",
        }),
        worktree.path(),
    )
    .await
    .expect("query_subgraph dispatch succeeds");
    let budget = result["query_subgraph"]["budget"]
        .as_object()
        .expect("query_subgraph response carries a `budget` object");
    for field in [
        "requested_tokens",
        "estimated_tokens",
        "truncated",
        "omitted_nodes",
        "omitted_edges",
    ] {
        assert!(
            budget.contains_key(field),
            "query_subgraph `budget` object missing field {field}: {budget:?}"
        );
    }
    // `truncated` must be a boolean so the agent can branch on it
    // without parsing strings. A regression that returned
    // `truncated: 0` (integer) or omitted the field would break
    // the standard `if response.budget.truncated` pattern agents
    // use to decide whether to retry.
    assert!(
        budget["truncated"].is_boolean(),
        "query_subgraph `budget.truncated` must be a boolean, got {:?}",
        budget["truncated"]
    );
    assert!(
        budget["requested_tokens"].is_number(),
        "query_subgraph `budget.requested_tokens` must be a number"
    );
    assert!(
        budget["omitted_nodes"].is_number(),
        "query_subgraph `budget.omitted_nodes` must be a number"
    );
    assert!(
        budget["omitted_edges"].is_number(),
        "query_subgraph `budget.omitted_edges` must be a number"
    );
}

/// Acceptance criterion #2 (agent side) — the `traversal` object
/// always carries the hub-avoidance scaffolding (`max_depth`,
/// `hub_degree_threshold`, `hubs_blocked`, `skipped_edge_kinds`).
/// Even when the stub returns an empty traversal debug block, the
/// shape must match the control-plane snapshot so schema
/// generation in the agent extension doesn't drift.
#[tokio::test]
async fn code_graph_dispatch_query_subgraph_traversal_block_carries_hub_avoidance_scaffold() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-query-subgraph-traversal-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "query_subgraph",
            "project_path": worktree.path().to_string_lossy(),
            "query": "broad auth question",
        }),
        worktree.path(),
    )
    .await
    .expect("query_subgraph dispatch succeeds");
    let traversal = result["query_subgraph"]["traversal"]
        .as_object()
        .expect("query_subgraph response carries a `traversal` object");
    for field in [
        "max_depth",
        "hub_degree_threshold",
        "hubs_blocked",
        "skipped_edge_kinds",
    ] {
        assert!(
            traversal.contains_key(field),
            "query_subgraph `traversal` object missing field {field}: {traversal:?}"
        );
    }
    assert!(
        traversal["hubs_blocked"].is_array(),
        "query_subgraph `traversal.hubs_blocked` must be an array (possibly empty)"
    );
    assert!(
        traversal["skipped_edge_kinds"].is_array(),
        "query_subgraph `traversal.skipped_edge_kinds` must be an array (possibly empty)"
    );
}

/// Acceptance criterion #4 (agent side) — the natural-language
/// `query` echoes back to the agent verbatim, so the agent can
/// use the response to confirm "yes, this is about auth routing"
/// without re-reading the original prompt. We deliberately use
/// leading/trailing whitespace + mixed case to verify the
/// extension layer trims before forwarding (matches the
/// `params.normalize()` contract).
#[tokio::test]
async fn code_graph_dispatch_query_subgraph_echoes_trimmed_natural_language_query() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-query-subgraph-echo-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "query_subgraph",
            "project_path": worktree.path().to_string_lossy(),
            // Leading whitespace + mixed case; the extension must
            // trim/normalize before forwarding to the bridge.
            "query": "  How does the AUTH middleware work?  ",
        }),
        worktree.path(),
    )
    .await
    .expect("query_subgraph dispatch succeeds with trimmed natural-language query");
    let echoed = result["query_subgraph"]["query"]
        .as_str()
        .expect("query_subgraph response echoes the natural-language question");
    assert_eq!(
        echoed, "How does the AUTH middleware work?",
        "query_subgraph response must echo the trimmed natural-language question verbatim, got {echoed:?}"
    );
}

/// Acceptance criterion #3 (agent side) — natural-language edge
/// intent inference works through the agent dispatch. The agent
/// extension does not strip the question wording before
/// forwarding, so any future "smart pre-rewrite" pass that loses
/// intent-bearing keywords (calls, reads, writes, implements,
/// imports) would break this test loudly.
#[tokio::test]
async fn code_graph_dispatch_query_subgraph_preserves_intent_bearing_query_wording() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-query-subgraph-wording-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    // Loop over the five phrasings the spec calls out. Each is
    // passed verbatim to the agent extension and the response
    // must echo it back. We don't assert what the bridge inferred
    // here (the stub returns an empty inferred_edge_kinds list);
    // we only assert the wording round-trip is lossless, which
    // is the agent-side half of the contract.
    for wording in [
        "who calls the login function",
        "who reads the users table",
        "who writes the audit log",
        "implementations of the Auth trait",
        "imports from internal/auth",
    ] {
        let result = code_graph_tool(
            &state,
            serde_json::json!({
                "operation": "query_subgraph",
                "project_path": worktree.path().to_string_lossy(),
                "query": wording,
            }),
            worktree.path(),
        )
        .await
        .unwrap_or_else(|err| {
            panic!("query_subgraph with wording {wording:?} should dispatch, got: {err}")
        });
        let echoed = result["query_subgraph"]["query"]
            .as_str()
            .unwrap_or_else(|| {
                panic!("query_subgraph response missing echoed query for {wording:?}")
            });
        assert_eq!(
            echoed, wording,
            "agent extension must round-trip intent-bearing wording {wording:?} verbatim"
        );
    }
}

/// Acceptance criterion #1 (companion, agent side) — invalid
/// budget values must be rejected through the agent dispatch
/// path, not silently forwarded to the bridge. The user-facing
/// message must name the field so the model can self-correct.
#[tokio::test]
async fn code_graph_dispatch_query_subgraph_rejects_zero_token_budget_with_field_named_error() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-query-subgraph-zero-budget-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "query_subgraph",
            "project_path": worktree.path().to_string_lossy(),
            "query": "anything",
            "token_budget": 0,
        }),
        worktree.path(),
    )
    .await
    .expect_err("zero token_budget must be rejected through agent dispatch");
    assert!(
        err.contains("token_budget"),
        "agent must surface the offending field name in the error, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_cycles_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-cycles-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "cycles",
            "project_path": worktree.path().to_string_lossy(),
            "min_size": 2,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "cycles should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_orphans_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-orphans-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "orphans",
            "project_path": worktree.path().to_string_lossy(),
            "visibility": "private",
            "limit": 10,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "orphans should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_path_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-path-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "path",
            "project_path": worktree.path().to_string_lossy(),
            "from": "src/a.rs",
            "to": "src/b.rs",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "path should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_path_requires_from_and_to() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-path-missing-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "path",
            "project_path": worktree.path().to_string_lossy(),
            "from": "src/a.rs",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'to' is required"),
        "path without 'to' should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_edges_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-edges-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "edges",
            "project_path": worktree.path().to_string_lossy(),
            "from_glob": "server/src/**",
            "to_glob": "server/crates/**",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "edges should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_edges_requires_globs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-edges-missing-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "edges",
            "project_path": worktree.path().to_string_lossy(),
            "from_glob": "server/src/**",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'to_glob' is required"),
        "edges without to_glob should fail, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_describe_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-describe-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "describe",
            "project_path": worktree.path().to_string_lossy(),
            "key": "scip-rust . . . AgentSession#",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "describe should reach graph ops layer, got: {err}"
    );
}

/// v8 cochange op: routes through `RepoGraphOps::coupling`. Agent stub
/// returns "code_graph not available" — same pattern as every other
/// dispatch test. Verifies wiring rather than empty-state semantics.
#[tokio::test]
async fn code_graph_dispatch_cochange_with_key_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-cochange-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "cochange",
            "project_path": worktree.path().to_string_lossy(),
            "key": "file:internal/worker/page_worker.go",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "cochange-with-key should reach graph ops layer, got: {err}"
    );
}

/// v8 cochange without key routes through `RepoGraphOps::coupling_hotspots`.
#[tokio::test]
async fn code_graph_dispatch_cochange_without_key_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-cochange-pairs-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "cochange",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "cochange-without-key should reach graph ops layer, got: {err}"
    );
}

/// v8 churn op: routes through `RepoGraphOps::churn`. Same dispatch
/// test pattern.
#[tokio::test]
async fn code_graph_dispatch_churn_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-churn-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "churn",
            "project_path": worktree.path().to_string_lossy(),
            "limit": 10,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "churn should reach graph ops layer, got: {err}"
    );
}

/// v8 hotspots op: short-circuits cleanly when graph isn't warmed —
/// the underlying ranked() call hits the same "code_graph not available"
/// stub. Asserts the dispatch is wired even though the empty-state
/// behavior depends on warm + churn data.
#[tokio::test]
async fn code_graph_dispatch_hotspots_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-hotspots-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "hotspots",
            "project_path": worktree.path().to_string_lossy(),
            "limit": 5,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "hotspots should reach the bridge stub, got: {err}"
    );
}

/// Iter 28 complexity op: dispatches through
/// `RepoGraphOps::complexity` and surfaces the unavailability error
/// when the agent stub is in play. Confirms the new arm is wired.
#[tokio::test]
async fn code_graph_dispatch_complexity_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-complexity-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "complexity",
            "project_path": worktree.path().to_string_lossy(),
            "target": "functions",
            "sort_by": "cognitive",
            "limit": 5,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("complexity not available"),
        "complexity should reach the bridge stub, got: {err}"
    );
}

/// Iter 29 refactor_candidates op: dispatches through
/// `RepoGraphOps::refactor_candidates` and surfaces the unavailability
/// error when the agent stub is in play. Confirms the new arm is wired.
#[tokio::test]
async fn code_graph_dispatch_refactor_candidates_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-refactor-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "refactor_candidates",
            "project_path": worktree.path().to_string_lossy(),
            "since_days": 60,
            "limit": 5,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("refactor_candidates not available"),
        "refactor_candidates should reach the bridge stub, got: {err}"
    );
}

/// v8 final batch: 5 trait-delegation ops (status / snapshot /
/// symbols_at / diff_touches / detect_changes). Same pattern.
#[tokio::test]
async fn code_graph_dispatch_status_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-status-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "status",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_snapshot_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-snapshot-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "snapshot",
            "project_path": worktree.path().to_string_lossy(),
            "node_cap": 1000,
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_workspaces_passthrough_uses_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-workspaces-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "workspaces",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .expect("workspaces should use the RepoGraphOps workspaces contract");

    assert_eq!(
        result
            .get("workspaces")
            .and_then(|value| value.as_array())
            .map(Vec::len),
        Some(0),
        "default stub should return the trait passthrough shape: {result}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_symbols_at_validates_inputs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-symat-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "symbols_at",
            "project_path": worktree.path().to_string_lossy(),
            // Missing key + min_size — should hit arg validation.
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    // iter-21: error message updated to mention both new + legacy field names.
    assert!(
        err.contains("'file_path'") && err.contains("legacy 'key'"),
        "got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_diff_touches_validates_inputs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-diff-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "diff_touches",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("'changed_ranges' is required"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_detect_changes_validates_inputs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-dc-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "detect_changes",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("detect_changes requires"), "got: {err}");
}

/// v8 batch: 6 trait-delegation ops (api_surface / metrics_at /
/// dead_symbols / deprecated_callers / touches_hot_path /
/// coupling_hubs) all reach the agent bridge stub. One test per op
/// — deliberately uniform so adding the next trait op only needs a
/// tiny copy-paste here.
#[tokio::test]
async fn code_graph_dispatch_api_surface_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-api-surface-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "api_surface",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_metrics_at_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-metrics-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "metrics_at",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_dead_symbols_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-dead-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "dead_symbols",
            "project_path": worktree.path().to_string_lossy(),
            "kind_filter": "high",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_deprecated_callers_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-deprecated-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "deprecated_callers",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

#[tokio::test]
async fn code_graph_dispatch_touches_hot_path_validates_inputs() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-hotpath-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "touches_hot_path",
            "project_path": worktree.path().to_string_lossy(),
            // Missing the required from_glob/to_glob/query — should
            // fail with arg-validation message before reaching the
            // bridge stub.
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("touches_hot_path requires"),
        "should fail with arg-validation, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_coupling_hubs_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-hubs-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "coupling_hubs",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("code_graph not available"), "got: {err}");
}

/// v8 boundary_check op: reaches the bridge layer (which short-circuits
/// in agent-side stub mode). Asserts the dispatch wire is hooked up
/// AND that the rules-required validation fires before the bridge.
#[tokio::test]
async fn code_graph_dispatch_boundary_check_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-boundary-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "boundary_check",
            "project_path": worktree.path().to_string_lossy(),
            "rules": [
                {
                    "name": "domain-must-not-depend-on-transport",
                    "from_glob": "internal/domain/**",
                    "forbid_to": ["internal/api/**", "internal/transport/**"]
                }
            ]
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "boundary_check should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_boundary_check_requires_rules() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-boundary-no-rules-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "boundary_check",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'rules' is required"),
        "boundary_check without rules should fail with arg-validation error, got: {err}"
    );
}

/// v8 blast_radius op: aggregates `neighbors(incoming, group_by=file)`
/// + `impact(group_by=file)`, categorises each file path into
/// runtime/tests/e2e_tests buckets. The agent bridge stub still short-
/// circuits before reaching graph_ops, so this test asserts the op is
/// wired (reaches the bridge) rather than the categorizer logic — the
/// path-classification logic is exercised by direct unit tests in
/// `code_intel.rs`.
#[tokio::test]
async fn code_graph_dispatch_blast_radius_reaches_graph_ops() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-blast-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "blast_radius",
            "project_path": worktree.path().to_string_lossy(),
            "key": "file:internal/worker/page_worker.go",
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("code_graph not available"),
        "blast_radius should reach graph ops layer, got: {err}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_blast_radius_requires_key() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-blast-no-key-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let err = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "blast_radius",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("'key' is required"),
        "blast_radius without key should fail with arg-validation error, got: {err}"
    );
}

/// v8 capability introspection: returns metadata about what's actually
/// wired in this binary — does NOT load the canonical graph, so it
/// works against a fresh tempdir with no warm cache. Asserts the
/// payload shape so client agents can rely on the keys being present.
#[tokio::test]
async fn code_graph_dispatch_capabilities_returns_introspection_payload() {
    let worktree = crate::test_helpers::test_tempdir("djinn-cg-capabilities-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let result = code_graph_tool(
        &state,
        serde_json::json!({
            "operation": "capabilities",
            "project_path": worktree.path().to_string_lossy(),
        }),
        worktree.path(),
    )
    .await
    .expect("capabilities should not error");

    // Top-level keys clients depend on:
    let obj = result.as_object().expect("payload must be a JSON object");
    assert!(
        obj.contains_key("operations"),
        "missing operations: {result}"
    );
    assert!(
        obj.contains_key("default_search_mode"),
        "missing default_search_mode"
    );
    assert!(
        obj.contains_key("available_search_modes"),
        "missing available_search_modes"
    );
    assert!(obj.contains_key("env_features"), "missing env_features");
    assert!(
        obj.contains_key("access_classifier_languages"),
        "missing access_classifier_languages"
    );
    assert!(
        obj.contains_key("repo_graph_artifact_version"),
        "missing repo_graph_artifact_version"
    );
    assert!(obj.contains_key("filter_tiers"), "missing filter_tiers");
    assert!(
        obj.contains_key("default_filters"),
        "missing default_filters"
    );
    assert!(
        obj.contains_key("query_subgraph"),
        "missing query_subgraph capability contract"
    );

    // capabilities itself must list itself, otherwise clients can't
    // discover the op via probing.
    let ops = obj["operations"]
        .as_array()
        .expect("operations must be array");
    assert!(
        ops.iter().any(|o| o.as_str() == Some("capabilities")),
        "capabilities op must list itself in `operations`"
    );

    // Artifact version stamp follows the canonical repo-graph artifact schema.
    assert_eq!(obj["repo_graph_artifact_version"], 10);

    assert!(
        ops.iter().any(|o| o.as_str() == Some("workspaces")),
        "workspaces op must be listed in capabilities"
    );

    // Natural-language subgraph queries must be discoverable without
    // consulting the external MCP schema snapshot. This locks the chat-facing
    // parameter mirror to the final control-plane names and narrowing semantics.
    let subgraph = obj["query_subgraph"]
        .as_object()
        .expect("query_subgraph capability must be object");
    assert_eq!(subgraph["operation"], "query_subgraph");
    assert_eq!(subgraph["required"], serde_json::json!(["query"]));
    for field in [
        "workspace",
        "context_filter",
        "file_filter",
        "kind_filter",
        "edge_filters",
        "max_depth",
        "max_seeds",
        "token_budget",
    ] {
        assert!(
            subgraph["optional_filters"].get(field).is_some(),
            "query_subgraph capability missing optional filter {field}: {result}"
        );
    }
    let response_fields = subgraph["response"]["fields"]
        .as_array()
        .expect("response fields must be array");
    for field in [
        "nodes",
        "edges",
        "seeds",
        "budget",
        "traversal",
        "narrowing_hints",
    ] {
        assert!(
            response_fields
                .iter()
                .any(|value| value.as_str() == Some(field)),
            "query_subgraph response capability missing {field}: {result}"
        );
    }

    // Languages we ship a tree-sitter classifier for.
    let langs = obj["access_classifier_languages"]
        .as_array()
        .expect("languages must be array");
    for required in ["rust", "go", "python", "typescript", "javascript"] {
        assert!(
            langs.iter().any(|l| l.as_str() == Some(required)),
            "missing language {required} in access_classifier_languages"
        );
    }
}

#[test]
fn code_graph_workspace_traversal_keeps_seed_resolution_in_backend() {
    let mut impact: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "impact",
        "workspace": "server",
        "key": "Handler",
    }))
    .expect("impact params parse");
    impact.normalize();
    assert!(
        !should_pre_resolve_chat_key(&impact),
        "workspace-scoped impact must let RepoGraphOps resolve the seed inside the workspace"
    );

    let mut path: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "path",
        "workspace": "server",
        "from": "Handler",
        "to": "Database",
    }))
    .expect("path params parse");
    path.normalize();
    assert!(
        !should_pre_resolve_chat_key(&path),
        "workspace-scoped path must let RepoGraphOps resolve endpoints inside the workspace"
    );

    let mut unscoped: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "impact",
        "workspace": "",
        "key": "Handler",
    }))
    .expect("unscoped params parse");
    unscoped.normalize();
    assert!(
        should_pre_resolve_chat_key(&unscoped),
        "empty workspace normalizes away, preserving legacy chat pre-resolution"
    );

    let mut listing: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "ranked",
        "workspace": "server",
    }))
    .expect("listing params parse");
    listing.normalize();
    assert!(
        should_pre_resolve_chat_key(&listing),
        "listing/bounded ops can still use normal dispatch; only traversal seeds are special"
    );
}

// -----------------------------------------------------------------------
// wraw: graph_staleness contract on the chat dispatch path.
//
// Mirrors the control-plane jc47 contract: when the caller passes
// `current_head`, the chat dispatcher's successful response should
// include an additive `graph_staleness` object comparing the trimmed
// caller commit against the cached graph blob's pinned commit. The
// flag is serve-stale-with-warning only: it never blocks the query and
// never triggers graph re-warming. A missing caller commit, a missing
// pinned commit, or a status lookup failure must NOT cause the field
// to appear.
// -----------------------------------------------------------------------

#[derive(Clone)]
struct StalenessDispatchStub {
    neighbors: djinn_control_plane::bridge::NeighborsResult,
    /// `pinned_commit` returned by `status()`. `None` mirrors an
    /// un-warmed cache so tests can exercise the non-stale-safe path.
    pinned_commit: Option<String>,
}

fn staleness_stub(pinned_commit: Option<&str>) -> StalenessDispatchStub {
    StalenessDispatchStub {
        neighbors: detailed_neighbors(),
        pinned_commit: pinned_commit.map(str::to_string),
    }
}

impl StalenessDispatchStub {
    fn status_value(&self) -> djinn_control_plane::bridge::GraphStatus {
        djinn_control_plane::bridge::GraphStatus {
            project_id: "project-1".to_string(),
            warmed: self.pinned_commit.is_some(),
            last_warm_at: None,
            pinned_commit: self.pinned_commit.clone(),
            commits_since_pin: None,
            route_parity_enabled: false,
            route_exclusion_config: serde_json::Value::Null,
        }
    }
}

#[async_trait::async_trait]
impl djinn_control_plane::bridge::RepoGraphOps for StalenessDispatchStub {
    async fn neighbors(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<djinn_control_plane::bridge::NeighborsResult, String> {
        Ok(self.neighbors.clone())
    }
    async fn status(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
    ) -> Result<djinn_control_plane::bridge::GraphStatus, String> {
        Ok(self.status_value())
    }
    async fn ranked(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::RankedNode>, String> {
        Err("not used".into())
    }
    async fn implementations(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
    ) -> Result<Vec<String>, String> {
        Err("not used".into())
    }
    async fn search(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::SearchHit>, String> {
        Err("not used".into())
    }
    async fn impact(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: &str,
        _: usize,
        _: Option<&str>,
        _: Option<f64>,
    ) -> Result<djinn_control_plane::bridge::ImpactResult, String> {
        Err("not used".into())
    }
    async fn cycles(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CycleGroup>, String> {
        Err("not used".into())
    }
    async fn orphans(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::OrphanEntry>, String> {
        Err("not used".into())
    }
    async fn path(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: &str,
        _: &str,
        _: Option<usize>,
    ) -> Result<Option<djinn_control_plane::bridge::PathResult>, String> {
        Err("not used".into())
    }
    async fn edges(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::EdgeEntry>, String> {
        Err("not used".into())
    }
    async fn describe(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
    ) -> Result<Option<djinn_control_plane::bridge::SymbolDescription>, String> {
        Err("not used".into())
    }
    async fn context(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: bool,
    ) -> Result<Option<djinn_control_plane::bridge::SymbolContext>, String> {
        Err("not used".into())
    }
    async fn snapshot(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: djinn_control_plane::bridge::SnapshotLevel,
        _: usize,
        _: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    ) -> Result<djinn_control_plane::bridge::SnapshotPayload, String> {
        Err("not used".into())
    }
    async fn symbols_at(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: u32,
        _: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::SymbolAtHit>, String> {
        Err("not used".into())
    }
    async fn diff_touches(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &[djinn_control_plane::bridge::ChangedRange],
    ) -> Result<djinn_control_plane::bridge::DiffTouchesResult, String> {
        Err("not used".into())
    }
    async fn detect_changes(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: &[String],
    ) -> Result<djinn_control_plane::bridge::DetectedChangesResult, String> {
        Err("not used".into())
    }
    async fn api_surface(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::ApiSurfaceEntry>, String> {
        Err("not used".into())
    }
    async fn boundary_check(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &[djinn_control_plane::bridge::BoundaryRule],
        _: &str,
    ) -> Result<Vec<djinn_control_plane::bridge::BoundaryViolation>, String> {
        Err("not used".into())
    }
    async fn hotspots(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: u32,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::HotspotEntry>, String> {
        Err("not used".into())
    }
    async fn complexity(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<djinn_control_plane::bridge::ComplexityResult, String> {
        Err("not used".into())
    }
    async fn refactor_candidates(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<u32>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::RefactorCandidate>, String> {
        Err("not used".into())
    }
    async fn metrics_at(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
    ) -> Result<djinn_control_plane::bridge::MetricsAtResult, String> {
        Err("not used".into())
    }
    async fn dead_symbols(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::DeadSymbolEntry>, String> {
        Err("not used".into())
    }
    async fn deprecated_callers(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::DeprecatedHit>, String> {
        Err("not used".into())
    }
    async fn touches_hot_path(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _workspace: Option<&str>,
        _: &[String],
        _: &[String],
        _: &[String],
    ) -> Result<Vec<djinn_control_plane::bridge::HotPathHit>, String> {
        Err("not used".into())
    }
    async fn coupling(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingEntry>, String> {
        Err("not used".into())
    }
    async fn churn(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::ChurnEntry>, String> {
        Err("not used".into())
    }
    async fn coupling_hotspots(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CoupledPairEntry>, String> {
        Err("not used".into())
    }
    async fn coupling_hubs(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingHubEntry>, String> {
        Err("not used".into())
    }
    async fn resolve(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
    ) -> Result<djinn_control_plane::bridge::ResolveOutcome, String> {
        Err("not used".into())
    }
}

async fn dispatch_with_staleness_stub(
    mut params: CodeGraphParams,
    stub: StalenessDispatchStub,
) -> serde_json::Value {
    params.normalize();
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: "project-1".to_string(),
        clone_path: "/repo".to_string(),
        workspace: None,
        sub_path: None,
    };
    call_code_graph_inner(&state, &mut params, &ctx, &stub)
        .await
        .expect("code_graph dispatch should serialize")
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_attaches_graph_staleness_when_caller_head_matches() {
    // Wrapped op (neighbors -> emits `{ key, neighbors, ... }`): when
    // `current_head` matches the cached graph commit, the response
    // should include `graph_staleness` with `is_stale=false`.
    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "symbol:root",
        "current_head": "  abc123 "
    }))
    .expect("neighbors params parse");
    params.normalize();

    let value = dispatch_with_staleness_stub(params, staleness_stub(Some("abc123"))).await;

    assert_eq!(value["key"], "symbol:root");
    let staleness = value
        .get("graph_staleness")
        .expect("graph_staleness must be present when caller supplies current_head");
    assert_eq!(staleness["cached_commit"], "abc123");
    assert_eq!(staleness["caller_commit"], "abc123");
    assert_eq!(staleness["is_stale"], false);
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_attaches_graph_staleness_when_caller_head_differs() {
    // Same wrapped op: when the caller commit differs from the cached
    // commit, `is_stale=true` so the agent can warn the user.
    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "symbol:root",
        "current_head": "newer-sha"
    }))
    .expect("neighbors params parse");
    params.normalize();

    let value = dispatch_with_staleness_stub(params, staleness_stub(Some("older-sha"))).await;

    let staleness = value
        .get("graph_staleness")
        .expect("graph_staleness must be present when caller supplies current_head");
    assert_eq!(staleness["cached_commit"], "older-sha");
    assert_eq!(staleness["caller_commit"], "newer-sha");
    assert_eq!(staleness["is_stale"], true);
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_omits_graph_staleness_without_caller_head() {
    // No `current_head` passed — the response shape must be exactly
    // what callers got before the staleness contract landed (no
    // `graph_staleness` field). Backward compatibility.
    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "symbol:root"
    }))
    .expect("neighbors params parse");
    params.normalize();

    let value = dispatch_with_staleness_stub(params, staleness_stub(Some("abc123"))).await;

    assert_eq!(value["key"], "symbol:root");
    assert!(
        value.get("graph_staleness").is_none(),
        "no current_head supplied => no graph_staleness field: {value}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_omits_graph_staleness_when_caller_head_blank() {
    // `current_head: ""` is normalized to `None` by `CodeGraphParams::normalize()`,
    // so the field should still be absent — protects against chat-side
    // LLMs that emit every field as an empty string.
    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "symbol:root",
        "current_head": "   "
    }))
    .expect("neighbors params parse");
    params.normalize();
    assert!(
        params.current_head.is_none(),
        "blank current_head must normalize to None"
    );

    let value = dispatch_with_staleness_stub(params, staleness_stub(Some("abc123"))).await;

    assert!(
        value.get("graph_staleness").is_none(),
        "blank current_head normalizes to None => no graph_staleness field: {value}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_neighbors_graph_staleness_reports_unknown_when_cache_unwarmed() {
    // Wrapped op with `current_head` but the cache has no pinned
    // commit. The contract is non-stale-safe: `is_stale=false` and
    // `cached_commit` is absent. The query must still return the
    // graph result (no error, no block).
    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "symbol:root",
        "current_head": "abc123"
    }))
    .expect("neighbors params parse");
    params.normalize();

    let value = dispatch_with_staleness_stub(params, staleness_stub(None)).await;

    assert_eq!(value["key"], "symbol:root");
    let staleness = value
        .get("graph_staleness")
        .expect("graph_staleness present even when cache has no pinned commit");
    assert_eq!(staleness["caller_commit"], "abc123");
    assert_eq!(staleness["is_stale"], false);
    assert!(
        staleness.get("cached_commit").is_none(),
        "missing pinned commit must not invent a cached_commit value: {staleness}"
    );
}

#[tokio::test]
async fn code_graph_dispatch_describe_serde_direct_attaches_graph_staleness() {
    // Serde-direct op (describe -> `serde_json::to_value(&description)`
    // emits a flat object with no agent-side wrapper). Confirms the
    // staleness field attaches to every response shape, not only
    // the wrapped ones.
    #[derive(Clone)]
    struct DescribeStub;
    #[async_trait::async_trait]
    impl djinn_control_plane::bridge::RepoGraphOps for DescribeStub {
        async fn describe(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
        ) -> Result<Option<djinn_control_plane::bridge::SymbolDescription>, String> {
            Ok(Some(djinn_control_plane::bridge::SymbolDescription {
                key: "symbol:root".to_string(),
                kind: "function".to_string(),
                display_name: "root".to_string(),
                file: Some("src/lib.rs".to_string()),
                start_line: Some(42),
                end_line: Some(50),
                signature: Some("fn root()".to_string()),
                documentation: Some("entry".to_string()),
                fan_in: 0,
                fan_out: 0,
                visibility: None,
                is_external: false,
                is_entry_point: false,
                is_test: false,
                complexity: None,
            }))
        }
        async fn status(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
        ) -> Result<djinn_control_plane::bridge::GraphStatus, String> {
            Ok(djinn_control_plane::bridge::GraphStatus {
                project_id: "project-1".to_string(),
                warmed: true,
                last_warm_at: None,
                pinned_commit: Some("abc123".to_string()),
                commits_since_pin: None,
                route_parity_enabled: false,
                route_exclusion_config: serde_json::Value::Null,
            })
        }
        async fn ranked(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::RankedNode>, String> {
            Err("not used".into())
        }
        async fn implementations(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
        ) -> Result<Vec<String>, String> {
            Err("not used".into())
        }
        async fn search(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::SearchHit>, String> {
            Err("not used".into())
        }
        async fn neighbors(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<djinn_control_plane::bridge::NeighborsResult, String> {
            Err("not used".into())
        }
        async fn impact(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: &str,
            _: usize,
            _: Option<&str>,
            _: Option<f64>,
        ) -> Result<djinn_control_plane::bridge::ImpactResult, String> {
            Err("not used".into())
        }
        async fn cycles(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::CycleGroup>, String> {
            Err("not used".into())
        }
        async fn orphans(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::OrphanEntry>, String> {
            Err("not used".into())
        }
        async fn path(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: &str,
            _: &str,
            _: Option<usize>,
        ) -> Result<Option<djinn_control_plane::bridge::PathResult>, String> {
            Err("not used".into())
        }
        async fn edges(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::EdgeEntry>, String> {
            Err("not used".into())
        }
        async fn context(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: bool,
        ) -> Result<Option<djinn_control_plane::bridge::SymbolContext>, String> {
            Err("not used".into())
        }
        async fn snapshot(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: djinn_control_plane::bridge::SnapshotLevel,
            _: usize,
            _: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
        ) -> Result<djinn_control_plane::bridge::SnapshotPayload, String> {
            Err("not used".into())
        }
        async fn symbols_at(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: u32,
            _: Option<u32>,
        ) -> Result<Vec<djinn_control_plane::bridge::SymbolAtHit>, String> {
            Err("not used".into())
        }
        async fn diff_touches(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &[djinn_control_plane::bridge::ChangedRange],
        ) -> Result<djinn_control_plane::bridge::DiffTouchesResult, String> {
            Err("not used".into())
        }
        async fn detect_changes(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: &[String],
        ) -> Result<djinn_control_plane::bridge::DetectedChangesResult, String> {
            Err("not used".into())
        }
        async fn api_surface(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: Option<&str>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::ApiSurfaceEntry>, String> {
            Err("not used".into())
        }
        async fn boundary_check(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &[djinn_control_plane::bridge::BoundaryRule],
            _: &str,
        ) -> Result<Vec<djinn_control_plane::bridge::BoundaryViolation>, String> {
            Err("not used".into())
        }
        async fn hotspots(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: u32,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::HotspotEntry>, String> {
            Err("not used".into())
        }
        async fn complexity(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<djinn_control_plane::bridge::ComplexityResult, String> {
            Err("not used".into())
        }
        async fn refactor_candidates(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<u32>,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::RefactorCandidate>, String> {
            Err("not used".into())
        }
        async fn metrics_at(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
        ) -> Result<djinn_control_plane::bridge::MetricsAtResult, String> {
            Err("not used".into())
        }
        async fn dead_symbols(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::DeadSymbolEntry>, String> {
            Err("not used".into())
        }
        async fn deprecated_callers(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::DeprecatedHit>, String> {
            Err("not used".into())
        }
        async fn touches_hot_path(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: Option<&str>,
            _: &[String],
            _: &[String],
            _: &[String],
        ) -> Result<Vec<djinn_control_plane::bridge::HotPathHit>, String> {
            Err("not used".into())
        }
        async fn coupling(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::CouplingEntry>, String> {
            Err("not used".into())
        }
        async fn churn(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: usize,
            _: Option<u32>,
        ) -> Result<Vec<djinn_control_plane::bridge::ChurnEntry>, String> {
            Err("not used".into())
        }
        async fn coupling_hotspots(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::CoupledPairEntry>, String> {
            Err("not used".into())
        }
        async fn coupling_hubs(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: usize,
            _: Option<u32>,
            _: usize,
        ) -> Result<Vec<djinn_control_plane::bridge::CouplingHubEntry>, String> {
            Err("not used".into())
        }
        async fn resolve(
            &self,
            _: &djinn_control_plane::bridge::ProjectCtx,
            _: &str,
            _: Option<&str>,
        ) -> Result<djinn_control_plane::bridge::ResolveOutcome, String> {
            Err("not used".into())
        }
    }

    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "describe",
        "key": "symbol:root",
        "current_head": "abc123"
    }))
    .expect("describe params parse");
    params.normalize();
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: "project-1".to_string(),
        clone_path: "/repo".to_string(),
        workspace: None,
        sub_path: None,
    };
    let value = call_code_graph_inner(&state, &mut params, &ctx, &DescribeStub)
        .await
        .expect("describe dispatch should serialize");

    // `describe` is serde-direct (calls `serde_json::to_value`); the
    // `graph_staleness` object must be added on top of the flat
    // `SymbolDescription` shape, NOT in a wrapper.
    assert_eq!(value["key"], "symbol:root");
    assert_eq!(value["kind"], "function");
    let staleness = value
        .get("graph_staleness")
        .expect("graph_staleness must attach to serde-direct describe response too");
    assert_eq!(staleness["cached_commit"], "abc123");
    assert_eq!(staleness["caller_commit"], "abc123");
    assert_eq!(staleness["is_stale"], false);
}

#[test]
fn code_graph_params_current_head_blank_normalizes_to_none() {
    // Acceptance: blank `current_head` and the camelCase / snake-case
    // aliases all parse cleanly. Blank / whitespace values normalize
    // to `None` so chat-side LLMs that emit every schema field as
    // `""` don't accidentally trigger staleness computation.
    // (Note: serde aliases are mutually exclusive — sending two aliases
    // in the same payload is a duplicate-field error, so each alias is
    // tested independently.)
    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "x",
        "current_head": "  "
    }))
    .expect("current_head blank params parse");
    params.normalize();
    assert!(
        params.current_head.is_none(),
        "blank current_head must normalize to None: {:?}",
        params.current_head
    );
    assert_eq!(params.current_head.as_deref(), None);
    assert_eq!(params.resolved_current_head(), None);

    // Blank via camelCase alias
    let mut params_blank_alias: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "x",
        "currentHead": "  "
    }))
    .expect("currentHead blank alias params parse");
    params_blank_alias.normalize();
    assert!(
        params_blank_alias.current_head.is_none(),
        "blank currentHead alias must normalize to None: {:?}",
        params_blank_alias.current_head
    );

    let mut params_alias: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "x",
        "currentHead": "abc123"
    }))
    .expect("camelCase alias parses");
    params_alias.normalize();
    assert_eq!(params_alias.current_head.as_deref(), Some("abc123"));
    assert_eq!(
        params_alias.resolved_current_head().as_deref(),
        Some("abc123")
    );

    let mut params_snake: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "key": "x",
        "caller_commit": "abc123"
    }))
    .expect("caller_commit alias parses");
    params_snake.normalize();
    assert_eq!(params_snake.current_head.as_deref(), Some("abc123"));
}

// -----------------------------------------------------------------------
// h1hn corpus-driven dispatch coverage.
//
// The graph-ops-level `trait_dispatch_corpus_e2e.rs` exercises the
// `RepoDependencyGraph` fixtures and the `code_graph` test-harness
// equivalents (`collect_context_buckets`, `shared::impact_bfs`). The
// tests below exercise the **agent dispatch boundary** itself:
// `call_code_graph_inner` against a hand-shaped `RepoGraphOps` stub
// whose `context` / `impact` returns are built from the corpus
// entries documented in
// `server/src/mcp_bridge/graph_ops/tests/trait_dispatch_corpus.rs`
// (`RuntimeOps::list_taskrun_jobs`, `RepoGraphOps::context`,
// `SlotPoolOps::get_status`, `RepoGraphOps::impact`).
//
// The fixtures mirror the corpus's hand-verified topology: a
// `TraitDispatchCall` caller edge at the 0.70 floor lands the
// production caller in the `Calls` bucket for both `context` and
// `impact`, and the high-confidence `Implements` edge surfaces the
// trait method in the impl's `outgoing.implements` bucket. This is
// the "agent side" of the contract: the dispatch must serialize the
// bridge response unchanged, so any regression that drops the
// `symbol_context` / `impact` array (or reshapes the bucket keys)
// would break these tests loudly.
// -----------------------------------------------------------------------

/// Build a `SymbolContext` payload for one corpus entry. The caller
/// entry's `name`/`uid`/`confidence` model the corpus's hand-verified
/// topology — `RuntimeOps::list_taskrun_jobs`'s caller
/// (`reap_orphaned_taskrun_jobs`) carries the synthesized
/// `TraitDispatchCall` confidence floor (0.70).
fn corpus_symbol_context_for_runtime_ops_list_taskrun_jobs()
-> djinn_control_plane::bridge::SymbolContext {
    use djinn_control_plane::bridge::{EdgeCategory, RelatedSymbol, SymbolContext, SymbolNode};
    use std::collections::BTreeMap;

    let symbol = SymbolNode {
        uid: "symbol:runtime_bridge.rs::list_taskrun_jobs".to_string(),
        name: "list_taskrun_jobs".to_string(),
        kind: "method".to_string(),
        file_path: "server/crates/djinn-control-plane/src/bridge/runtime_bridge.rs".to_string(),
        start_line: 137,
        end_line: 137,
        content: None,
        method_metadata: None,
        complexity: None,
    };
    let mut incoming: BTreeMap<EdgeCategory, Vec<RelatedSymbol>> = BTreeMap::new();
    let mut outgoing: BTreeMap<EdgeCategory, Vec<RelatedSymbol>> = BTreeMap::new();
    incoming.insert(
        EdgeCategory::Calls,
        vec![RelatedSymbol {
            uid: "symbol:health.rs::reap_orphaned_taskrun_jobs".to_string(),
            name: "reap_orphaned_taskrun_jobs".to_string(),
            kind: "function".to_string(),
            file_path: Some(
                "server/crates/djinn-agent/src/actors/coordinator/health.rs".to_string(),
            ),
            confidence: 0.70,
            confidence_tier: "inferred".to_string(),
            confidence_reason: Some("trait-dispatch-call".to_string()),
            excluded_reason: None,
            route_language_chain: None,
        }],
    );
    outgoing.insert(
        EdgeCategory::Implements,
        vec![RelatedSymbol {
            uid: "symbol:app_state.rs::list_taskrun_jobs".to_string(),
            name: "list_taskrun_jobs".to_string(),
            kind: "method".to_string(),
            file_path: Some("server/src/mcp_bridge/mod.rs".to_string()),
            confidence: 0.90,
            confidence_tier: "extracted".to_string(),
            confidence_reason: None,
            excluded_reason: None,
            route_language_chain: None,
        }],
    );
    SymbolContext {
        symbol,
        incoming,
        outgoing,
        processes: vec![],
    }
}

/// Stub `RepoGraphOps` whose `context` returns the corpus-shaped
/// payload above and whose `impact` returns the same caller in the
/// blast radius at the 0.70 confidence floor. All other methods
/// return "not used" — the agent dispatch routes them to other
/// handlers (not used in this test) and we never call them.
struct CorpusContextImpactStub {
    symbol_context: djinn_control_plane::bridge::SymbolContext,
    impact_entries: Vec<djinn_control_plane::bridge::ImpactEntry>,
}

fn runtime_ops_corpus_stub() -> (
    CorpusContextImpactStub,
    &'static str,
    &'static str,
    &'static str,
) {
    let symbol_context = corpus_symbol_context_for_runtime_ops_list_taskrun_jobs();
    let impact_entries = vec![djinn_control_plane::bridge::ImpactEntry {
        uid: "symbol:health.rs::reap_orphaned_taskrun_jobs".to_string(),
        key: "symbol:health.rs::reap_orphaned_taskrun_jobs".to_string(),
        depth: 1,
        file_path: Some("server/crates/djinn-agent/src/actors/coordinator/health.rs".to_string()),
        confidence_tier: Some("symbol".to_string()),
        exclusion_reason: None,
    }];
    (
        CorpusContextImpactStub {
            symbol_context,
            impact_entries,
        },
        "symbol:health.rs::reap_orphaned_taskrun_jobs",
        "reap_orphaned_taskrun_jobs",
        "symbol:runtime_bridge.rs::list_taskrun_jobs",
    )
}

#[async_trait::async_trait]
impl djinn_control_plane::bridge::RepoGraphOps for CorpusContextImpactStub {
    async fn context(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: bool,
    ) -> Result<Option<djinn_control_plane::bridge::SymbolContext>, String> {
        Ok(Some(self.symbol_context.clone()))
    }

    async fn impact(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: &str,
        _: usize,
        _: Option<&str>,
        _: Option<f64>,
    ) -> Result<djinn_control_plane::bridge::ImpactResult, String> {
        Ok(djinn_control_plane::bridge::ImpactResult::Detailed(
            self.impact_entries.clone(),
        ))
    }

    async fn neighbors(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<djinn_control_plane::bridge::NeighborsResult, String> {
        Err("not used".into())
    }

    async fn ranked(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::RankedNode>, String> {
        Err("not used".into())
    }
    async fn implementations(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
    ) -> Result<Vec<String>, String> {
        Err("not used".into())
    }
    async fn search(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::SearchHit>, String> {
        Err("not used".into())
    }
    async fn cycles(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CycleGroup>, String> {
        Err("not used".into())
    }
    async fn orphans(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::OrphanEntry>, String> {
        Err("not used".into())
    }
    async fn path(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: &str,
        _: &str,
        _: Option<usize>,
    ) -> Result<Option<djinn_control_plane::bridge::PathResult>, String> {
        Err("not used".into())
    }
    async fn edges(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::EdgeEntry>, String> {
        Err("not used".into())
    }
    async fn describe(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
    ) -> Result<Option<djinn_control_plane::bridge::SymbolDescription>, String> {
        Err("not used".into())
    }
    async fn status(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
    ) -> Result<djinn_control_plane::bridge::GraphStatus, String> {
        Err("not used".into())
    }
    async fn snapshot(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: djinn_control_plane::bridge::SnapshotLevel,
        _: usize,
        _: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    ) -> Result<djinn_control_plane::bridge::SnapshotPayload, String> {
        Err("not used".into())
    }
    async fn symbols_at(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: u32,
        _: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::SymbolAtHit>, String> {
        Err("not used".into())
    }
    async fn diff_touches(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &[djinn_control_plane::bridge::ChangedRange],
    ) -> Result<djinn_control_plane::bridge::DiffTouchesResult, String> {
        Err("not used".into())
    }
    async fn detect_changes(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: &[String],
    ) -> Result<djinn_control_plane::bridge::DetectedChangesResult, String> {
        Err("not used".into())
    }
    async fn api_surface(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::ApiSurfaceEntry>, String> {
        Err("not used".into())
    }
    async fn boundary_check(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &[djinn_control_plane::bridge::BoundaryRule],
        _: &str,
    ) -> Result<Vec<djinn_control_plane::bridge::BoundaryViolation>, String> {
        Err("not used".into())
    }
    async fn hotspots(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: u32,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::HotspotEntry>, String> {
        Err("not used".into())
    }
    async fn complexity(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<djinn_control_plane::bridge::ComplexityResult, String> {
        Err("not used".into())
    }
    async fn refactor_candidates(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<u32>,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::RefactorCandidate>, String> {
        Err("not used".into())
    }
    async fn metrics_at(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
    ) -> Result<djinn_control_plane::bridge::MetricsAtResult, String> {
        Err("not used".into())
    }
    async fn dead_symbols(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::DeadSymbolEntry>, String> {
        Err("not used".into())
    }
    async fn deprecated_callers(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::DeprecatedHit>, String> {
        Err("not used".into())
    }
    async fn touches_hot_path(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: Option<&str>,
        _: &[String],
        _: &[String],
        _: &[String],
    ) -> Result<Vec<djinn_control_plane::bridge::HotPathHit>, String> {
        Err("not used".into())
    }
    async fn coupling(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingEntry>, String> {
        Err("not used".into())
    }
    async fn churn(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::ChurnEntry>, String> {
        Err("not used".into())
    }
    async fn coupling_hotspots(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CoupledPairEntry>, String> {
        Err("not used".into())
    }
    async fn coupling_hubs(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: usize,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingHubEntry>, String> {
        Err("not used".into())
    }
    async fn resolve(
        &self,
        _: &djinn_control_plane::bridge::ProjectCtx,
        _: &str,
        _: Option<&str>,
    ) -> Result<djinn_control_plane::bridge::ResolveOutcome, String> {
        Err("not used".into())
    }
}

async fn dispatch_corpus_context_impact(
    mut params: CodeGraphParams,
    stub: CorpusContextImpactStub,
) -> serde_json::Value {
    params.normalize();
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: "project-1".to_string(),
        clone_path: "/repo".to_string(),
        workspace: None,
        sub_path: None,
    };
    call_code_graph_inner(&state, &mut params, &ctx, &stub)
        .await
        .expect("corpus-driven code_graph dispatch should serialize")
}

/// AC: `code_graph context` for `RuntimeOps::list_taskrun_jobs`
/// surfaces the production caller `reap_orphaned_taskrun_jobs` in
/// the incoming `Calls` bucket at the synthesized
/// `TraitDispatchCall` confidence floor (0.70). The dispatch must
/// preserve the `symbol_context` discriminator wrapper and the
/// bucketed `incoming`/`outgoing` shape unchanged.
#[tokio::test]
async fn code_graph_dispatch_corpus_runtime_ops_list_taskrun_jobs_context() {
    let (stub, caller_uid, caller_name, trait_uid) = runtime_ops_corpus_stub();
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "context",
        "key": trait_uid,
        "include_content": false
    }))
    .expect("context params parse");
    let value = dispatch_corpus_context_impact(params, stub).await;

    let payload = value
        .get("symbol_context")
        .and_then(|v| v.as_object())
        .expect("code_graph context dispatch must wrap response in symbol_context");
    assert_eq!(
        payload["symbol"]["uid"], trait_uid,
        "context dispatch must echo the queried symbol uid"
    );
    let incoming_calls = payload["incoming"]["calls"]
        .as_array()
        .expect("context incoming.calls must be an array");
    assert!(
        incoming_calls
            .iter()
            .any(|r| { r["uid"] == caller_uid && r["name"] == caller_name }),
        "context incoming.calls must include corpus caller {caller_name} (uid={caller_uid}); got {incoming_calls:?}"
    );
    let caller_entry = incoming_calls
        .iter()
        .find(|r| r["uid"] == caller_uid)
        .expect("caller present");
    assert!(
        (caller_entry["confidence"].as_f64().unwrap_or(0.0) - 0.70).abs() < f64::EPSILON,
        "corpus caller confidence {} must equal TraitDispatchCall floor 0.70",
        caller_entry["confidence"]
    );
    assert_eq!(
        caller_entry["confidence_tier"], "inferred",
        "TraitDispatchCall caller must classify as inferred confidence tier"
    );
}

/// AC: `code_graph context` for the *concrete impl* method
/// `AppState::list_taskrun_jobs` must surface the trait method in
/// its outgoing `Implements` bucket via the high-confidence
/// `Implements` relationship edge. The dispatch must preserve the
/// bucket keys (`outgoing.implements`) unchanged so the
/// `/code-graph` UI parser keeps working.
#[tokio::test]
async fn code_graph_dispatch_corpus_runtime_ops_list_taskrun_jobs_impl_method_implements_bucket() {
    let (stub, _caller_uid, _caller_name, _trait_uid) = runtime_ops_corpus_stub();
    // The stub's `context` returns the same SymbolContext regardless
    // of `key`; here we exercise the dispatch boundary by querying
    // with the impl method's uid and asserting the bucket shape.
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "context",
        "key": "symbol:app_state.rs::list_taskrun_jobs",
        "include_content": false
    }))
    .expect("context params parse");
    let value = dispatch_corpus_context_impact(params, stub).await;

    let payload = value
        .get("symbol_context")
        .and_then(|v| v.as_object())
        .expect("symbol_context wrapper must be present");
    let outgoing_implements = payload["outgoing"]["implements"]
        .as_array()
        .expect("context outgoing.implements must be an array");
    assert!(
        !outgoing_implements.is_empty(),
        "outgoing.implements must carry the trait method for the impl_method context; got {outgoing_implements:?}"
    );
    let trait_hop = outgoing_implements
        .iter()
        .find(|r| r["uid"] == "symbol:app_state.rs::list_taskrun_jobs")
        .expect("trait method hop must be present");
    assert!(
        (trait_hop["confidence"].as_f64().unwrap_or(0.0) - 0.90).abs() < f64::EPSILON,
        "Implements hop confidence {} must equal Implements floor 0.90",
        trait_hop["confidence"]
    );
    assert_eq!(
        trait_hop["confidence_tier"], "extracted",
        "Implements hop must classify as extracted confidence tier"
    );
}

/// AC: `code_graph impact` for `RuntimeOps::list_taskrun_jobs` must
/// include the production caller `reap_orphaned_taskrun_jobs` in the
/// blast radius at the synthesized `TraitDispatchCall` confidence
/// floor. The dispatch must preserve the `key` / `impact` array
/// shape unchanged.
#[tokio::test]
async fn code_graph_dispatch_corpus_runtime_ops_list_taskrun_jobs_impact() {
    let (stub, caller_uid, caller_name, trait_uid) = runtime_ops_corpus_stub();
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "impact",
        "key": trait_uid,
        "limit": 3,
        "min_confidence": 0.70
    }))
    .expect("impact params parse");
    let value = dispatch_corpus_context_impact(params, stub).await;

    assert_eq!(
        value["key"], trait_uid,
        "impact dispatch must echo the queried key"
    );
    let impact = value["impact"]
        .as_array()
        .expect("impact dispatch must include a non-empty impact array");
    assert!(
        impact
            .iter()
            .any(|e| e["key"] == caller_uid || e["uid"] == caller_uid),
        "impact must include corpus caller {caller_name} (uid={caller_uid}); got {impact:?}"
    );
    let caller_entry = impact
        .iter()
        .find(|e| e["key"] == caller_uid || e["uid"] == caller_uid)
        .expect("caller in blast radius");
    assert_eq!(
        caller_entry["depth"], 1,
        "caller must be reached at depth 1 (direct trait-dispatch caller)"
    );
}

/// AC: `code_graph impact` honors an explicit `min_confidence` lower
/// than the default 0.85 to include the synthesized trait-dispatch
/// caller. The dispatch must forward `min_confidence` to the bridge
/// unchanged so callers can opt into the inferred edge tier.
#[tokio::test]
async fn code_graph_dispatch_corpus_runtime_ops_list_taskrun_jobs_impact_min_confidence_floor() {
    let (stub, caller_uid, _caller_name, trait_uid) = runtime_ops_corpus_stub();
    // Pass `min_confidence=0.0` to opt into the full edge set per
    // the AC: "Include `min_confidence` coverage only as needed to
    // validate end-to-end behavior not already covered by `ggrm`
    // unit fixtures." The dispatch boundary is what we're testing
    // here, not the BFS — that's covered by
    // `trait_dispatch_impact.rs`.
    let params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "impact",
        "key": trait_uid,
        "limit": 3,
        "min_confidence": 0.0
    }))
    .expect("impact params parse");
    let value = dispatch_corpus_context_impact(params, stub).await;

    let impact = value["impact"]
        .as_array()
        .expect("impact dispatch must include an impact array");
    assert!(
        impact
            .iter()
            .any(|e| e["key"] == caller_uid || e["uid"] == caller_uid),
        "min_confidence=0.0 must include the trait-dispatch caller '{caller_uid}'; got {impact:?}"
    );
}
