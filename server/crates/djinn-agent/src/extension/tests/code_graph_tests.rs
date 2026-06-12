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
    )
    .await
}

#[test]
fn code_graph_params_normalize_uid_triplet_and_traversal_pagination_fields() {
    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "impact",
        "project": "owner/repo",
        "uid": "node:src/lib.rs#AuthService",
        "name": "AuthService",
        "file_path": "src/lib.rs",
        "kind": "struct",
        "limit": 3,
        "offset": 40,
        "summaryOnly": true,
        "byDepthCounts": true,
        "pageLimit": 25
    }))
    .expect("extension code_graph params should accept MCP/chat resolver and pagination fields");

    params.normalize();
    params.normalize_resolver_inputs();

    assert_eq!(
        params.key.as_deref(),
        Some("node:src/lib.rs#AuthService"),
        "uid must normalize into key so chat/extension and MCP share exact follow-up resolution"
    );
    assert_eq!(params.uid.as_deref(), Some("node:src/lib.rs#AuthService"));
    assert_eq!(params.name.as_deref(), Some("AuthService"));
    assert_eq!(params.file_path.as_deref(), Some("src/lib.rs"));
    assert_eq!(params.kind.as_deref(), Some("struct"));
    assert_eq!(
        params.kind_hint.as_deref(),
        Some("struct"),
        "kind must feed the existing bridge kind_hint score path"
    );
    assert_eq!(params.limit, Some(3));
    assert_eq!(params.offset, Some(40));
    assert_eq!(params.summary_only, Some(true));
    assert_eq!(params.by_depth_counts, Some(true));
    assert_eq!(params.page_limit, Some(25));
}

#[test]
fn code_graph_params_normalize_name_file_kind_when_uid_is_unavailable() {
    let mut params: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "project": "owner/repo",
        "key": "",
        "uid": "",
        "name": "  ",
        "file_path": "src/auth.rs",
        "kind": "function"
    }))
    .expect("extension code_graph params should deserialize name/file_path/kind resolver triplet");

    // Empty strings are normalized only when exactly empty; trim-like handling
    // belongs to the graph resolver. Verify the truly-empty uid/key fields do
    // not mask a non-empty triplet supplied by chat/MCP clients.
    params.normalize();
    params.name = Some("login".to_string());
    params.normalize_resolver_inputs();

    assert_eq!(
        params.key.as_deref(),
        Some("login"),
        "name must become the bridge key when no uid/key exact identity was supplied"
    );
    assert_eq!(params.file_path.as_deref(), Some("src/auth.rs"));
    assert_eq!(params.kind_hint.as_deref(), Some("function"));
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
fn code_graph_params_normalize_uid_triplet_and_traversal_controls() {
    let mut by_uid: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "neighbors",
        "uid": "symbol:scip-rust pkg src/auth.rs `login`().",
        "key": "stale-display-name",
        "name": "login",
        "file_path": "src/auth.rs",
        "kind": "function",
        "offset": 25,
        "summaryOnly": true,
        "byDepthCounts": true,
        "pageLimit": 50,
    }))
    .expect("uid params parse");
    by_uid.normalize();
    by_uid.normalize_resolver_inputs();
    assert_eq!(
        by_uid.key.as_deref(),
        Some("symbol:scip-rust pkg src/auth.rs `login`()."),
        "stable uid must win over legacy key/name resolver input"
    );
    assert_eq!(by_uid.kind_hint.as_deref(), Some("function"));
    assert_eq!(by_uid.file_path.as_deref(), Some("src/auth.rs"));
    assert_eq!(by_uid.offset, Some(25));
    assert_eq!(by_uid.summary_only, Some(true));
    assert_eq!(by_uid.by_depth_counts, Some(true));
    assert_eq!(by_uid.page_limit, Some(50));

    let mut by_triplet: CodeGraphParams = serde_json::from_value(serde_json::json!({
        "operation": "impact",
        "uid": "",
        "key": "",
        "name": "UserService",
        "file_path": "src/user/service.rs",
        "kind": "class",
        "summary_only": false,
        "by_depth_counts": true,
    }))
    .expect("triplet params parse");
    by_triplet.normalize();
    by_triplet.normalize_resolver_inputs();
    assert_eq!(by_triplet.key.as_deref(), Some("UserService"));
    assert_eq!(by_triplet.kind_hint.as_deref(), Some("class"));
    assert_eq!(by_triplet.file_path.as_deref(), Some("src/user/service.rs"));
    assert_eq!(by_triplet.summary_only, Some(false));
    assert_eq!(by_triplet.by_depth_counts, Some(true));
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
