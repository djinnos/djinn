use super::*;

impl RepoGraphBridge {
    pub(super) async fn workspaces(&self, ctx: &ProjectCtx) -> Result<WorkspacesResult, String> {
        let mut counts = match djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await
        {
            Ok(graph) => shared::graph_workspace_node_counts(&graph),
            Err(err) => {
                tracing::debug!(
                    project_id = %ctx.id,
                    error = %err,
                    "code_graph workspaces: graph unavailable; returning freshness-only workspaces"
                );
                std::collections::BTreeMap::new()
            }
        };
        let freshness = djinn_db::ProjectWorkspaceGraphRepository::new(self.state.db().clone())
            .list_for_project(&ctx.id)
            .await
            .map_err(|err| err.to_string())?;

        let mut freshness_by_slug = std::collections::BTreeMap::new();
        for row in freshness {
            // The code-less sentinel is a freshness marker ("nothing to warm
            // = considered warmed"), not a workspace — listing it would put a
            // `__djinn_no_code__` entry in the UI workspace picker.
            if row.workspace_slug == djinn_db::CODELESS_WORKSPACE_SLUG {
                continue;
            }
            let slug = shared::normalize_workspace_slug(Some(&row.workspace_slug))
                .unwrap_or(row.workspace_slug.clone());
            counts.entry(slug.clone()).or_insert(0);
            freshness_by_slug.insert(slug, row);
        }

        let workspaces = counts
            .into_iter()
            .map(|(slug, node_count)| {
                let freshness = freshness_by_slug.remove(&slug);
                GraphWorkspaceEntry {
                    name: slug.clone(),
                    slug,
                    node_count,
                    commit_sha: freshness.as_ref().map(|row| row.commit_sha.clone()),
                    warmed_at: freshness.as_ref().map(|row| row.warmed_at.clone()),
                    status: freshness.as_ref().map(|row| row.status.clone()),
                }
            })
            .collect();

        Ok(WorkspacesResult {
            project_id: ctx.id.clone(),
            workspaces,
        })
    }

    pub(super) async fn workspace_node_counts(
        &self,
        ctx: &ProjectCtx,
    ) -> Result<HashMap<String, usize>, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        Ok(shared::graph_workspace_node_counts(&graph)
            .into_iter()
            .collect())
    }

    pub(super) async fn workspace_hint(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
    ) -> Result<Option<Vec<String>>, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        Ok(shared::workspace_hint_from_graph(&graph, workspace))
    }

    pub(super) async fn neighbors(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        direction: Option<&str>,
        group_by: Option<&str>,
        kind_filter: Option<&str>,
    ) -> Result<NeighborsResult, String> {
        use djinn_graph::repo_graph::RepoGraphEdgeKind;
        use petgraph::Direction;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        // v8: apply project graph_excluded_paths to the neighbor set so
        // SCIP module-tree synthetic nodes (`crate/`, `…/MODULE.`) and
        // user-configured globs don't leak into dependents-discovery
        // queries — same as ranked / search / cycles / impact / dead.
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let node_index = resolve_node_or_err(&graph, key)?;
        // v8: pre-compute the queried node's identity so we can filter
        // out self-referential neighbors. User feedback: querying a
        // file's outgoing neighbors returns the file itself (because
        // the file's own symbols reach back via DeclaredInFile/
        // FileReference) — same file path as the source, no useful
        // signal. Same for a symbol whose declaring file shows up.
        let self_node = graph.node(node_index);
        let self_key = format_node_key(&self_node.id);
        let self_file = self_node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string());
        let directions: Vec<Direction> = match direction {
            Some("incoming") => vec![Direction::Incoming],
            Some("outgoing") => vec![Direction::Outgoing],
            _ => vec![Direction::Incoming, Direction::Outgoing],
        };

        // PR A3: when the caller asks for `kind_filter=reads|writes`,
        // restrict the BFS frontier to that edge kind. Validation happens
        // upstream (`validate_edge_kind_filter`); anything else here is a
        // bug and we treat it as "no filter" rather than panic.
        let edge_kind_filter: Option<RepoGraphEdgeKind> = match kind_filter {
            Some("reads") => Some(RepoGraphEdgeKind::Reads),
            Some("writes") => Some(RepoGraphEdgeKind::Writes),
            _ => None,
        };

        let mut neighbors = Vec::new();

        // Proposal qoxm: explicit opt-in to the commit co-change sidecar via
        // `kind_filter=co_changed_with`. Co-change edges live OUTSIDE the
        // petgraph (see `djinn_graph::cochange`), so the default walk below
        // never emits them — this branch is the only way they enter a
        // `neighbors` response, keeping the default-exclude contract intact.
        // The relationship is undirected (stored once, `file_a < file_b`), so
        // the caller's `direction` filter is ignored here and each partner is
        // labeled "undirected"; the coupling score rides in `edge_weight` and
        // the kind string matches the `edges`/`snapshot` channel.
        if kind_filter == Some("co_changed_with") {
            for cc in graph.cochange_edges() {
                let other_index = if cc.source == node_index {
                    cc.target
                } else if cc.target == node_index {
                    cc.source
                } else {
                    continue;
                };
                let other_node = graph.node(other_index);
                let other_key = format_node_key(&other_node.id);
                let other_file = other_node
                    .file_path
                    .as_ref()
                    .map(|p| p.display().to_string());
                if exclusions.excludes(&other_key, other_file.as_deref(), &other_node.display_name)
                {
                    continue;
                }
                neighbors.push((
                    other_node,
                    GraphNeighbor {
                        uid: other_key.clone(),
                        key: other_key,
                        kind: format!("{:?}", other_node.kind).to_lowercase(),
                        display_name: other_node.display_name.clone(),
                        edge_kind: format!("{:?}", RepoGraphEdgeKind::CoChangedWith),
                        edge_weight: cc.confidence,
                        direction: "undirected".to_string(),
                    },
                ));
            }
            return match group_by {
                None => Ok(NeighborsResult::Detailed(
                    neighbors.into_iter().map(|(_, n)| n).collect(),
                )),
                Some("file") => Ok(NeighborsResult::Grouped(group_neighbors_by_file(
                    &neighbors,
                ))),
                Some(other) => Err(format!(
                    "invalid group_by '{other}': only 'file' is supported"
                )),
            };
        }
        for dir in directions {
            let dir_label = match dir {
                Direction::Incoming => "incoming",
                Direction::Outgoing => "outgoing",
            };
            for edge in graph.graph().edges_directed(node_index, dir) {
                if let Some(filter) = edge_kind_filter
                    && edge.weight().kind != filter
                {
                    continue;
                }
                let other_index = match dir {
                    Direction::Outgoing => edge.target(),
                    Direction::Incoming => edge.source(),
                };
                let other_node = graph.node(other_index);
                // v8: skip external (vendored / third-party / cross-crate)
                // neighbors. `neighbors` is "what's connected to this in
                // MY codebase"; an imported `tokio::spawn` showing up
                // among callers is noise.
                if other_node.is_external {
                    continue;
                }
                let other_key = format_node_key(&other_node.id);
                let other_file = other_node
                    .file_path
                    .as_ref()
                    .map(|p| p.display().to_string());
                if exclusions.excludes(&other_key, other_file.as_deref(), &other_node.display_name)
                {
                    continue;
                }
                // v8: drop self-references. Two flavours:
                //   1. Other node IS the queried node (rare — would
                //      require a self-loop edge).
                //   2. Other node lives in the SAME file as the queried
                //      node (very common: querying file:foo.rs returns
                //      its own symbols via FileReference; querying a
                //      symbol returns its declaring file via
                //      DeclaredInFile, which is the same file the
                //      symbol lives in).
                if other_index == node_index || other_key == self_key {
                    continue;
                }
                if let (Some(sf), Some(of)) = (self_file.as_deref(), other_file.as_deref())
                    && sf == of
                {
                    continue;
                }
                neighbors.push((
                    other_node,
                    GraphNeighbor {
                        uid: other_key.clone(),
                        key: other_key,
                        kind: format!("{:?}", other_node.kind).to_lowercase(),
                        display_name: other_node.display_name.clone(),
                        edge_kind: format!("{:?}", edge.weight().kind),
                        edge_weight: edge.weight().weight,
                        direction: dir_label.to_string(),
                    },
                ));
            }
        }

        match group_by {
            None => Ok(NeighborsResult::Detailed(
                neighbors.into_iter().map(|(_, n)| n).collect(),
            )),
            Some("file") => {
                let groups = group_neighbors_by_file(&neighbors);
                Ok(NeighborsResult::Grouped(groups))
            }
            Some(other) => Err(format!(
                "invalid group_by '{other}': only 'file' is supported"
            )),
        }
    }

    pub(super) async fn query_subgraph(
        &self,
        ctx: &ProjectCtx,
        req: QuerySubgraphRequest,
    ) -> Result<WireQuerySubgraphResult, String> {
        use djinn_graph::query_subgraph::{QuerySubgraphParams, SeedSource};
        use djinn_graph::repo_graph::{RepoGraphEdgeKind, RepoGraphNodeKind};

        fn node_kind(label: Option<&str>) -> Result<Option<RepoGraphNodeKind>, String> {
            match label.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
                None | Some("") => Ok(None),
                Some("file") => Ok(Some(RepoGraphNodeKind::File)),
                Some("symbol") => Ok(Some(RepoGraphNodeKind::Symbol)),
                Some("process") => Ok(Some(RepoGraphNodeKind::Process)),
                Some("table") => Ok(Some(RepoGraphNodeKind::Table)),
                Some("route") => Ok(Some(RepoGraphNodeKind::Route)),
                Some(other) => Err(format!("invalid kind_filter '{other}' for query_subgraph")),
            }
        }
        fn edge_kind(label: &str) -> Result<RepoGraphEdgeKind, String> {
            match label.trim().to_ascii_lowercase().as_str() {
                "contains_definition" | "containsdefinition" => {
                    Ok(RepoGraphEdgeKind::ContainsDefinition)
                }
                "declared_in_file" | "declaredinfile" => Ok(RepoGraphEdgeKind::DeclaredInFile),
                "file_reference" | "filereference" | "imports" | "import" => {
                    Ok(RepoGraphEdgeKind::FileReference)
                }
                "symbol_reference" | "symbolreference" | "calls" | "call" | "references" => {
                    Ok(RepoGraphEdgeKind::SymbolReference)
                }
                "handles_route" | "handlesroute" => Ok(RepoGraphEdgeKind::HandlesRoute),
                "reads" | "read" => Ok(RepoGraphEdgeKind::Reads),
                "route" | "routes" => Ok(RepoGraphEdgeKind::Route),
                "fetches" | "fetch" => Ok(RepoGraphEdgeKind::Fetches),
                "writes" | "write" => Ok(RepoGraphEdgeKind::Writes),
                "extends" | "extend" => Ok(RepoGraphEdgeKind::Extends),
                "implements" | "implement" => Ok(RepoGraphEdgeKind::Implements),
                "type_defines" | "typedefines" | "returns" | "return" => {
                    Ok(RepoGraphEdgeKind::TypeDefines)
                }
                "defines" | "define" => Ok(RepoGraphEdgeKind::Defines),
                "entry_point_of" | "entrypointof" => Ok(RepoGraphEdgeKind::EntryPointOf),
                "member_of" | "memberof" => Ok(RepoGraphEdgeKind::MemberOf),
                "step_in_process" | "stepinprocess" => Ok(RepoGraphEdgeKind::StepInProcess),
                // PR t16t: synthesized trait-dispatch caller edge.
                "trait_dispatch_call" | "traitdispatchcall" => {
                    Ok(RepoGraphEdgeKind::TraitDispatchCall)
                }
                other => Err(format!("invalid edge kind '{other}' for query_subgraph")),
            }
        }
        fn edge_label(kind: RepoGraphEdgeKind) -> String {
            format!("{:?}", kind).to_ascii_lowercase()
        }
        fn node_label(kind: RepoGraphNodeKind) -> String {
            format!("{:?}", kind).to_ascii_lowercase()
        }
        fn seed_label(source: SeedSource) -> String {
            format!("{:?}", source).to_ascii_lowercase()
        }

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let params = QuerySubgraphParams {
            query: req.query,
            workspace: req.workspace,
            context_filter: req.context_filter,
            file_filter: req.file_filter,
            kind_filter: node_kind(req.kind_filter.as_deref())?,
            edge_filter: req
                .edge_filter
                .iter()
                .map(|kind| edge_kind(kind))
                .collect::<Result<Vec<_>, _>>()?,
            token_budget: req.token_budget,
            max_depth: req.max_depth,
            max_seeds: req.max_seeds,
            min_hub_degree: None,
        };
        let result = graph.query_subgraph(params, None);
        Ok(WireQuerySubgraphResult {
            query: result.query,
            nodes: result
                .nodes
                .into_iter()
                .map(|node| WireQuerySubgraphNode {
                    uid: node.uid,
                    kind: node_label(node.kind),
                    display_name: node.display_name,
                    file_path: node.file_path,
                    workspace: node.workspace,
                    is_seed: node.is_seed,
                    is_hub: node.is_hub,
                    degree: node.degree,
                })
                .collect(),
            edges: result
                .edges
                .into_iter()
                .map(|edge| WireQuerySubgraphEdge {
                    from_uid: edge.from_uid,
                    to_uid: edge.to_uid,
                    kind: edge_label(edge.kind),
                    confidence: edge.confidence,
                    confidence_tier: format!("{:?}", edge.confidence_tier).to_ascii_lowercase(),
                    reason: edge.reason,
                })
                .collect(),
            seeds: result
                .seeds
                .into_iter()
                .map(|seed| WireQuerySubgraphSeedDebug {
                    uid: seed.uid,
                    display_name: seed.display_name,
                    score: seed.score,
                    source: seed_label(seed.source),
                    matched_text: seed.matched_text,
                    debug: seed.debug,
                })
                .collect(),
            inferred_edge_kinds: result
                .inferred_edge_kinds
                .into_iter()
                .map(edge_label)
                .collect(),
            budget: WireQuerySubgraphBudget {
                requested_tokens: result.budget.requested_tokens,
                estimated_tokens: result.budget.estimated_tokens,
                truncated: result.budget.truncated,
                omitted_nodes: result.budget.omitted_nodes,
                omitted_edges: result.budget.omitted_edges,
            },
            traversal: WireQuerySubgraphTraversalDebug {
                max_depth: result.traversal.max_depth,
                hub_degree_threshold: result.traversal.hub_degree_threshold,
                hubs_blocked: result.traversal.hubs_blocked,
                skipped_edge_kinds: result
                    .traversal
                    .skipped_edge_kinds
                    .into_iter()
                    .map(edge_label)
                    .collect(),
            },
            narrowing_hints: result.narrowing_hints,
        })
    }

    pub(super) async fn ranked(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        kind_filter: Option<&str>,
        sort_by: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RankedNode>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        // Read the cached PageRank populated by `ensure_canonical_graph`
        // during warm.  Without this cache, every `ranked` call re-ran a full
        // PageRank pass and hung for 30+ s on real-world graphs even when
        // `code_graph status` reported `warmed: true`.
        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let workspace_prefix = shared::active_workspace_prefix(&graph, workspace);
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        let mut nodes: Vec<RankedNode> = ranking
            .nodes
            .iter()
            .filter(|node| filter.is_none() || Some(node.kind) == filter)
            .filter_map(|node| {
                let graph_node = graph.node(node.node_index);
                // v8: skip external (vendored / third-party / cross-crate)
                // symbols. `ranked` is "what's central in MY codebase"; an
                // imported `tokio::spawn` getting top-3 is noise. Mirrors
                // the long-standing filter in `orphans` and `dead`.
                if graph_node.is_external {
                    return None;
                }
                if graph_node.is_route_or_tool() {
                    return None;
                }
                if let Some(prefix) = workspace_prefix.as_deref()
                    && !shared::repo_graph_node_matches_workspace(graph_node, prefix)
                {
                    return None;
                }
                let key = format_node_key(&node.key);
                let file_hint = shared::repo_graph_node_file_path(graph_node);
                // PR F4: apply graph exclusions BEFORE the limit truncate
                // so the user gets `limit` non-excluded results, not
                // `limit` raw results minus exclusions.
                if exclusions.excludes(&key, file_hint.as_deref(), &graph_node.display_name) {
                    return None;
                }
                // v8: drop test files from `ranked` centrality output.
                // User feedback: tests with high out-degree (test
                // files reference many production symbols) dominated
                // out_degree-sorted rankings without being
                // "architecturally meaningful". Conservative: only
                // skips file paths that match the per-language test
                // convention (`is_test_path`); test SYMBOLS in a
                // production file pass through. Tests that ARE in
                // a `tests/` directory or `*_test.go`-named file
                // also drop their symbol nodes (the file path on the
                // symbol matches).
                if let Some(path) = file_hint.as_deref()
                    && djinn_control_plane::tools::graph_exclusions::is_test_path(path)
                {
                    return None;
                }
                // PR F4: pick the lowest-ordinal step's process when the
                // node belongs to multiple — that's the "most upstream"
                // membership, which makes the bucket label the entry
                // point closest to this node.
                let process_id = shared::pick_lowest_ordinal_process_id(&graph, node.node_index);
                let community_id = graph.community_id(node.node_index).map(|s| s.to_string());
                Some(RankedNode {
                    uid: key.clone(),
                    key,
                    kind: format!("{:?}", node.kind).to_lowercase(),
                    display_name: graph_node.display_name.clone(),
                    score: node.score,
                    page_rank: node.page_rank,
                    structural_weight: node.structural_weight,
                    inbound_edge_weight: node.inbound_edge_weight,
                    outbound_edge_weight: node.outbound_edge_weight,
                    process_id,
                    community_id,
                    is_entry_point: node.is_entry_point,
                    entry_point_distance: node.entry_point_distance,
                })
            })
            .collect();

        match sort_by {
            None | Some("fused") => {
                // PR F4: already in fused (RRF) order — the canonical
                // ranking sorts by `fused_rank` desc.
            }
            Some("pagerank") => {
                nodes.sort_by(|a, b| b.page_rank.total_cmp(&a.page_rank));
            }
            Some("in_degree") => {
                nodes.sort_by(|a, b| b.inbound_edge_weight.total_cmp(&a.inbound_edge_weight));
            }
            Some("out_degree") => {
                nodes.sort_by(|a, b| b.outbound_edge_weight.total_cmp(&a.outbound_edge_weight));
            }
            Some("total_degree") => {
                nodes.sort_by(|a, b| {
                    let total_b = b.inbound_edge_weight + b.outbound_edge_weight;
                    let total_a = a.inbound_edge_weight + a.outbound_edge_weight;
                    total_b.total_cmp(&total_a)
                });
            }
            Some(other) => {
                return Err(format!(
                    "invalid sort_by '{other}': expected 'fused', 'pagerank', 'in_degree', \
                     'out_degree', or 'total_degree'"
                ));
            }
        }

        nodes.truncate(limit);
        Ok(nodes)
    }

    pub(super) async fn implementations(
        &self,
        ctx: &ProjectCtx,
        symbol: &str,
    ) -> Result<Vec<String>, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        // v8: exclude vendored impl files via graph_excluded_paths.
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        // Route through shared resolver to accept canonical `symbol:<scip>`-prefixed keys.
        let node_index = resolve_node_or_err(&graph, symbol)?;
        Ok(collect_implementations(&graph, node_index, &exclusions))
    }

    pub(super) async fn impact(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        key: &str,
        max_depth: usize,
        group_by: Option<&str>,
        min_confidence: Option<f64>,
    ) -> Result<ImpactResult, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        // v8: thread project graph_excluded_paths through impact too —
        // even with the behavioral-edge whitelist, the BFS frontier
        // can land on nodes the user has explicitly excluded
        // (vendored mirrors, generated dirs).
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let start = shared::resolve_node_or_err_for_workspace_seed(&graph, key, workspace)?;
        // PR s6ch / 92z7: when `DJINN_ROUTE_PARITY` is enabled, run
        // the policy-aware BFS so inferred `Fetches` consumer edges
        // below the confidence floor (or pointing at a health /
        // param-only path) are downgraded to suggestions instead of
        // hard blast-radius links. With the flag off, fall back to
        // the pre-92z7 `impact_bfs` so the shadow path can compare
        // the unfiltered set without parity-related churn.
        let policy = djinn_graph::route_extraction::route_parity_enabled()
            .then(|| graph.route_exclusion_config());
        let raw = shared::impact_bfs_with_policy(&graph, start, max_depth, min_confidence, policy);
        let result: Vec<_> = raw
            .into_iter()
            .filter(|(idx, _)| {
                let node = graph.node(*idx);
                // v8: skip external (vendored / third-party / cross-crate)
                // dependents — "what breaks if I change this" should be
                // about MY code, not someone else's.
                if node.is_external {
                    return false;
                }
                let key = format_node_key(&node.id);
                let file_hint = node.file_path.as_ref().map(|p| p.display().to_string());
                !exclusions.excludes(&key, file_hint.as_deref(), &node.display_name)
            })
            .collect();

        match group_by {
            None => Ok(ImpactResult::Detailed(
                result.into_iter().map(|(_, e)| e).collect(),
            )),
            Some("file") => {
                let groups = group_impact_by_file(&graph, &result);
                Ok(ImpactResult::Grouped(groups))
            }
            Some(other) => Err(format!(
                "invalid group_by '{other}': only 'file' is supported"
            )),
        }
    }

    pub(super) async fn search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        // PR F4: over-fetch so exclusions filter runs before capping to `limit`.
        let hits = graph.search_by_name(query, filter, usize::MAX);
        let mut out: Vec<SearchHit> = Vec::new();
        for hit in hits {
            let node = graph.node(hit.node_index);
            let key = format_node_key(&node.id);
            let file = node.file_path.as_ref().map(|p| p.display().to_string());
            if exclusions.excludes(&key, file.as_deref(), &node.display_name) {
                continue;
            }
            // v8: skip test-file results to avoid mock-dominated rankings.
            if let Some(path) = file.as_deref()
                && djinn_control_plane::tools::graph_exclusions::is_test_path(path)
            {
                continue;
            }
            out.push(SearchHit {
                uid: key.clone(),
                key,
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                score: hit.score,
                file,
                match_kind: None,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub(super) async fn hybrid_search(
        &self,
        ctx: &ProjectCtx,
        query: &str,
        kind_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, String> {
        // PR B4: cache-first orchestrator entrypoint. The actual three-
        // signal RRF fusion lives in `hybrid_search::run` so the
        // RepoGraphBridge stays thin.
        super::super::hybrid_search::run(&self.state, ctx, query, kind_filter, limit).await
    }

    pub(super) async fn cycles(
        &self,
        ctx: &ProjectCtx,
        kind_filter: Option<&str>,
        min_size: usize,
    ) -> Result<Vec<CycleGroup>, String> {
        // v8: use precomputed per-kind SCC cache; `min_size` applied at read time (materialised at 2).
        let (graph, _ranking, sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let cached: &Vec<Vec<petgraph::graph::NodeIndex>> = match kind_filter {
            Some("file") => &sccs.file,
            Some("symbol") => &sccs.symbol,
            _ => &sccs.full,
        };
        let min = min_size.max(2);
        Ok(cached
            .iter()
            .filter(|component| component.len() >= min)
            .map(|component| {
                let members = component
                    .iter()
                    .map(|idx| {
                        let node = graph.node(*idx);
                        CycleMember {
                            key: format_node_key(&node.id),
                            uid: format_node_key(&node.id),
                            display_name: node.display_name.clone(),
                            kind: format!("{:?}", node.kind).to_lowercase(),
                        }
                    })
                    .collect::<Vec<_>>();
                CycleGroup {
                    size: component.len(),
                    members,
                }
            })
            .collect())
    }

    pub(super) async fn orphans(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        kind_filter: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OrphanEntry>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        use djinn_graph::scip_parser::ScipVisibility;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let workspace_prefix = shared::active_workspace_prefix(&graph, workspace);
        let filter = match kind_filter {
            Some("file") => Some(RepoGraphNodeKind::File),
            Some("symbol") => Some(RepoGraphNodeKind::Symbol),
            _ => None,
        };
        let vis = match visibility {
            Some("public") => Some(ScipVisibility::Public),
            Some("private") => Some(ScipVisibility::Private),
            None | Some("any") => None,
            Some(other) => {
                return Err(format!(
                    "invalid visibility '{other}': expected 'public', 'private', or 'any'"
                ));
            }
        };
        // v8: over-fetch from the graph layer so we can post-filter
        // entry-points / tests / framework hooks without under-filling
        // `limit`. Cheap — graph.orphans is O(V) anyway.
        let raw_nodes = graph.orphans(filter, vis, limit.saturating_mul(4).clamp(limit, 1000));
        // Pre-collect EntryPointOf incoming-edge targets so we can
        // skip framework-invoked entry points without re-walking the
        // graph for each candidate.
        use djinn_graph::repo_graph::RepoGraphEdgeKind;
        let mut entry_set: std::collections::HashSet<petgraph::graph::NodeIndex> =
            std::collections::HashSet::new();
        for idx in graph.graph().node_indices() {
            if graph
                .graph()
                .edges_directed(idx, petgraph::Direction::Incoming)
                .any(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf)
            {
                entry_set.insert(idx);
            }
        }
        let mut out: Vec<OrphanEntry> = Vec::new();
        for idx in raw_nodes {
            let node = graph.node(idx);
            if let Some(prefix) = workspace_prefix.as_deref()
                && !shared::repo_graph_node_matches_workspace(node, prefix)
            {
                continue;
            }
            // v8: framework-invoked entry points are not dead code.
            // The detector covers `fn main`, route handlers, tests,
            // python `__main__`, etc. via EntryPointOf edges; SCIP-
            // marked tests via is_test. Defensive name check for
            // Go's `init()` (often missed by detectors because
            // every Go file may have one).
            if entry_set.contains(&idx) || node.is_test {
                continue;
            }
            if matches!(
                node.display_name.as_str(),
                "main" | "init" | "_start" | "TestMain"
            ) {
                continue;
            }
            // v8: also skip test files (file-path heuristic) — they
            // legitimately have no incoming production references but
            // aren't "dead". Symbols inside a test file flagged
            // is_test handle the symbol case; this catches FILE nodes.
            if let Some(path) = shared::repo_graph_node_file_path(node).as_deref()
                && djinn_control_plane::tools::graph_exclusions::is_test_path(path)
            {
                continue;
            }
            out.push(OrphanEntry {
                key: format_node_key(&node.id),
                uid: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                file: shared::repo_graph_node_file_path(node),
                visibility: node
                    .visibility
                    .map(|v| v.as_str().to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub(super) async fn describe(
        &self,
        ctx: &ProjectCtx,
        key: &str,
    ) -> Result<Option<SymbolDescription>, String> {
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let node_index = match resolve_node_or_err(&graph, key) {
            Ok(idx) => idx,
            Err(_) => return Ok(None),
        };
        let node = graph.node(node_index);
        let documentation = if node.documentation.is_empty() {
            None
        } else {
            Some(node.documentation.join("\n"))
        };
        // v8: behavioural fan-in / fan-out (skip structural anchors +
        // synthetic side-channels, same edge-kind partition the impact
        // BFS uses).
        use djinn_graph::repo_graph::RepoGraphEdgeKind;
        let is_behavioral = |kind: RepoGraphEdgeKind| -> bool {
            matches!(
                kind,
                RepoGraphEdgeKind::Reads
                    | RepoGraphEdgeKind::Writes
                    | RepoGraphEdgeKind::SymbolReference
                    | RepoGraphEdgeKind::FileReference
                    | RepoGraphEdgeKind::Implements
                    | RepoGraphEdgeKind::Extends
                    | RepoGraphEdgeKind::TypeDefines
                    | RepoGraphEdgeKind::Defines
                    | RepoGraphEdgeKind::Route
                    | RepoGraphEdgeKind::HandlesRoute
                    | RepoGraphEdgeKind::Fetches
                    // PR t16t: synthesized trait-dispatch caller edges
                    // carry the same "behavioral" blast-radius semantics
                    // as a direct call site.
                    | RepoGraphEdgeKind::TraitDispatchCall
            )
        };
        let fan_in = graph
            .graph()
            .edges_directed(node_index, petgraph::Direction::Incoming)
            .filter(|e| is_behavioral(e.weight().kind))
            .count();
        let fan_out = graph
            .graph()
            .edges_directed(node_index, petgraph::Direction::Outgoing)
            .filter(|e| is_behavioral(e.weight().kind))
            .count();
        // v8: line range from the graph's symbol_ranges sidecar.
        let (start_line, end_line) = node
            .file_path
            .as_ref()
            .and_then(|p| graph.range_for_node(node_index, p))
            .map(|(s, e)| (Some(s), Some(e)))
            .unwrap_or((None, None));
        // v8: entry-point flag derived from incoming EntryPointOf edges.
        let is_entry_point = graph
            .graph()
            .edges_directed(node_index, petgraph::Direction::Incoming)
            .any(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf);
        Ok(Some(SymbolDescription {
            key: format_node_key(&node.id),
            kind: format!("{:?}", node.kind).to_lowercase(),
            display_name: node.display_name.clone(),
            signature: node.signature.clone(),
            documentation,
            file: node.file_path.as_ref().map(|p| p.display().to_string()),
            start_line,
            end_line,
            fan_in,
            fan_out,
            visibility: node.visibility.map(|v| v.as_str().to_string()),
            is_external: node.is_external,
            is_entry_point,
            is_test: node.is_test,
            complexity: node
                .complexity
                .map(super::refactor::complexity_metrics_to_wire),
        }))
    }

    /// PR C1: 360° symbol context. Resolve `key` to a single graph node,
    /// gather every incident edge, bucket by [`EdgeCategory`], and hard-cap
    /// each list at 30. When `include_content` is true, attempt to read
    /// the symbol body from disk (best-effort: failures degrade silently
    /// to `content: None`).
    pub(super) async fn context(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        include_content: bool,
    ) -> Result<Option<SymbolContext>, String> {
        use petgraph::Direction;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let node_index = match resolve_node_or_err(&graph, key) {
            Ok(idx) => idx,
            Err(_) => return Ok(None),
        };
        let node = graph.node(node_index);

        // v8: filter related symbols through graph_excluded_paths so the
        // 360° view doesn't pull in synthetic SCIP module-tree nodes
        // (`crate/`, `…/MODULE.`) or vendored copies for the queried
        // symbol's neighborhood.
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        // Build incoming/outgoing buckets. We over-collect into per-category
        // Vecs and truncate at 30 once everything is in — sorting by
        // confidence (desc) so the highest-trust edges win the cap.
        let mut incoming: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
            std::collections::BTreeMap::new();
        let mut outgoing: std::collections::BTreeMap<EdgeCategory, Vec<RelatedSymbol>> =
            std::collections::BTreeMap::new();

        for dir in [Direction::Incoming, Direction::Outgoing] {
            for edge in graph.graph().edges_directed(node_index, dir) {
                let other_index = match dir {
                    Direction::Incoming => edge.source(),
                    Direction::Outgoing => edge.target(),
                };
                let other = graph.node(other_index);
                // v8: skip external (vendored / third-party / cross-crate)
                // related symbols. The 360° view is "what surrounds THIS
                // codebase symbol"; an imported `tokio::Future` showing
                // up alongside in-repo callers is noise.
                if other.is_external {
                    continue;
                }
                let other_key = format_node_key(&other.id);
                let other_file = other.file_path.as_ref().map(|p| p.display().to_string());
                if exclusions.excludes(&other_key, other_file.as_deref(), &other.display_name) {
                    continue;
                }
                let category = classify_edge_category(Some(edge.weight()), other);
                let related = build_related_symbol(other, edge.weight().confidence);
                let bucket = match dir {
                    Direction::Incoming => incoming.entry(category).or_default(),
                    Direction::Outgoing => outgoing.entry(category).or_default(),
                };
                bucket.push(related);
            }
        }

        // Plan-mandated hard limit: 30 per category. Sort desc by
        // confidence first so the bucket-truncation drops the
        // lowest-confidence entries.
        for buckets in [&mut incoming, &mut outgoing] {
            for entries in buckets.values_mut() {
                entries.sort_by(|a, b| {
                    b.confidence
                        .partial_cmp(&a.confidence)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| a.uid.cmp(&b.uid))
                });
                entries.truncate(30);
            }
        }

        // Pin the symbol's range and (optionally) body content.
        let (start_line, end_line) = node
            .file_path
            .as_ref()
            .and_then(|p| graph.range_for_node(node_index, p))
            .unwrap_or((0, 0));
        let file_path = node
            .file_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        let content = if include_content && start_line > 0 && !file_path.is_empty() {
            read_symbol_content(&ctx.clone_path, &file_path, start_line, end_line)
        } else {
            None
        };

        let method_metadata = build_method_metadata(node);

        let symbol = SymbolNode {
            uid: format_node_key(&node.id),
            name: node.display_name.clone(),
            kind: kind_label_for_node(node),
            file_path,
            start_line,
            end_line,
            content,
            method_metadata,
            complexity: node
                .complexity
                .map(super::refactor::complexity_metrics_to_wire),
        };

        // PR F2: populate process memberships from the per-graph
        // sidecar. Empty when process detection is disabled
        // (`DJINN_PROCESS_DETECTION=false`), when the cached artifact
        // pre-dates v4, or when the queried node doesn't appear in
        // any traced flow.
        let processes: Vec<ProcessRef> = graph
            .processes_for_node(node_index)
            .into_iter()
            .map(|p| ProcessRef {
                id: p.id.clone(),
                uid: p.id.clone(),
                label: p.label.clone(),
                role: "step".to_string(),
            })
            .collect();

        Ok(Some(SymbolContext {
            symbol,
            incoming,
            outgoing,
            processes,
        }))
    }

    pub(super) async fn status(&self, ctx: &ProjectCtx) -> Result<GraphStatus, String> {
        use djinn_db::RepoGraphCacheRepository;

        let (project_root, _index_tree_path) =
            djinn_graph::canonical_graph::normalize_graph_query_paths(&ctx.clone_path);

        // Source of truth: the `repo_graph_cache` row written by the K8s
        // graph warmer Job. The server process itself never rebuilds —
        // status reports whatever the warmer has persisted.
        let cache_repo = RepoGraphCacheRepository::new(self.state.db().clone());
        let row = cache_repo
            .latest_for_project(&ctx.id)
            .await
            .map_err(|e| format!("read repo_graph_cache: {e}"))?;

        let Some(row) = row else {
            return Ok(GraphStatus {
                project_id: ctx.id.clone(),
                warmed: false,
                last_warm_at: None,
                pinned_commit: None,
                commits_since_pin: None,
                route_parity_enabled: djinn_graph::route_extraction::route_parity_enabled(),
                route_exclusion_config: serde_json::to_value(
                    djinn_graph::repo_graph::RouteExclusionConfig::default(),
                )
                .unwrap_or_else(|_| serde_json::Value::Null),
            });
        };

        let commits_since_pin = djinn_graph::canonical_graph::canonical_graph_count_commits_since(
            &project_root,
            &row.commit_sha,
        )
        .await;

        Ok(GraphStatus {
            project_id: ctx.id.clone(),
            warmed: true,
            last_warm_at: Some(row.built_at),
            pinned_commit: Some(row.commit_sha.clone()),
            commits_since_pin,
            route_parity_enabled: djinn_graph::route_extraction::route_parity_enabled(),
            route_exclusion_config:
                djinn_graph::repo_graph::deserialize_repo_graph_artifact_bincode(&row.graph_blob)
                    .map(|artifact| serde_json::to_value(artifact.route_exclusion_config).ok())
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| {
                        serde_json::to_value(
                            djinn_graph::repo_graph::RouteExclusionConfig::default(),
                        )
                        .unwrap_or_else(|_| serde_json::Value::Null)
                    }),
        })
    }

    pub(super) async fn crate_graph(&self, ctx: &ProjectCtx) -> Result<CrateGraphResponse, String> {
        crate_graph_from_warmed_cache(ctx).await
    }
}

// Helpers extracted to query_helpers.rs to satisfy the file-size guard.
#[cfg(test)]
pub(super) use query_helpers::test_helpers;
use query_helpers::{collect_implementations, crate_graph_from_warmed_cache};
