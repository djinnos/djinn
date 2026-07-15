use super::*;

use super::handler_impact_check::{
    CrateIndex, ImpactAggregator, check_impact_check_staleness,
    derive_safe_slice_and_recommendation,
};

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
        // df6s: the underlying `neighbors()` returns every edge incident
        // on the node (1k+ for high-centrality files). We retain + sort
        // the **unsliced** post-exclusion set first so the agent-facing
        // pagination can never be misread as graph absence. Slicing
        // happens only when building the response DTO, mirroring the
        // epic's "pagination at the boundary" rule.
        //
        // Default cap is 20 — matches the other list ops. The pagination
        // helper clamps to `[1, 1000]` so a runaway caller can't dump the
        // whole graph into a single page. Legacy `limit` is honoured as
        // the page cap (so pre-df6s callers keep working unchanged);
        // the new `page_limit` field takes precedence when set.
        let default_cap = params.limit.unwrap_or(20).max(0) as usize;
        let pagination = PaginationParams::resolve(params, default_cap);
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let (mut neighbors, mut file_groups) = match result {
            NeighborsResult::Detailed(v) => {
                let mut v = v;
                v.retain(|n| !exclusions.excludes(&n.key, None, &n.display_name));
                v.sort_by(|a, b| {
                    b.edge_weight
                        .partial_cmp(&a.edge_weight)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                (Some(v), None)
            }
            NeighborsResult::Grouped(v) => {
                let mut v = v;
                v.retain(|g| !exclusions.excludes(&g.file, Some(&g.file), &g.file));
                v.sort_by_key(|g| std::cmp::Reverse(g.occurrence_count));
                (None, Some(v))
            }
        };
        // Compute the unsliced total **before** applying the page slice
        // so a caller paginating across an offset can never see `total`
        // shrink on the second page.
        let total = neighbors
            .as_ref()
            .map(|v| v.len())
            .or_else(|| file_groups.as_ref().map(|v| v.len()))
            .unwrap_or(0);
        // Build the (offset, limit) page over the unsliced lists.
        let mut has_more = false;
        if let Some(v) = neighbors.as_mut() {
            let page = apply_page_slice(v, pagination.offset, pagination.limit);
            has_more = page.has_more;
        }
        if let Some(v) = file_groups.as_mut() {
            let page = apply_page_slice(v, pagination.offset, pagination.limit);
            has_more = has_more || page.has_more;
        }
        // `summary_only` strips the heavy list fields; `total` carries
        // the count signal. Skip serializing the lists entirely.
        if pagination.summary_only {
            neighbors = None;
            file_groups = None;
        }
        let emit_pagination = pagination_applied(pagination, total, pagination.limit);
        let (resp_offset, resp_limit, resp_total, resp_has_more) = if emit_pagination {
            (
                Some(pagination.offset),
                Some(pagination.limit),
                Some(total),
                Some(has_more),
            )
        } else {
            (None, None, None, None)
        };
        Ok(CodeGraphResponse::Neighbors(NeighborsResponse {
            key: key.to_string(),
            neighbors,
            file_groups,
            total: resp_total,
            offset: resp_offset,
            limit: resp_limit,
            has_more: resp_has_more,
            summary_only: pagination.summary_only.then_some(true),
            next_step: None,
            graph_staleness: None,
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
            graph_staleness: None,
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
                graph_staleness: None,
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
        // df6s: `limit` keeps its pre-df6s semantic — it's the BFS
        // traversal **depth** (default 3). Result-cap pagination is
        // served by `page_limit` instead so a caller that wants a
        // smaller page doesn't accidentally shrink the blast-radius
        // search. `page_limit` defaults to 100, matching the
        // `neighbors`/`coupling_hotspots` family at the wire layer.
        let depth = params.limit.unwrap_or(3) as usize;
        let pagination = PaginationParams::resolve(params, 100);
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
        let (mut impact, mut file_groups, metrics) = match result {
            ImpactResult::Detailed(v) => {
                let mut v = v;
                // ImpactEntry has no display_name; match key only (Tier
                // 1 still catches module artifacts; Tier 2 globs bound
                // against the SCIP key, matching the old client-side
                // behaviour).
                v.retain(|e| !exclusions.excludes(&e.key, None, &e.key));
                let metrics = metrics_from_detailed(&v);
                (Some(v), None, metrics)
            }
            ImpactResult::Grouped(v) => {
                let mut v = v;
                v.retain(|g| !exclusions.excludes(&g.file, Some(&g.file), &g.file));
                let metrics = metrics_from_grouped(&v);
                (None, Some(v), metrics)
            }
        };
        // PR C3: classify the post-exclusion blast radius and ship
        // both the structured bucket (`risk`) and a human-readable
        // 1-line summary so chat UIs / reviewer prompts / dashboards
        // can each pick the form they want.
        //
        // df6s: `risk` + `summary` continue to be derived from the
        // **unsliced** post-exclusion set, so a capped page still
        // reports the same blast-radius bucket as the full response.
        // The classification metrics read `total`/`modules` from the
        // unsliced counts, never from the page.
        let risk = ImpactRisk::classify(metrics.direct, metrics.total, metrics.modules);
        let summary = impact_summary(metrics);
        // Compute the unsliced total before applying the page slice.
        // `impact` and `file_groups` are mutually exclusive at this
        // point (the bridge returns one or the other), so we just sum
        // whichever is `Some`.
        let total = impact
            .as_ref()
            .map(|v| v.len())
            .or_else(|| file_groups.as_ref().map(|v| v.len()))
            .unwrap_or(0);
        // Apply the page slice at the response DTO layer so the
        // internal BFS never has to know about pagination.
        let mut has_more = false;
        if let Some(v) = impact.as_mut() {
            let page = apply_page_slice(v, pagination.offset, pagination.limit);
            has_more = has_more || page.has_more;
        }
        if let Some(v) = file_groups.as_mut() {
            let page = apply_page_slice(v, pagination.offset, pagination.limit);
            has_more = has_more || page.has_more;
        }
        // df6s: per-depth counts are computed from the **unsliced**
        // detailed set so they reflect the full impact distribution
        // (a `page_limit=10` page still reports e.g. `{1: 12, 2: 7}`
        // when there are 19 total impacted entries).
        let by_depth_counts = if pagination.summary_only || pagination.by_depth_counts {
            impact.as_ref().map(|v| build_by_depth_counts(v))
        } else {
            None
        };
        // `summary_only` strips the heavy list fields. Risk + summary
        // (and the per-depth breakdown when applicable) carry the
        // count signal.
        if pagination.summary_only {
            impact = None;
            file_groups = None;
        }
        let emit_pagination = pagination_applied(pagination, total, pagination.limit);
        let (resp_offset, resp_limit, resp_total, resp_has_more) = if emit_pagination {
            (
                Some(pagination.offset),
                Some(pagination.limit),
                Some(total),
                Some(has_more),
            )
        } else {
            (None, None, None, None)
        };
        Ok(CodeGraphResponse::Impact(ImpactResponse {
            key: key.to_string(),
            impact,
            file_groups,
            risk: Some(risk),
            summary: Some(summary),
            workspace_hint: scope.hint,
            total: resp_total,
            offset: resp_offset,
            limit: resp_limit,
            has_more: resp_has_more,
            summary_only: pagination.summary_only.then_some(true),
            by_depth_counts,
            next_step: None,
            graph_staleness: None,
            coverage: None,
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
            graph_staleness: None,
            coverage: None,
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
            graph_staleness: None,
        }))
    }

    pub(super) async fn code_graph_route_map(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let limit = bounded_required_limit(params.limit, 50, "route_map")?;
        let route_map = self
            .state
            .repo_graph()
            .route_map(
                ctx,
                params.route_id.as_deref(),
                params.method.as_deref(),
                params.path.as_deref(),
                params.path_glob.as_deref(),
                params.framework.as_deref(),
                limit,
            )
            .await?;
        Ok(CodeGraphResponse::RouteMap(RouteMapResponse {
            route_map,
            next_step: None,
            graph_staleness: None,
        }))
    }

    pub(super) async fn code_graph_shape_check(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let selector = require_route_selector(params)?;
        let shape_check = self
            .state
            .repo_graph()
            .shape_check(
                ctx,
                selector.route_id,
                selector.method,
                selector.path,
                params.include_optional.unwrap_or(false),
            )
            .await?;
        Ok(CodeGraphResponse::ShapeCheck(ShapeCheckResponse {
            shape_check,
            next_step: None,
            graph_staleness: None,
        }))
    }

    pub(super) async fn code_graph_api_impact(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let selector = require_route_selector(params)?;
        let min_confidence = params.min_confidence.unwrap_or(0.5);
        validate_min_confidence_value(min_confidence)?;
        let limit = bounded_required_limit(params.limit, 50, "api_impact")?;
        let api_impact = self
            .state
            .repo_graph()
            .api_impact(
                ctx,
                selector.route_id,
                selector.method,
                selector.path,
                min_confidence,
                limit,
            )
            .await?;
        Ok(CodeGraphResponse::ApiImpact(ApiImpactResponse {
            api_impact,
            next_step: None,
            graph_staleness: None,
        }))
    }

    pub(super) async fn code_graph_flow(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let query = require_query(params)?;
        validate_flow_kind_filter(params.kind_filter.as_deref())?;
        let limit = bounded_required_limit(params.limit, 20, "flow")?;
        let flow = self
            .state
            .repo_graph()
            .flow(ctx, query, params.kind_filter.as_deref(), limit)
            .await?;
        Ok(CodeGraphResponse::Flow(FlowResponse {
            flow,
            next_step: None,
            graph_staleness: None,
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
            graph_staleness: None,
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
            graph_staleness: None,
            coverage: None,
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
            graph_staleness: None,
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
            graph_staleness: None,
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
            graph_staleness: None,
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
            graph_staleness: None,
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
            graph_staleness: None,
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
            graph_staleness: None,
        }))
    }

    /// glqk: index-coverage table. Reads `project_workspace_coverage` rows
    /// directly — the CHEAP path that never loads the graph blob — and returns
    /// the per-(workspace, language) coverage entries, a per-language rollup,
    /// and the discovered-but-unindexed source roots.
    pub(super) async fn code_graph_coverage(
        &self,
        _ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        use djinn_db::{ProjectWorkspaceCoverageRepository, coverage_status_is_gap};

        let rows = ProjectWorkspaceCoverageRepository::new(self.state.db().clone())
            .list_for_project(&params.project_id)
            .await
            .map_err(|e| format!("coverage: list_for_project failed: {e}"))?;

        let mut workspaces: Vec<CoverageWorkspaceEntry> = Vec::with_capacity(rows.len());
        let mut unindexed_source_roots: Vec<UnindexedSourceRoot> = Vec::new();
        // language -> (total, indexed, gap, discovered_sum, indexed_sum,
        // any_discovered_known, any_indexed_known)
        let mut rollup: std::collections::BTreeMap<String, [i64; 7]> =
            std::collections::BTreeMap::new();
        let mut has_gaps = false;

        for row in rows {
            let is_gap = coverage_status_is_gap(&row.status);
            has_gaps |= is_gap;
            let is_indexed = row.status == djinn_db::COVERAGE_STATUS_INDEXED;

            if is_gap {
                unindexed_source_roots.push(UnindexedSourceRoot {
                    workspace_slug: row.workspace_slug.clone(),
                    language: row.language.clone(),
                    workspace_root: row.workspace_root.clone(),
                    status: row.status.clone(),
                });
            }

            let acc = rollup.entry(row.language.clone()).or_default();
            acc[0] += 1; // total
            if is_indexed {
                acc[1] += 1; // indexed
            }
            if is_gap {
                acc[2] += 1; // gap
            }
            if let Some(d) = row.discovered_files {
                acc[3] += d;
                acc[5] = 1;
            }
            if let Some(i) = row.indexed_files {
                acc[4] += i;
                acc[6] = 1;
            }

            workspaces.push(CoverageWorkspaceEntry {
                workspace_slug: row.workspace_slug,
                language: row.language,
                status: row.status,
                is_gap,
                detail: row.detail,
                workspace_root: row.workspace_root,
                marker_evidence: row.marker_evidence,
                discovered_files: row.discovered_files,
                indexed_files: row.indexed_files,
                commit_sha: row.commit_sha,
                warmed_at: row.warmed_at,
            });
        }

        let language_rollup: Vec<CoverageLanguageRollup> = rollup
            .into_iter()
            .map(|(language, acc)| CoverageLanguageRollup {
                language,
                workspaces_total: acc[0] as usize,
                workspaces_indexed: acc[1] as usize,
                workspaces_gap: acc[2] as usize,
                discovered_files: (acc[5] == 1).then_some(acc[3]),
                indexed_files: (acc[6] == 1).then_some(acc[4]),
            })
            .collect();

        let next_step = if has_gaps {
            Some(
                "Some workspaces are not indexed. Before asserting \"no callers / \
                 unused / safe to remove\" for symbols in these workspaces, fall \
                 back to grep and say so."
                    .to_string(),
            )
        } else {
            None
        };

        Ok(CodeGraphResponse::Coverage(CoverageResponse {
            coverage: true,
            has_gaps,
            workspaces,
            language_rollup,
            unindexed_source_roots,
            next_step,
            graph_staleness: None,
        }))
    }

    pub(super) async fn code_graph_crate_graph(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let result = self.state.repo_graph().crate_graph(ctx).await?;
        Ok(CodeGraphResponse::CrateGraph(CrateGraphOpResponse {
            crates: result.crates,
            edges: result.edges,
            message: result.message,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Advisory impact preflight: determine which crates, files, and
    /// symbols would break if the proposed removals/renames land, and
    /// whether the proposed task slice can ship independently.
    ///
    /// For crate-level targets, uses `crate_graph().edges` to find
    /// inbound consumer crates. For symbol/file targets, uses
    /// `impact()` with `is_external` already filtered out by the
    /// bridge.
    ///
    /// Decomposition (epic uajf, task 1zcr): the original body mixed
    /// six concerns — request validation, graph freshness, crate
    /// index building, per-target impact aggregation, slice /
    /// recommendation derivation, and response formatting. Each
    /// concern now lives in a named private helper so the top-level
    /// flow reads as a short orchestrator. Response shape, public
    /// signature, recommendation strings, stale-graph short-circuit,
    /// `low_confidence` handling, and the graceful-skip behaviour
    /// for individual target impact errors are preserved verbatim.
    pub(super) async fn code_graph_impact_check(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        // 1. Request validation: extract the non-empty
        //    `impact_targets` list and the optional `scope_crates`
        //    set used for the `safe_independent_slice` derivation.
        let (targets, scope_crates) = self.validate_impact_check_request(params)?;

        // 2. Graph freshness check (shared with `code_graph` ops —
        //    no duplication, epic z3en/kfgh).
        let caller_head_raw = params.current_head.as_deref().unwrap_or("");
        let staleness_info =
            check_impact_check_staleness(self.state.repo_graph().as_ref(), ctx, caller_head_raw)
                .await;
        if staleness_info.is_stale {
            return Ok(CodeGraphResponse::ImpactCheck(
                self.build_stale_impact_check_response(&staleness_info),
            ));
        }

        // 3. Build crate indexes (known crates + directory
        //    prefixes) from the workspace crate graph.
        let crate_result = self.state.repo_graph().crate_graph(ctx).await?;
        let crate_index = CrateIndex::from_crate_graph(&crate_result);

        // 4. Aggregate per-target impacts into a small accumulator.
        let depth = params.limit.unwrap_or(3).max(0) as usize;
        let mut aggregator = ImpactAggregator::new();
        for target in targets {
            self.aggregate_target_impact(ctx, params, target, depth, &crate_index, &mut aggregator)
                .await;
        }
        let (affected_crates, affected_files, affected_symbols) = aggregator.finalized();

        // 5. Derive the safe-slice boolean and the recommendation
        //    string.  Pure function of the aggregated sets and the
        //    caller-supplied scope — no I/O, no bridging.
        let (safe_independent_slice, recommendation) =
            derive_safe_slice_and_recommendation(&affected_crates, &scope_crates);

        // 5b. glqk coverage gate: if an UNINDEXED workspace intersects the
        //     scope (or affected) crates, the impact analysis can't prove "no
        //     callers" — escalate to `needs_spike` and name the workspace. Only
        //     `impact_check` escalates (per the design); the other five ops just
        //     attach the advisory.
        let mut relevant_crates: std::collections::HashSet<String> = scope_crates.clone();
        relevant_crates.extend(affected_crates.iter().cloned());
        let coverage_gaps = self
            .impact_check_coverage_gaps(&params.project_id, &crate_index, &relevant_crates)
            .await;
        if !coverage_gaps.is_empty() {
            return Ok(CodeGraphResponse::ImpactCheck(
                self.build_uncovered_impact_check_response(
                    affected_crates,
                    affected_files,
                    affected_symbols,
                    &coverage_gaps,
                    &staleness_info,
                ),
            ));
        }

        // 6. Format the final response in one place so the wire
        //    shape stays in lock-step with the tool contract.
        Ok(CodeGraphResponse::ImpactCheck(
            self.build_impact_check_response(
                affected_crates,
                affected_files,
                affected_symbols,
                safe_independent_slice,
                recommendation,
                &staleness_info,
            ),
        ))
    }
}
