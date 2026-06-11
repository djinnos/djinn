use super::*;

// ── Handler ─────────────────────────────────────────────────────────────────────

#[tool_router(router = graph_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Query the repository dependency graph built from SCIP indexer output.
    #[tool(
        description = "Query the repository dependency graph built from SCIP indexer output and the commit-based file-coupling index. Operations: workspaces (enumerate graph workspaces with node counts and freshness), neighbors (edges in/out of a node, with optional group_by=file rollup), ranked (top nodes; sort_by pagerank/in_degree/out_degree/total_degree), impact (transitive dependents, with optional group_by=file rollup), implementations (find implementors of a trait/interface symbol), search (name-based symbol lookup; `query` is substring/name text), query_subgraph (natural-language, budget-bounded subgraph query; requires nonblank `query` as the question), route_map (route handler/middleware/consumer map; optional route_id or method/path_glob/framework filters), shape_check (route response-shape drift; requires route_id or method+path), api_impact (route consumer risk; requires route_id or method+path), flow (execution-flow/process search; requires nonblank query). query_subgraph details: requires nonblank `query` as the question. Optional narrowing fields: `workspace` scopes to a warmed workspace, `file_filter` narrows by repository-relative path/file substring [`file_glob` alias when omitted], `kind_filter` narrows node kind to file|symbol, `edge_filters` narrows traversal edge kinds such as calls/imports/returns/reads/writes/implements/extends [`edge_kind` single-kind alias when omitted], and `context_filter` narrows to a subsystem/API/type/concern. Budget controls: `token_budget` is approximate; omit for the backend default (currently about 2000 tokens), positive values are clamped into 1024..=32000, and zero/negative values are rejected. `max_depth` is clamped into 0..=8 [0 means seeds only; omit for backend default, currently 2], and positive `max_seeds` is clamped into 1..=32 [omit for backend default, currently 6; zero/negative rejected]. The response is bounded and includes nodes/edges, seed debug metadata, inferred/requested edge kinds, budget/truncation/traversal/hub-skip state where available, and `narrowing_hints` suggesting tighter context/path/kind/edge filters or budget changes), cycles (strongly-connected components), orphans (zero-incoming-reference nodes, with visibility filter), path (shortest dependency path), edges (enumerate edges by from_glob/to_glob), symbols_at (given file+line range, return SCIP symbols whose definition range encloses those lines — diff-hunk → symbol lookup), diff_touches (given a list of changed line ranges parsed from `git diff --unified=0 base..head`, return every base-graph symbol touched, with fan-in/fan-out and file grouping; the base graph is always current main — this op does NOT build a head graph), detect_changes (given from_sha + to_sha [or a changed_files list], return touched symbols + their PageRank tier [High/Medium/Low quartile] + per-file rollup; shells out to `git diff --unified=0 from_sha..to_sha` server-side and maps hunks via symbols_enclosing — replaces the architect's manual diff inspection), describe (symbol signature/documentation without an LSP round trip), context (PR C1: 360° symbol view — categorized incoming/outgoing dicts [calls/reads/writes/extends/implements/...], plus structured method_metadata when SCIP populates it; pass include_content=true to include the symbol body. Each category list is hard-capped at 30 entries), status (peek at the persisted canonical graph cache; never warms), api_surface (list every public symbol with fan-in/fan-out and a used-outside-crate signal), boundary_check (edge-based architecture rule scanner over from_glob→to_glob pairs; returns forbidden violations), hotspots (file churn × centrality ranking over a configurable window; top_symbols per file), complexity (rank functions or files by complexity metric — target: functions|files, sort_by: cognitive|cyclomatic|nloc|max_nesting|param_count, file_glob, limit), refactor_candidates (composite refactor-priority ranking — fuses cognitive complexity × file-level churn × PageRank into a single z-score and surfaces the top function-level targets; respects since_days [default 90, clamped 1..=365], file_glob, limit [default 30, clamped 1..=200]; each entry carries the composite score, a tier label [high/medium/low], and the underlying raw + z-score signals so callers can re-rank locally), metrics_at (scalar graph snapshot: node/edge/cycle counts, god-object floor, orphans, public API and doc coverage), dead_symbols (no-incoming-edge-from-entry-points enumeration; confidence=high|med|low), deprecated_callers (symbols whose signature/documentation contains #[deprecated] or @deprecated, with caller list), touches_hot_path (given entry and sink SCIP keys, report which queried symbols sit on any entry→sink shortest path), coupling (files most frequently co-edited with `file`, sourced from the per-commit change log; returns co-edit count, last co-edit timestamp, and up to three supporting SHAs per peer), churn (top files by distinct-commit count over an optional `since_days` window; returns commit count, cumulative insertions/deletions, and last-touched timestamp), coupling_hotspots (top file PAIRS by co-edit count project-wide; returns [{file_a,file_b,co_edits,last_co_edit}]; respects `since_days` and `max_files_per_commit` [default 15] — useful for spotting implicit coupling between distant parts of the tree), coupling_hubs (top FILES by cumulative coupling across all partners; returns [{file_path,total_coupling,partner_count}] — change-propagation risk map, higher total_coupling means a touch to this file is more likely to require touching many others), snapshot (PR D2: full graph snapshot with workspace-fair, endpoint-consistent capping — returns {snapshot:{project_id,git_head,generated_at,truncated,total_nodes,total_edges,node_cap,nodes,edges}}; default cap 2000 nodes [Sigma WebGL ceiling], settable via `limit` up to 10k. Drives the `/code-graph` UI's force-directed render. Pass tests=include|exclude|only to filter test files/symbols — include is the default [whole graph], exclude drops everything marked is_test, only keeps test nodes; classification is the canonical is_test flag built from the file-path convention OR the SCIP Test role). All coupling / churn outputs are filtered through the project's `project_graph_exclusions` glob list at query time, so tuning exclusions takes effect without re-ingesting."
    )]
    pub async fn code_graph(
        &self,
        Parameters(mut params): Parameters<CodeGraphParams>,
    ) -> Json<ErrorOr<CodeGraphResponse>> {
        params.normalize();
        // Resolve `project` (UUID or slug) to (project_id, clone_path)
        // once here; inner handlers read the pre-populated `project_id`
        // and `project_path` fields without hitting the DB again.
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match repo.resolve(&params.project).await {
            Ok(Some(id)) => match repo.get(&id).await {
                Ok(Some(p)) => p,
                _ => {
                    return Json(ErrorOr::Error(ErrorResponse {
                        error: format!("project not found: {}", params.project),
                    }));
                }
            },
            Ok(None) => {
                return Json(ErrorOr::Error(ErrorResponse {
                    error: format!("project not found: {}", params.project),
                }));
            }
            Err(e) => {
                return Json(ErrorOr::Error(ErrorResponse {
                    error: format!("project lookup failed: {e}"),
                }));
            }
        };
        params.project_id = project.id.clone();
        params.project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
                .to_string_lossy()
                .into_owned();

        // Build the resolved `ProjectCtx` once. Inner handlers pass it
        // straight to the `RepoGraphOps` bridge so no downstream code
        // needs to reverse-parse `{projects_root}/{owner}/{repo}`.
        let ctx = ProjectCtx {
            id: params.project_id.clone(),
            clone_path: params.project_path.clone(),
            workspace: params.workspace.clone(),
            sub_path: None,
        };

        // Both pre-resolve and the per-op match now live inside
        // `dispatch_code_graph`, which also wraps the inner call in a
        // tokio timeout + tracing span so the chat handler can't be
        // wedged forever by a slow op.
        let result = self.dispatch_code_graph(&ctx, &mut params).await;

        Json(match result {
            Ok(mut response) => {
                if next_step_hints_enabled() {
                    attach_next_step_hint(params.operation.as_str(), &mut response);
                }
                ErrorOr::Ok(response)
            }
            Err(error) => ErrorOr::Error(ErrorResponse { error }),
        })
    }
}

/// Default per-op timeout for `dispatch_code_graph`. Override with the
/// `DJINN_CODE_GRAPH_DISPATCH_TIMEOUT_SECS` env var. 60s is comfortably
/// above the slowest healthy op we measure (snapshot at full size
/// ~1.5s) but well under the chat handler's outer guard so the
/// timeout error surfaces to the model instead of stalling the stream.
const CODE_GRAPH_DISPATCH_TIMEOUT_DEFAULT_SECS: u64 = 60;

fn code_graph_dispatch_timeout() -> std::time::Duration {
    let secs = std::env::var("DJINN_CODE_GRAPH_DISPATCH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(CODE_GRAPH_DISPATCH_TIMEOUT_DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

impl DjinnMcpServer {
    /// Single source of truth for `code_graph` dispatch.
    ///
    /// Wraps the op-string match in:
    /// - **Pre-resolve** — short identifiers (`User`, `helper`) routed
    ///   through `RepoGraphOps::resolve` so they short-circuit to
    ///   `Ambiguous` / `NotFound` instead of failing inside the inner
    ///   handler.
    /// - **Tokio timeout** — a slow op (e.g. an unindexed coupling
    ///   self-join hitting Dolt's planner pathology) returns a
    ///   structured timeout error after
    ///   `DJINN_CODE_GRAPH_DISPATCH_TIMEOUT_SECS` (default 60s) instead
    ///   of stalling the chat stream forever. This is the chat-handler
    ///   hang fix — without it, a slow op wedges the whole tool loop.
    /// - **Tracing span** — every dispatch emits an `info_span!` with
    ///   `op`, `project_id`, `elapsed_ms`, `status` so we can grep
    ///   latency / failure rates.
    ///
    /// Both the MCP tool entry (`code_graph` below) and the chat
    /// extension (`djinn_agent::extension::handlers::code_intel`) call
    /// this method. Keep the per-op match here; do not duplicate it.
    pub async fn dispatch_code_graph(
        &self,
        ctx: &ProjectCtx,
        params: &mut CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        match Self::pre_resolve_key(self.state.repo_graph().as_ref(), ctx, params).await? {
            None => {}
            Some(short_circuit) => return Ok(short_circuit),
        }

        let timeout = code_graph_dispatch_timeout();
        let op = params.operation.clone();
        let project_id = params.project_id.clone();
        let started = std::time::Instant::now();

        let span = tracing::info_span!(
            "code_graph",
            op = %op,
            project_id = %project_id,
        );
        let inner = self.dispatch_code_graph_op(ctx, params);
        let result = tokio::time::timeout(timeout, inner)
            .instrument(span)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "code_graph op '{op}' exceeded {}s — try a narrower call \
                     (lower limit, file_glob filter, since_days) or a different op",
                    timeout.as_secs()
                ))
            });

        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => tracing::info!(
                target: "djinn_control_plane::tools::graph_tools",
                op = %op,
                project_id = %project_id,
                elapsed_ms,
                status = "ok",
                "code_graph dispatch completed"
            ),
            Err(err) => tracing::warn!(
                target: "djinn_control_plane::tools::graph_tools",
                op = %op,
                project_id = %project_id,
                elapsed_ms,
                status = "error",
                error = %err,
                "code_graph dispatch failed"
            ),
        }

        result
    }

    /// Inner op-string match. Lives here so [`Self::dispatch_code_graph`]
    /// can wrap it uniformly in timeout + tracing without each per-op
    /// handler having to know about either.
    async fn dispatch_code_graph_op(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        match params.operation.as_str() {
            "neighbors" => self.code_graph_neighbors(ctx, params).await,
            "ranked" => self.code_graph_ranked(ctx, params).await,
            "implementations" => self.code_graph_implementations(ctx, params).await,
            "impact" => self.code_graph_impact(ctx, params).await,
            "search" => self.code_graph_search(ctx, params).await,
            "query_subgraph" => self.code_graph_query_subgraph(ctx, params).await,
            "route_map" => self.code_graph_route_map(ctx, params).await,
            "shape_check" => self.code_graph_shape_check(ctx, params).await,
            "api_impact" => self.code_graph_api_impact(ctx, params).await,
            "flow" => self.code_graph_flow(ctx, params).await,
            "cycles" => self.code_graph_cycles(ctx, params).await,
            "orphans" => self.code_graph_orphans(ctx, params).await,
            "path" => self.code_graph_path(ctx, params).await,
            "edges" => self.code_graph_edges(ctx, params).await,
            "describe" => self.code_graph_describe(ctx, params).await,
            "context" => self.code_graph_context(ctx, params).await,
            "status" => self.code_graph_status(ctx, params).await,
            "workspaces" => self.code_graph_workspaces(ctx, params).await,
            "symbols_at" => self.code_graph_symbols_at(ctx, params).await,
            "diff_touches" => self.code_graph_diff_touches(ctx, params).await,
            "detect_changes" => self.code_graph_detect_changes(ctx, params).await,
            "api_surface" => self.code_graph_api_surface(ctx, params).await,
            "boundary_check" => self.code_graph_boundary_check(ctx, params).await,
            "hotspots" => self.code_graph_hotspots(ctx, params).await,
            "complexity" => self.code_graph_complexity(ctx, params).await,
            "refactor_candidates" => self.code_graph_refactor_candidates(ctx, params).await,
            "metrics_at" => self.code_graph_metrics_at(ctx, params).await,
            "dead_symbols" => self.code_graph_dead_symbols(ctx, params).await,
            "deprecated_callers" => self.code_graph_deprecated_callers(ctx, params).await,
            "touches_hot_path" => self.code_graph_touches_hot_path(ctx, params).await,
            "coupling" => self.code_graph_coupling(ctx, params).await,
            "churn" => self.code_graph_churn(ctx, params).await,
            "coupling_hotspots" => self.code_graph_coupling_hotspots(ctx, params).await,
            "coupling_hubs" => self.code_graph_coupling_hubs(ctx, params).await,
            "snapshot" => self.code_graph_snapshot(ctx, params).await,
            other => Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'query_subgraph', 'route_map', 'shape_check', \
                 'api_impact', 'flow', 'cycles', 'orphans', 'path', 'edges', \
                 'symbols_at', 'diff_touches', 'detect_changes', \
                 'describe', 'context', 'status', 'workspaces', \
                 'api_surface', 'boundary_check', 'hotspots', 'complexity', \
                 'refactor_candidates', 'metrics_at', \
                 'dead_symbols', 'deprecated_callers', 'touches_hot_path', \
                 'coupling', 'churn', 'coupling_hotspots', 'coupling_hubs', \
                 'snapshot'"
            )),
        }
    }

    /// PR C2 dispatcher hook: for ops that read a caller-supplied node
    /// key (`neighbors`, `impact`, `implementations`, `describe`,
    /// `path`), pre-resolve via [`RepoGraphOps::resolve`] so the inner
    /// op gets either a unique RepoNodeKey or short-circuits on a
    /// disambiguation list / hard miss.
    ///
    /// Returns:
    /// - `Ok(None)` — caller may dispatch the inner op as usual. For
    ///   `Found(uid)` we rewrite `params.key` (or `from`/`to` for
    ///   `path`) to the canonical key first.
    /// - `Ok(Some(response))` — short-circuit; emit `Ambiguous`/`NotFound`.
    /// - `Err(_)` — bridge call failed; surface as an MCP error.
    async fn pre_resolve_key(
        graph: &dyn crate::bridge::RepoGraphOps,
        ctx: &ProjectCtx,
        params: &mut CodeGraphParams,
    ) -> Result<Option<CodeGraphResponse>, String> {
        // Operations that take a single `key`. `search`/`ranked`/
        // `cycles`/`orphans`/`hotspots`/etc. don't go through
        // resolution — their `key` is a query/glob.
        let single_key_ops = [
            "neighbors",
            "impact",
            "implementations",
            "describe",
            // PR C1: `context` shares the same key-resolution path so a
            // short identifier like `User` short-circuits to Ambiguous /
            // NotFound instead of failing inside the graph backend.
            "context",
        ];
        if single_key_ops.contains(&params.operation.as_str())
            && let Some(key) = params.key.as_deref().filter(|k| !k.is_empty())
        {
            let kind_hint = params.kind_hint.as_deref();
            match graph.resolve(ctx, key, kind_hint).await? {
                ResolveOutcome::Found(uid) => {
                    params.key = Some(uid);
                }
                ResolveOutcome::Ambiguous(candidates) => {
                    return Ok(Some(CodeGraphResponse::Ambiguous(AmbiguousResponse {
                        candidates,
                        next_step: None,
                    })));
                }
                ResolveOutcome::NotFound => {
                    return Ok(Some(CodeGraphResponse::NotFound(NotFoundResponse {
                        not_found: NotFoundDetail {
                            query: key.to_string(),
                            kind_hint: kind_hint.map(str::to_string),
                        },
                        next_step: None,
                    })));
                }
            }
        }

        // `path` takes two keys; resolve both.
        if params.operation == "path" {
            for which in ["from", "to"] {
                let raw = match which {
                    "from" => params.from.as_deref().filter(|s| !s.is_empty()),
                    _ => params.to.as_deref().filter(|s| !s.is_empty()),
                };
                let Some(key) = raw else { continue };
                let kind_hint = params.kind_hint.as_deref();
                match graph.resolve(ctx, key, kind_hint).await? {
                    ResolveOutcome::Found(uid) => {
                        if which == "from" {
                            params.from = Some(uid);
                        } else {
                            params.to = Some(uid);
                        }
                    }
                    ResolveOutcome::Ambiguous(candidates) => {
                        return Ok(Some(CodeGraphResponse::Ambiguous(AmbiguousResponse {
                            candidates,
                            next_step: None,
                        })));
                    }
                    ResolveOutcome::NotFound => {
                        return Ok(Some(CodeGraphResponse::NotFound(NotFoundResponse {
                            not_found: NotFoundDetail {
                                query: key.to_string(),
                                kind_hint: kind_hint.map(str::to_string),
                            },
                            next_step: None,
                        })));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Load the per-project graph exclusions, rendered into a compiled
    /// [`GraphExclusions`] predicate. On any lookup failure we fall
    /// back to [`GraphExclusions::empty`], which still applies Tier 1
    /// (universal SCIP module-artifact suppression).
    pub(super) async fn load_graph_exclusions(&self, project_id: &str) -> GraphExclusions {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.get_config(project_id).await {
            Ok(Some(config)) => GraphExclusions::from_config(&config),
            Ok(None) => GraphExclusions::empty(),
            Err(e) => {
                tracing::debug!(
                    project_id = %project_id,
                    error = %e,
                    "graph_exclusions: config read failed; using Tier 1 only",
                );
                GraphExclusions::empty()
            }
        }
    }
}
