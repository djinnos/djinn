use super::*;
use djinn_core::clock::Clock;

// ── Handler ─────────────────────────────────────────────────────────────────────

#[tool_router(router = graph_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Query the repository dependency graph built from SCIP indexer output.
    #[tool(
        description = "Query the repository dependency graph and file-coupling index. Prefer `uid` as the stable exact node input; fall back to `name` + `file_path` + `kind` when a UID is unavailable; ambiguous names return ranked candidates. Agent-boundary traversal triage controls include `limit`, `offset`, `pageLimit`, `summaryOnly`, and `byDepthCounts`. Partial pages and capped summaries are triage views; absence from a page or summary is NOT evidence a node/edge/pair is absent from the full graph. Operations include workspaces, neighbors, ranked, impact, implementations, search, query_subgraph, route_map, shape_check, api_impact, flow, cycles, orphans, path, edges, symbols_at, diff_touches, detect_changes, describe, context, status, snapshot, api_surface, boundary_check, hotspots, complexity, refactor_candidates, metrics_at, dead_symbols, deprecated_callers, touches_hot_path, coupling, churn, coupling_hotspots, coupling_hubs, crate_graph, and impact_check."
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
        // jc47: capture the caller commit before pre-resolve potentially
        // short-circuits. The field is already normalized (blank → None)
        // by `code_graph` / the chat extension before reaching here.
        let caller_head = params
            .current_head
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        match Self::pre_resolve_key(self.state.repo_graph().as_ref(), ctx, params).await? {
            None => {}
            Some(mut short_circuit) => {
                if let Some(ref head) = caller_head {
                    attach_graph_staleness(
                        self.state.repo_graph().as_ref(),
                        ctx,
                        head,
                        &mut short_circuit,
                    )
                    .await;
                }
                return Ok(short_circuit);
            }
        }

        let timeout = code_graph_dispatch_timeout();
        let op = params.operation.clone();
        let project_id = params.project_id.clone();
        let started = djinn_core::clock::SystemClock::new().now_instant();

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

        // jc47: attach per-query staleness metadata on success. Only
        // populated when the caller supplied a commit; a missing or
        // failed status lookup yields a non-stale-safe shape rather
        // than erroring. This never blocks or triggers re-warming.
        if let Ok(mut response) = result {
            if let Some(ref head) = caller_head {
                attach_graph_staleness(self.state.repo_graph().as_ref(), ctx, head, &mut response)
                    .await;
            }
            // glqk: attach the coverage advisory (best-effort, cheap DB read;
            // no graph blob). Only the six designated ops carry a coverage
            // field — the helper skips every other variant. Independent of
            // `caller_head`: coverage is about workspace index status, not the
            // caller's commit.
            attach_coverage_advisory(self.state.db(), &project_id, &mut response).await;
            Ok(response)
        } else {
            result
        }
    }

    /// Inner op-string match. Lives here so [`Self::dispatch_code_graph`]
    /// can wrap it uniformly in timeout + tracing without each per-op
    /// handler having to know about either.
    ///
    /// Dispatch is registry-derived: the operation string is looked up
    /// in [`operation_registry::CODE_GRAPH_REGISTRY`] and the matching
    /// entry's canonical name drives the handler match.  Unknown
    /// operations produce an error whose expected-ops list is
    /// auto-generated from the registry so it never goes stale.
    async fn dispatch_code_graph_op(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let op = params.operation.as_str();

        // Registry is the gatekeeper — unregistered ops fail early
        // with an auto-generated expected-ops list.
        let entry = operation_registry::lookup_by_name(op).ok_or_else(|| {
            let names = operation_registry::registered_names().join(", ");
            format!("unknown code_graph operation '{op}': expected one of {names}")
        })?;

        // Validation routing is registry-derived: the entry's
        // ValidationCategory determines which validators run.  This
        // mirrors the inline validation each handler performs so the
        // routing decision comes from the single source of truth.
        validation::run_validation_checks(entry.validation, params)?;

        // Dispatch on the registry entry's canonical name.  Every
        // entry in CODE_GRAPH_REGISTRY has a handler arm here.  Adding
        // a new operation requires one registry entry plus the handler
        // function — no independent stale operation-name list.
        match entry.name {
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
            "coupling_hotspots" => self.code_graph_coupling_hotspots(ctx, params).await,
            "coupling_hubs" => self.code_graph_coupling_hubs(ctx, params).await,
            "churn" => self.code_graph_churn(ctx, params).await,
            "snapshot" => self.code_graph_snapshot(ctx, params).await,
            "crate_graph" => self.code_graph_crate_graph(ctx, params).await,
            "impact_check" => self.code_graph_impact_check(ctx, params).await,
            "coverage" => self.code_graph_coverage(ctx, params).await,
            // Every registry entry is handled above.  This arm is
            // unreachable because `lookup_by_name` already rejected
            // unknown ops, but the compiler needs a wildcard.
            other => Err(format!(
                "internal dispatch error: registry entry '{other}' has no handler arm"
            )),
        }
    }

    /// PR C2 dispatcher hook: for ops that read a caller-supplied node
    /// key (`neighbors`, `impact`, `implementations`, `describe`,
    /// `context`) or two keys (`path`), pre-resolve via
    /// [`RepoGraphOps::resolve`] so the inner op gets either a unique
    /// RepoNodeKey or short-circuits on a disambiguation list / hard miss.
    ///
    /// Pre-resolve classification is **registry-derived**: the
    /// operation's [`operation_registry::PreResolveCategory`] determines
    /// which resolution path is taken.  No independent handwritten
    /// operation-name lists are maintained.
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
        // Look up the registry entry to determine pre-resolve behaviour.
        // Unknown ops (not in the registry) skip pre-resolve — they'll
        // fail later in dispatch_code_graph_op with a clear error.
        let entry = operation_registry::lookup_by_name(&params.operation);

        match entry.map(|e| e.pre_resolve) {
            Some(operation_registry::PreResolveCategory::SingleKey) => {
                if let Some(key) = params.key.as_deref().filter(|k| !k.is_empty()) {
                    let kind_hint = params.kind_hint.as_deref();
                    match graph.resolve(ctx, key, kind_hint).await? {
                        ResolveOutcome::Found(uid) => {
                            params.key = Some(uid);
                        }
                        ResolveOutcome::Ambiguous(candidates) => {
                            return Ok(Some(CodeGraphResponse::Ambiguous(AmbiguousResponse {
                                candidates,
                                next_step: None,
                                graph_staleness: None,
                            })));
                        }
                        ResolveOutcome::NotFound => {
                            return Ok(Some(CodeGraphResponse::NotFound(NotFoundResponse {
                                not_found: NotFoundDetail {
                                    query: key.to_string(),
                                    kind_hint: kind_hint.map(str::to_string),
                                },
                                next_step: None,
                                graph_staleness: None,
                            })));
                        }
                    }
                }
            }
            Some(operation_registry::PreResolveCategory::DualKey) => {
                // `path` is the only DualKey op; resolve `from` and `to`.
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
                                graph_staleness: None,
                            })));
                        }
                        ResolveOutcome::NotFound => {
                            return Ok(Some(CodeGraphResponse::NotFound(NotFoundResponse {
                                not_found: NotFoundDetail {
                                    query: key.to_string(),
                                    kind_hint: kind_hint.map(str::to_string),
                                },
                                next_step: None,
                                graph_staleness: None,
                            })));
                        }
                    }
                }
            }
            Some(operation_registry::PreResolveCategory::None) | None => {
                // No pre-resolve for this operation.
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
