use super::*;

impl DjinnMcpServer {
    /// Handler for `operation = "symbols_at"`.
    ///
    /// Requires `file` + `start_line`; `end_line` defaults to `start_line`
    /// when omitted. No exclusion filter is applied here — the caller
    /// already named the file, so this is a lookup, not a discovery.
    pub(super) async fn code_graph_symbols_at(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let file = params
            .file
            .as_deref()
            .filter(|f| !f.is_empty())
            .ok_or_else(|| format!("'file' is required for operation '{}'", params.operation))?;
        let start_line = params.start_line.ok_or_else(|| {
            format!(
                "'start_line' is required for operation '{}'",
                params.operation
            )
        })?;
        let start_line_u32 = u32::try_from(start_line.max(0)).unwrap_or(0);
        let end_line_u32 = params
            .end_line
            .map(|n| u32::try_from(n.max(0)).unwrap_or(0));
        let hits = self
            .state
            .repo_graph()
            .symbols_at(ctx, file, start_line_u32, end_line_u32)
            .await?;
        Ok(CodeGraphResponse::SymbolsAt(SymbolsAtResponse {
            file: file.to_string(),
            hits,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "diff_touches"`.
    ///
    /// Requires a non-empty `changed_ranges` list. The Phase 0 graph
    /// exclusions filter is applied post-query: touched symbols whose
    /// key, file, or display_name matches an exclusion are dropped
    /// because they are noise even in PR-review context (generated
    /// `mod.rs`, third-party shims, etc.).
    pub(super) async fn code_graph_diff_touches(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let changed_ranges = params.changed_ranges.as_deref().ok_or_else(|| {
            format!(
                "'changed_ranges' is required for operation '{}'",
                params.operation
            )
        })?;
        if changed_ranges.is_empty() {
            return Err(format!(
                "'changed_ranges' must not be empty for operation '{}'",
                params.operation
            ));
        }
        let result = self
            .state
            .repo_graph()
            .diff_touches(ctx, changed_ranges)
            .await?;
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let touched_symbols: Vec<TouchedSymbol> = result
            .touched_symbols
            .into_iter()
            .filter(|s| !exclusions.excludes(&s.key, s.file.as_deref(), &s.display_name))
            .collect();
        Ok(CodeGraphResponse::DiffTouches(DiffTouchesResponse {
            touched_symbols,
            affected_files: result.affected_files,
            unknown_files: result.unknown_files,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "detect_changes"`.
    ///
    /// Two input modes:
    /// * `from_sha` + `to_sha` — runs `git diff --unified=0 from..to`
    ///   server-side and maps hunks via `symbols_enclosing`.
    /// * `changed_files` — every symbol in each listed file is treated
    ///   as touched (no line-level filtering).
    ///
    /// When both are provided line-level wins; the file list is
    /// ignored. At least one mode must be supplied.
    pub(super) async fn code_graph_detect_changes(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let from = params.from_sha.as_deref().filter(|s| !s.is_empty());
        let to = params.to_sha.as_deref().filter(|s| !s.is_empty());
        let changed_files: Vec<String> = params
            .changed_files
            .as_ref()
            .map(|v| v.iter().filter(|s| !s.is_empty()).cloned().collect())
            .unwrap_or_default();
        let line_mode = from.is_some() && to.is_some();
        if !line_mode && changed_files.is_empty() {
            return Err(format!(
                "'detect_changes' requires either both 'from_sha' and \
                 'to_sha', or a non-empty 'changed_files' list (got \
                 from_sha={}, to_sha={}, changed_files={})",
                from.is_some(),
                to.is_some(),
                changed_files.len()
            ));
        }
        let result = self
            .state
            .repo_graph()
            .detect_changes(ctx, from, to, &changed_files)
            .await?;

        // Apply Phase-0 graph exclusions to suppress generated/vendored
        // noise — match the diff_touches policy.
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let mut filtered = result;
        filtered
            .touched_symbols
            .retain(|s| !exclusions.excludes(&s.uid, Some(&s.file_path), &s.name));
        // Rebuild `by_file` after filtering so the rollup matches.
        let mut by_file: std::collections::BTreeMap<String, Vec<_>> =
            std::collections::BTreeMap::new();
        for sym in &filtered.touched_symbols {
            by_file
                .entry(sym.file_path.clone())
                .or_default()
                .push(sym.clone());
        }
        filtered.by_file = by_file;

        // Bias the next-step hint toward the highest-tier symbol —
        // High > Medium > Low, then by symbol name (stable).
        let next_step = pick_next_step_target(&filtered.touched_symbols).map(|target| {
            format!(
                "Call `code_graph impact target={target}` to assess each \
                 touched symbol's blast radius."
            )
        });

        Ok(CodeGraphResponse::DetectedChanges(
            DetectedChangesResponse {
                detected_changes: filtered,
                next_step,
                graph_staleness: None,
            },
        ))
    }

    /// Handler for `operation = "api_surface"`.
    pub(super) async fn code_graph_api_surface(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        validate_visibility(params.visibility.as_deref())?;
        let limit = params.limit.unwrap_or(100).max(0) as usize;
        // pb94: route workspace resolution through the shared helper so
        // valid / unknown / single-workspace / empty semantics stay in
        // one place.
        let scope = resolve_workspace_scope(self.state.repo_graph(), ctx).await?;
        let symbols = self
            .state
            .repo_graph()
            .api_surface(
                ctx,
                scope.workspace.as_deref(),
                params.module_glob.as_deref(),
                params.visibility.as_deref(),
                limit.saturating_mul(4).clamp(limit, 500),
            )
            .await?;
        // The bridge already applies the exclusions; also defend against
        // noise that might slip in if the bridge is evolved later.
        let exclusions = self.load_graph_exclusions(&params.project_id).await;
        let symbols: Vec<ApiSurfaceEntry> = symbols
            .into_iter()
            .filter(|e| !exclusions.excludes(&e.key, e.file.as_deref(), &e.display_name))
            .take(limit)
            .collect();
        Ok(CodeGraphResponse::ApiSurface(ApiSurfaceResponse {
            symbols,
            workspace_hint: scope.hint,
            next_step: None,
            graph_staleness: None,
            coverage: None,
        }))
    }

    /// Handler for `operation = "boundary_check"`.
    pub(super) async fn code_graph_boundary_check(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let rules = params
            .rules
            .as_deref()
            .ok_or_else(|| format!("'rules' is required for operation '{}'", params.operation))?;
        if rules.is_empty() {
            return Err(format!(
                "'rules' must not be empty for operation '{}'",
                params.operation
            ));
        }
        let level = params.level.as_deref().unwrap_or("file");
        let violations = self
            .state
            .repo_graph()
            .boundary_check(ctx, rules, level)
            .await?;
        Ok(CodeGraphResponse::BoundaryCheck(BoundaryCheckResponse {
            violations,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "hotspots"`.
    pub(super) async fn code_graph_hotspots(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let window = params.window_days.unwrap_or(90).clamp(1, 365);
        let window_u32 = u32::try_from(window).unwrap_or(90);
        let limit = params.limit.unwrap_or(20).max(0) as usize;
        let limit = limit.clamp(1, 100);
        let hotspots = self
            .state
            .repo_graph()
            .hotspots(ctx, window_u32, params.file_glob.as_deref(), limit)
            .await?;
        Ok(CodeGraphResponse::Hotspots(HotspotsResponse {
            hotspots,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "complexity"` (iter 28). Reuses the
    /// shared `sort_by` / `file_glob` / `limit` params; adds a dedicated
    /// `target` discriminator (`functions` | `files`). Validation of
    /// `target` and `sort_by` happens in the bridge impl so the same
    /// error shape surfaces from every call path.
    pub(super) async fn code_graph_complexity(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let target = params.target.as_deref().unwrap_or("functions");
        let sort_by = params.sort_by.as_deref().unwrap_or("cognitive");
        let limit = params.limit.unwrap_or(30).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let result = self
            .state
            .repo_graph()
            .complexity(ctx, target, sort_by, params.file_glob.as_deref(), limit)
            .await?;
        Ok(CodeGraphResponse::Complexity(ComplexityResponse {
            complexity: result,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "refactor_candidates"` (iter 29).
    /// Composite ranking that fuses cognitive complexity, file-level
    /// churn, and PageRank z-scores. Reuses `since_days` (default 90,
    /// clamped to `[1, 365]` server-side), `file_glob`, and `limit`
    /// (default 30, clamped to `[1, 200]`).
    pub(super) async fn code_graph_refactor_candidates(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let since_days_u32 = params
            .since_days
            .map(|d| u32::try_from(d.max(0)).unwrap_or(0));
        let limit = params.limit.unwrap_or(30).max(0) as usize;
        let limit = limit.clamp(1, 200);
        let candidates = self
            .state
            .repo_graph()
            .refactor_candidates(ctx, since_days_u32, params.file_glob.as_deref(), limit)
            .await?;
        Ok(CodeGraphResponse::RefactorCandidates(
            RefactorCandidatesResponse {
                refactor_candidates: candidates,
                next_step: None,
                graph_staleness: None,
            },
        ))
    }

    /// Handler for `operation = "metrics_at"`.
    pub(super) async fn code_graph_metrics_at(
        &self,
        ctx: &ProjectCtx,
        _params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let metrics = self.state.repo_graph().metrics_at(ctx).await?;
        Ok(CodeGraphResponse::MetricsAt(MetricsAtResponse {
            metrics,
            next_step: None,
            graph_staleness: None,
        }))
    }

    /// Handler for `operation = "dead_symbols"`.
    pub(super) async fn code_graph_dead_symbols(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let confidence = params.confidence.as_deref().unwrap_or("high");
        if !matches!(confidence, "high" | "med" | "low") {
            return Err(format!(
                "invalid confidence '{confidence}': expected 'high', 'med', or 'low'"
            ));
        }
        let limit = params.limit.unwrap_or(100).max(0) as usize;
        let symbols = self
            .state
            .repo_graph()
            .dead_symbols(ctx, confidence, limit)
            .await?;
        Ok(CodeGraphResponse::DeadSymbols(DeadSymbolsResponse {
            symbols,
            next_step: None,
            graph_staleness: None,
            coverage: None,
        }))
    }

    /// Handler for `operation = "deprecated_callers"`.
    pub(super) async fn code_graph_deprecated_callers(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
    ) -> Result<CodeGraphResponse, String> {
        let limit = params.limit.unwrap_or(50).max(0) as usize;
        let hits = self
            .state
            .repo_graph()
            .deprecated_callers(ctx, limit)
            .await?;
        Ok(CodeGraphResponse::DeprecatedCallers(
            DeprecatedCallersResponse {
                hits,
                next_step: None,
                graph_staleness: None,
            },
        ))
    }
}
