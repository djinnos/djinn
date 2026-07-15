use super::*;

impl DjinnMcpServer {
    /// Handler for `operation = "coupling"`.
    pub(super) async fn code_graph_coupling(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let file = params
            .file
            .as_deref()
            .filter(|f| !f.is_empty())
            .ok_or_else(|| format!("'file' is required for operation '{}'", params.operation))?;
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 200);
        // Over-fetch 25× so the exclusion filter doesn't starve the
        // returned set. Underlying SQL sort is invariant to LIMIT, so
        // this is effectively free.
        let fetch_limit = (limit.saturating_mul(25)).clamp(limit, 500);
        let coupled = self
            .state
            .repo_graph()
            .coupling(ctx, file, fetch_limit)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let coupled: Vec<CouplingEntry> = coupled
            .into_iter()
            .filter(|c| !exclusions.excludes_path(&c.file_path))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::Coupling(CouplingResponse {
            file: file.to_string(),
            coupled,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "coupling_hotspots"`.
    ///
    /// df6s: paginate the unsliced post-exclusion set at the
    /// response DTO layer. The `coupling_hotspots` bridge call
    /// returns the full ranked pair list (up to `fetch_limit`),
    /// then we apply `offset` / `limit` / `summary_only` only when
    /// building the response. `total` always reflects the
    /// unsliced count so a caller paginating across offset
    /// boundaries can never mistake an empty page for "no more
    /// pairs in the graph".
    pub(super) async fn code_graph_coupling_hotspots(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        // Legacy `limit` is honoured as the page cap so pre-df6s
        // callers keep working unchanged; the new `page_limit`
        // field takes precedence when set. The default of 20
        // matches the other list ops.
        let default_cap = params.limit.unwrap_or(20).max(0) as usize;
        let pagination = PaginationParams::resolve(params, default_cap);
        let max_files_per_commit =
            params.max_files_per_commit.unwrap_or(15).clamp(1, 1000) as usize;
        // Fetch the unsliced ranked set. The 25× over-fetch still
        // matters — Tier 1/2 exclusions can drop 50% of pairs in
        // practice, and we need the post-exclusion list to honour
        // the requested page cap.
        let fetch_limit = (pagination.limit.saturating_mul(25)).clamp(pagination.limit, 500);
        let since_days_u32 = params
            .since_days
            .map(|d| u32::try_from(d.max(0)).unwrap_or(0));
        let pairs = self
            .state
            .repo_graph()
            .coupling_hotspots(ctx, fetch_limit, since_days_u32, max_files_per_commit)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let mut pairs: Vec<CoupledPairEntry> = pairs
            .into_iter()
            .filter(|p| {
                !exclusions.excludes_path(&p.file_a) && !exclusions.excludes_path(&p.file_b)
            })
            .collect();
        // Compute the unsliced total before applying the page slice.
        let total = pairs.len();
        // Apply the (offset, limit) page at the response DTO layer.
        let page = apply_page_slice(&mut pairs, pagination.offset, pagination.limit);
        let has_more = page.has_more;
        // `summary_only` strips the heavy list fields; `total`
        // carries the count signal.
        if pagination.summary_only {
            pairs.clear();
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
        Ok(CodeGraphResponse::CouplingHotspots(
            CouplingHotspotsResponse {
                pairs,
                total: resp_total,
                offset: resp_offset,
                limit: resp_limit,
                has_more: resp_has_more,
                summary_only: pagination.summary_only.then_some(true),
                next_step: None,
                graph_staleness: None,
            },
        ))
    }

    /// Handler for `operation = "coupling_hubs"`.
    pub(super) async fn code_graph_coupling_hubs(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let max_files_per_commit =
            params.max_files_per_commit.unwrap_or(15).clamp(1, 1000) as usize;
        let fetch_limit = (limit.saturating_mul(25)).clamp(limit, 500);
        let since_days_u32 = params
            .since_days
            .map(|d| u32::try_from(d.max(0)).unwrap_or(0));
        let hubs = self
            .state
            .repo_graph()
            .coupling_hubs(ctx, fetch_limit, since_days_u32, max_files_per_commit)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let hubs: Vec<CouplingHubEntry> = hubs
            .into_iter()
            .filter(|h| !exclusions.excludes_path(&h.file_path))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::CouplingHubs(CouplingHubsResponse {
            hubs,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "churn"`.
    pub(super) async fn code_graph_churn(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let since_days_u32 = params
            .since_days
            .map(|d| u32::try_from(d.max(0)).unwrap_or(0));
        let files = self
            .state
            .repo_graph()
            .churn(ctx, limit, since_days_u32)
            .await?;
        Ok(CodeGraphResponse::Churn(ChurnResponse {
            files,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "touches_hot_path"`.
    pub(super) async fn code_graph_touches_hot_path(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let seed_entries = params.seed_entries.as_deref().ok_or_else(|| {
            format!(
                "'seed_entries' is required for operation '{}'",
                params.operation
            )
        })?;
        let seed_sinks = params.seed_sinks.as_deref().ok_or_else(|| {
            format!(
                "'seed_sinks' is required for operation '{}'",
                params.operation
            )
        })?;
        let symbols = params
            .symbols
            .as_deref()
            .ok_or_else(|| format!("'symbols' is required for operation '{}'", params.operation))?;
        // pb94: `touches_hot_path` is a TRAVERSAL op — `workspace`
        // scopes only seed/entry/sink resolution, the shortest-path
        // walk itself must remain unconstrained.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let hits = self
            .state
            .repo_graph()
            .touches_hot_path(
                ctx,
                scope.workspace.as_deref(),
                seed_entries,
                seed_sinks,
                symbols,
            )
            .await?;
        Ok(CodeGraphResponse::TouchesHotPath(TouchesHotPathResponse {
            hits,
            workspace_hint: scope.hint,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "snapshot"` (PR D2).
    ///
    /// Returns the full repo graph capped by PageRank tier so the
    /// `/code-graph` UI can render it through Sigma + ForceAtlas2
    /// without hitting the ~5k-node WebGL ceiling on large
    /// repositories. The cap defaults to 2000 (matches the plan's
    /// `Sigma.js performance ceiling at ~5k nodes` mitigation) and is
    /// settable via the `limit` field.
    ///
    /// `graph_excluded_paths` filtering happens in the bridge so
    /// `total_nodes` / `total_edges` reflect the post-exclusion graph
    /// — the cap is then applied to the surviving population.
    pub(super) async fn code_graph_snapshot(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        // Default 2000 nodes — plan §"Risks & mitigations" calls out
        // this as the Sigma WebGL ceiling. Explicit `limit` is honored up
        // to 1M (an anti-DoS bound, not a product ceiling): the galaxy
        // renderer (lmkv) draws the whole graph in one instanced draw
        // call, so "show me everything" is a legitimate request and the
        // old 10k clamp silently under-delivered it.
        let node_cap = params
            .limit
            .map(|l| l.max(1) as usize)
            .unwrap_or(2_000)
            .clamp(1, 1_000_000);
        let level = SnapshotLevel::parse(params.level.as_deref())?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        // pb94: route workspace resolution through the shared helper so
        // valid / unknown / single-workspace / empty semantics stay in
        // one place.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let mut snapshot = self
            .state
            .repo_graph()
            .snapshot(
                ctx,
                scope.workspace.as_deref(),
                level,
                node_cap,
                &exclusions,
            )
            .await?;
        // v10: apply the `tests=` filter to the assembled snapshot. The
        // UI defaults to `include` (it toggles test visibility
        // client-side off `SnapshotNode.is_test`); agents can pass
        // `exclude` / `only` to get a server-side-narrowed graph. Drop
        // edges whose endpoints no longer survive so the wire shape
        // stays internally consistent.
        let filter = TestFilter::parse(params.tests.as_deref(), TestFilter::Include);
        if filter != TestFilter::Include {
            snapshot.nodes.retain(|n| filter.keeps(n.is_test));
            let kept: std::collections::HashSet<&str> =
                snapshot.nodes.iter().map(|n| n.id.as_str()).collect();
            snapshot
                .edges
                .retain(|e| kept.contains(e.from.as_str()) && kept.contains(e.to.as_str()));
        }
        Ok(CodeGraphResponse::Snapshot(SnapshotResponse {
            snapshot,
            workspace_hint: scope.hint,
            next_step: None,
            graph_staleness: None,
        }))
    }
}
