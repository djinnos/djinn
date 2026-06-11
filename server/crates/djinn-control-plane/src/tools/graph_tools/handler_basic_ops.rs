use super::*;

impl DjinnMcpServer {
    pub(super) async fn code_graph_neighbors(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        validate_direction(params.direction.as_deref())?;
        validate_group_by(params.group_by.as_deref())?;
        // PR A3: `neighbors` repurposes `kind_filter` for an *edge* kind
        // (`reads` / `writes`); other ops use it for the node kind
        // (`file` / `symbol`). Validate against the edge-kind set here so
        // a typo surfaces server-side rather than silently dropping every
        // neighbor.
        validate_edge_kind_filter(params.kind_filter.as_deref())?;
        let result = self
            .state
            .repo_graph()
            .neighbors(
                ctx,
                key,
                params.direction.as_deref(),
                params.group_by.as_deref(),
                params.kind_filter.as_deref(),
            )
            .await?;
        // Bound the wire size — the underlying neighbors() call returns every
        // edge incident on the node (1k+ for high-centrality files), which
        // makes the MCP response unusably large. Sort by weight desc and cap
        // at `limit` (default 20, matching other list operations).
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let (neighbors, file_groups) = match result {
            NeighborsResult::Detailed(mut v) => {
                v.retain(|n| !exclusions.excludes(&n.key, None, &n.display_name));
                v.sort_by(|a, b| {
                    b.edge_weight
                        .partial_cmp(&a.edge_weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                v.truncate(limit);
                (Some(v), None)
            }
            NeighborsResult::Grouped(mut v) => {
                v.retain(|g| !exclusions.excludes(&g.file, Some(&g.file), &g.file));
                v.sort_by_key(|g| std::cmp::Reverse(g.occurrence_count));
                v.truncate(limit);
                (None, Some(v))
            }
        };
        Ok(CodeGraphResponse::Neighbors(NeighborsResponse {
            key: key.to_string(),
            neighbors,
            file_groups,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_ranked(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        validate_sort_by(params.sort_by.as_deref())?;
        let limit = params.limit.unwrap_or(20) as usize;
        // Over-fetch so filtering doesn't leave us short of the
        // caller's requested limit. 4× is a cheap slack — on the
        // platform repo today Tier 1 strips ~2% of ranked nodes, so
        // 4× covers any realistic Tier 2 glob list without needing a
        // second round-trip. Clamp to 200 to keep the cache lookup
        // cheap and the post-filter linear.
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 200);
        // pb94: route workspace resolution through the shared helper so
        // valid / unknown / single-workspace / empty semantics stay in
        // one place.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let nodes = self
            .state
            .repo_graph()
            .ranked(
                ctx,
                scope.workspace.as_deref(),
                params.kind_filter.as_deref(),
                params.sort_by.as_deref(),
                fetch_limit,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let mut nodes: Vec<RankedNode> = nodes
            .into_iter()
            .filter(|n| !exclusions.excludes(&n.key, None, &n.display_name))
            .take(limit)
            .collect();
        // The bridge already returns ranked-order; `filter` preserves it.
        nodes.truncate(limit);
        Ok(CodeGraphResponse::Ranked(RankedResponse {
            nodes,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_implementations(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let implementations = self.state.repo_graph().implementations(ctx, key).await?;
        Ok(CodeGraphResponse::Implementations(
            ImplementationsResponse {
                symbol: key.to_string(),
                implementations,
                next_step: None,
            },
        ))
    }

    pub(super) async fn code_graph_impact(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        validate_group_by(params.group_by.as_deref())?;
        let depth = params.limit.unwrap_or(3) as usize;
        // PR A2: validate `min_confidence` lives in `[0, 1]` before letting
        // it loose on the BFS frontier; out-of-range values would silently
        // collapse the impact set to zero or do nothing.
        if let Some(c) = params.min_confidence
            && !(0.0..=1.0).contains(&c)
        {
            return Err(format!("invalid min_confidence {c}: must be in [0.0, 1.0]"));
        }
        // pb94: `impact` is a TRAVERSAL op — `workspace` scopes only the
        // seed (the `key`) inside the workspace, the BFS walk itself
        // must remain unconstrained so cross-workspace blast radius
        // remains visible. The shared helper returns the effective
        // scope (which is `None` for unknown slugs so the bridge
        // resolves `key` from the full graph) and the hint envelope.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let result = self
            .state
            .repo_graph()
            .impact(
                ctx,
                scope.workspace.as_deref(),
                key,
                depth,
                params.group_by.as_deref(),
                params.min_confidence,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let (impact, file_groups, metrics) = match result {
            ImpactResult::Detailed(mut v) => {
                // ImpactEntry has no display_name; match key only (Tier
                // 1 still catches module artifacts; Tier 2 globs bound
                // against the SCIP key, matching the old client-side
                // behaviour).
                v.retain(|e| !exclusions.excludes(&e.key, None, &e.key));
                let metrics = metrics_from_detailed(&v);
                (Some(v), None, metrics)
            }
            ImpactResult::Grouped(mut v) => {
                v.retain(|g| !exclusions.excludes(&g.file, Some(&g.file), &g.file));
                let metrics = metrics_from_grouped(&v);
                (None, Some(v), metrics)
            }
        };
        // PR C3: classify the post-exclusion blast radius and ship
        // both the structured bucket (`risk`) and a human-readable
        // 1-line summary so chat UIs / reviewer prompts / dashboards
        // can each pick the form they want.
        let risk = ImpactRisk::classify(metrics.direct, metrics.total, metrics.modules);
        let summary = impact_summary(metrics);
        Ok(CodeGraphResponse::Impact(ImpactResponse {
            key: key.to_string(),
            impact,
            file_groups,
            risk: Some(risk),
            summary: Some(summary),
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_search(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let query = require_query(params)?;
        validate_kind_filter(params.kind_filter.as_deref())?;
        let mode = resolve_search_mode(params.mode.as_deref())?;
        let limit = params.limit.unwrap_or(20) as usize;
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 200);
        // pb94: surface unknown-workspace hints for `search` too so a
        // caller that mistypes the workspace doesn't get a hard-empty
        // hit list. The bridge call itself is workspace-agnostic today
        // (search uses a name index that already spans all workspaces),
        // so the resolved scope is used purely for the hint envelope.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        // PR B4: dispatch on mode. `name` keeps the pre-PR-B4 fast
        // path; `hybrid` runs the RRF orchestrator on the bridge,
        // which composes lexical + semantic + structural signals and
        // tags each hit's `match_kind` for debug surfaces.
        let hits = match mode {
            SearchMode::Name => {
                self.state
                    .repo_graph()
                    .search(ctx, query, params.kind_filter.as_deref(), fetch_limit)
                    .await?
            }
            SearchMode::Hybrid => {
                self.state
                    .repo_graph()
                    .hybrid_search(ctx, query, params.kind_filter.as_deref(), fetch_limit)
                    .await?
            }
        };
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let hits: Vec<SearchHit> = hits
            .into_iter()
            .filter(|h| !exclusions.excludes(&h.key, h.file.as_deref(), &h.display_name))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Search(SearchResponse {
            query: query.to_string(),
            hits,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_query_subgraph(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let query = params
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| "'query' is required for operation 'query_subgraph'".to_string())?;
        validate_kind_filter(params.kind_filter.as_deref())?;

        let token_budget =
            bounded_optional_usize(params.token_budget, "token_budget", 1_024, 32_000, false)?;
        let max_depth = bounded_optional_usize(params.max_depth, "max_depth", 0, 8, true)?;
        let max_seeds = bounded_optional_usize(params.max_seeds, "max_seeds", 1, 32, false)?;

        let edge_filter = params
            .edge_filters
            .clone()
            .unwrap_or_else(|| params.edge_kind.clone().into_iter().collect())
            .into_iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        let file_filter = params
            .file_filter
            .as_deref()
            .or(params.file_glob.as_deref())
            .or(params.file.as_deref())
            .or(params.from_glob.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let request = QuerySubgraphRequest {
            query: query.to_string(),
            workspace: ctx
                .workspace
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            context_filter: params
                .context_filter
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            file_filter,
            kind_filter: params
                .kind_filter
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            edge_filter,
            token_budget,
            max_depth,
            max_seeds,
        };
        let result = self.state.repo_graph().query_subgraph(ctx, request).await?;
        Ok(CodeGraphResponse::QuerySubgraph(QuerySubgraphResponse {
            query_subgraph: result,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_cycles(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        let min_size = params.min_size.unwrap_or(2).max(0) as usize;
        // pb94: surface unknown-workspace hints for `cycles` so a caller
        // that mistypes the workspace doesn't get a hard-empty result
        // when the bridge's SCC cache is workspace-agnostic. The
        // bridge call itself stays workspace-agnostic (the SCC cache
        // spans the whole graph); the resolved scope is used purely
        // for the hint envelope.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        // Ask the warmer cache for SCCs with a size floor of 2 so we
        // can still shed an SCC whose surviving members drop below the
        // user-requested `min_size` after exclusion filtering. We
        // re-apply `min_size` post-filter below.
        let fetch_floor = min_size.max(2);
        let cycles = self
            .state
            .repo_graph()
            .cycles(ctx, params.kind_filter.as_deref(), fetch_floor)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let cycles: Vec<CycleGroup> = cycles
            .into_iter()
            .filter_map(|group| {
                let members: Vec<_> = group
                    .members
                    .into_iter()
                    .filter(|m| !exclusions.excludes(&m.key, None, &m.display_name))
                    .collect();
                if members.len() < min_size.max(2) {
                    None
                } else {
                    Some(CycleGroup {
                        size: members.len(),
                        members,
                    })
                }
            })
            .collect();
        Ok(CodeGraphResponse::Cycles(CyclesResponse {
            cycles,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_orphans(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_kind_filter(params.kind_filter.as_deref())?;
        validate_visibility(params.visibility.as_deref())?;
        let limit = params.limit.unwrap_or(50) as usize;
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 500);
        // pb94: route workspace resolution through the shared helper so
        // valid / unknown / single-workspace / empty semantics stay in
        // one place.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let orphans = self
            .state
            .repo_graph()
            .orphans(
                ctx,
                scope.workspace.as_deref(),
                params.kind_filter.as_deref(),
                params.visibility.as_deref(),
                fetch_limit,
            )
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let orphans: Vec<OrphanEntry> = orphans
            .into_iter()
            .filter(|o| !exclusions.excludes_orphan(&o.key, o.file.as_deref(), &o.display_name))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Orphans(OrphansResponse {
            orphans,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_path(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let (from, to) = require_from_to(params)?;
        let max_depth = params.max_depth.map(|v| v.max(0) as usize);
        // pb94: `path` is a TRAVERSAL op — `workspace` scopes only the
        // `from` and `to` endpoint resolution, the shortest-path walk
        // itself must remain unconstrained so cross-workspace path
        // context remains visible.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let path = self
            .state
            .repo_graph()
            .path(ctx, scope.workspace.as_deref(), from, to, max_depth)
            .await?;
        Ok(CodeGraphResponse::Path(PathResponse {
            path,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_edges(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let (from_glob, to_glob) = require_globs(params)?;
        let limit = params.limit.unwrap_or(100) as usize;
        // pb94: surface unknown-workspace hints for `edges` so a caller
        // that mistypes the workspace doesn't get a hard-empty result
        // when their `from_glob` / `to_glob` only matches edges in the
        // requested workspace. The bridge call itself stays
        // workspace-agnostic (edge globs match by path); the resolved
        // scope is used purely for the hint envelope.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        // Over-fetch so the exclusion post-filter doesn't starve the
        // requested limit. Edges are cheap to drop but we want the
        // returned set to honour `limit` after Tier 1+2 pruning.
        let fetch_limit = (limit.saturating_mul(4)).clamp(limit, 400);
        let edges = self
            .state
            .repo_graph()
            .edges(
                ctx,
                from_glob,
                to_glob,
                params.edge_kind.as_deref(),
                fetch_limit,
            )
            .await?;
        // Drop edges whose `from` OR `to` endpoint is filtered — a
        // boundary-check style query over the graph should not surface
        // edges that touch SCIP-artifact nodes or user-excluded paths,
        // even if the glob pair technically matches.
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let edges: Vec<EdgeEntry> = edges
            .into_iter()
            .filter(|e| {
                !exclusions.excludes(&e.from, Some(&e.from), &e.from)
                    && !exclusions.excludes(&e.to, Some(&e.to), &e.to)
            })
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Edges(EdgesResponse {
            edges,
            workspace_hint: scope.hint,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_describe(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let description = self.state.repo_graph().describe(ctx, key).await?;
        Ok(CodeGraphResponse::Describe(DescribeResponse {
            description,
            next_step: None,
        }))
    }

    /// PR C1: `context` op handler. Resolves to a 360° symbol view
    /// (categorized incoming/outgoing dicts + method metadata). The
    /// pre-resolve pass runs in `pre_resolve_key`; if we got here the
    /// `key` is already a canonical RepoNodeKey or the resolver
    /// short-circuited.
    pub(super) async fn code_graph_context(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let key = require_key(params)?;
        let include_content = params.include_content.unwrap_or(false);
        let symbol_context = self
            .state
            .repo_graph()
            .context(ctx, key, include_content)
            .await?
            .ok_or_else(|| format!("symbol '{key}' not found in graph"))?;
        Ok(CodeGraphResponse::Context(ContextResponse {
            symbol_context,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_status(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let status = self.state.repo_graph().status(ctx).await?;
        Ok(CodeGraphResponse::Status(StatusResponse {
            status,
            next_step: None,
        }))
    }

    pub(super) async fn code_graph_workspaces(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let result = self.state.repo_graph().workspaces(ctx).await?;
        Ok(CodeGraphResponse::Workspaces(WorkspacesResponse {
            result,
            next_step: None,
        }))
    }
}
