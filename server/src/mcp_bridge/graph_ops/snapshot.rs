use super::*;

impl RepoGraphBridge {
    pub(super) async fn snapshot(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        level: SnapshotLevel,
        node_cap: usize,
        exclusions: &djinn_control_plane::tools::graph_exclusions::GraphExclusions,
    ) -> Result<SnapshotPayload, String> {
        use djinn_db::RepoGraphCacheRepository;

        // Pull the warmed graph + cached PageRank ranking. We deliberately
        // use `load_canonical_graph` (not `_only`) so the same warm step
        // that backs `ranked` / `cycles` is reused — recomputing PageRank
        // on every snapshot call would dominate the latency budget on
        // large repos.
        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Look up the pinned commit SHA the warmer recorded. We treat
        // `git_head` as authoritative — there's no point asking the
        // worktree because the warmed graph is built from the pinned
        // commit, not HEAD.
        let cache_repo = RepoGraphCacheRepository::new(self.state.db().clone());
        let cache_row = cache_repo
            .latest_for_project(&ctx.id)
            .await
            .map_err(|e| format!("read repo_graph_cache: {e}"))?;
        let git_head = cache_row
            .as_ref()
            .map(|r| r.commit_sha.clone())
            .unwrap_or_default();

        // ISO8601 UTC. Reuse the same `OffsetDateTime` path the rest of
        // the bridge takes (e.g. `built_at` in `repo_graph_cache`).
        let generated_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| String::new());

        Ok(build_snapshot_payload(
            &graph,
            &ranking,
            ctx.id.clone(),
            git_head,
            generated_at,
            exclusions,
            workspace,
            level,
            node_cap,
        ))
    }

    pub(super) async fn symbols_at(
        &self,
        ctx: &ProjectCtx,
        file: &str,
        start_line: u32,
        end_line: Option<u32>,
    ) -> Result<Vec<SymbolAtHit>, String> {
        use petgraph::Direction;
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let end = end_line.unwrap_or(start_line);
        let (start, end) = if start_line <= end {
            (start_line, end)
        } else {
            (end, start_line)
        };

        let file_path = std::path::Path::new(file);
        let hits = graph.symbols_enclosing(file_path, start, end);

        // Also surface the file node itself when present — this gives
        // callers a stable anchor even when symbol_ranges is empty (e.g.
        // the artifact-restored cache path before the next rebuild).
        let mut out: Vec<SymbolAtHit> = Vec::new();
        if let Some(file_idx) = graph.file_node(file_path) {
            let node = graph.node(file_idx);
            out.push(SymbolAtHit {
                key: format_node_key(&node.id),
                uid: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                file: node.file_path.as_ref().map(|p| p.display().to_string()),
                start_line: None,
                end_line: None,
                visibility: node.visibility.map(|v| v.as_str().to_string()),
                symbol_kind: None,
            });
        }

        for idx in hits {
            let node = graph.node(idx);
            // `symbols_enclosing` is populated by definitions only; we do
            // not have the exact range stored on the node itself, so the
            // per-hit start/end are omitted. Callers that need the range
            // can re-query via `symbols_at` with a tighter window.
            let _ = (
                graph.graph().edges_directed(idx, Direction::Incoming),
                graph.graph().edges_directed(idx, Direction::Outgoing),
            );
            out.push(SymbolAtHit {
                key: format_node_key(&node.id),
                uid: format_node_key(&node.id),
                kind: format!("{:?}", node.kind).to_lowercase(),
                display_name: node.display_name.clone(),
                file: node.file_path.as_ref().map(|p| p.display().to_string()),
                start_line: Some(start),
                end_line: Some(end),
                visibility: node.visibility.map(|v| v.as_str().to_string()),
                symbol_kind: node.symbol_kind.as_ref().map(|k| format!("{k:?}")),
            });
        }
        Ok(out)
    }

    pub(super) async fn diff_touches(
        &self,
        ctx: &ProjectCtx,
        changed_ranges: &[ChangedRange],
    ) -> Result<DiffTouchesResult, String> {
        use petgraph::Direction;
        use std::collections::BTreeSet;

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        let mut touched_indices: BTreeSet<petgraph::graph::NodeIndex> = BTreeSet::new();
        let mut affected_files: Vec<String> = Vec::new();
        let mut unknown_files: Vec<String> = Vec::new();
        let mut seen_affected: BTreeSet<String> = BTreeSet::new();
        let mut seen_unknown: BTreeSet<String> = BTreeSet::new();

        for range in changed_ranges {
            let end = range.end_line.unwrap_or(range.start_line);
            let (start, end) = if range.start_line <= end {
                (range.start_line, end)
            } else {
                (end, range.start_line)
            };
            let start_u32 = u32::try_from(start.max(0)).unwrap_or(0);
            let end_u32 = u32::try_from(end.max(0)).unwrap_or(0);
            let file_path = std::path::Path::new(&range.file);
            let file_present = graph.file_node(file_path).is_some();
            if file_present {
                if seen_affected.insert(range.file.clone()) {
                    affected_files.push(range.file.clone());
                }
            } else if seen_unknown.insert(range.file.clone()) {
                unknown_files.push(range.file.clone());
            }
            for idx in graph.symbols_enclosing(file_path, start_u32, end_u32) {
                touched_indices.insert(idx);
            }
        }

        let mut touched_symbols: Vec<TouchedSymbol> = touched_indices
            .into_iter()
            .map(|idx| {
                let node = graph.node(idx);
                let fan_in = graph
                    .graph()
                    .edges_directed(idx, Direction::Incoming)
                    .count();
                let fan_out = graph
                    .graph()
                    .edges_directed(idx, Direction::Outgoing)
                    .count();
                TouchedSymbol {
                    key: format_node_key(&node.id),
                    uid: format_node_key(&node.id),
                    display_name: node.display_name.clone(),
                    kind: format!("{:?}", node.kind).to_lowercase(),
                    symbol_kind: node.symbol_kind.as_ref().map(|k| format!("{k:?}")),
                    visibility: node.visibility.map(|v| v.as_str().to_string()),
                    file: node.file_path.as_ref().map(|p| p.display().to_string()),
                    start_line: None,
                    end_line: None,
                    fan_in,
                    fan_out,
                }
            })
            .collect();

        // Stable output: highest fan-in first so PR reviewers see the
        // most structurally central symbols at the top.
        touched_symbols.sort_by(|a, b| {
            b.fan_in
                .cmp(&a.fan_in)
                .then_with(|| b.fan_out.cmp(&a.fan_out))
                .then_with(|| a.key.cmp(&b.key))
        });

        Ok(DiffTouchesResult {
            touched_symbols,
            affected_files,
            unknown_files,
        })
    }

    pub(super) async fn detect_changes(
        &self,
        ctx: &ProjectCtx,
        from_sha: Option<&str>,
        to_sha: Option<&str>,
        changed_files: &[String],
    ) -> Result<DetectedChangesResult, String> {
        use std::collections::{BTreeMap, BTreeSet};

        // Pull the warmed graph + pagerank ranking once. PageRank tiers
        // are computed against the *current* graph rather than a graph
        // rebuilt at the from/to shas — review weight should reflect
        // "what matters now" (per plan §C4).
        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Pre-compute pagerank tiers via quartile bucketing across all
        // *symbol* nodes (file nodes' pagerank is structurally inflated
        // by the `ContainsDefinition` fan-out, so mixing them in
        // produces tier thresholds that flag every method as Low).
        let tier_thresholds = shared::quartile_thresholds(&ranking);
        let pagerank_lookup: BTreeMap<petgraph::graph::NodeIndex, f64> = ranking
            .nodes
            .iter()
            .map(|n| (n.node_index, n.page_rank))
            .collect();

        // Resolve hunks → symbol indices.
        //
        // Line-mode wins when both inputs are present (per request-shape
        // contract). The `git diff` shell-out lives in `djinn_graph`;
        // we go through the helper so the worker binary can reuse it
        // without reaching into the bridge.
        let mut touched_indices: BTreeSet<petgraph::graph::NodeIndex> = BTreeSet::new();
        let line_mode =
            matches!((from_sha, to_sha), (Some(f), Some(t)) if !f.is_empty() && !t.is_empty());
        let (effective_from, effective_to) = if line_mode {
            (
                from_sha.unwrap_or("").to_string(),
                to_sha.unwrap_or("").to_string(),
            )
        } else {
            // Echo back empty strings when in changed_files mode — the
            // wire shape always includes both fields (see `DetectedChangesResult`).
            (String::new(), String::new())
        };

        if line_mode {
            let hunks = djinn_graph::git_diff::diff_changed_ranges(
                std::path::Path::new(&ctx.clone_path),
                &effective_from,
                &effective_to,
            )
            .await
            .map_err(|e| format!("git diff {}..{}: {e}", effective_from, effective_to))?;
            for hunk in &hunks {
                let start = hunk.start_line.max(0) as u32;
                let end = hunk.end_line.unwrap_or(hunk.start_line).max(0) as u32;
                let (start, end) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                let path = std::path::Path::new(&hunk.file);
                for idx in graph.symbols_enclosing(path, start, end) {
                    touched_indices.insert(idx);
                }
            }
        } else {
            // changed_files mode: every symbol inside the listed files
            // is treated as touched. We find them by walking the
            // ContainsDefinition fan-out from the file node.
            use djinn_graph::repo_graph::RepoGraphEdgeKind;
            use petgraph::Direction;
            for file in changed_files {
                let path = std::path::Path::new(file);
                let Some(file_idx) = graph.file_node(path) else {
                    continue;
                };
                for edge in graph.graph().edges_directed(file_idx, Direction::Outgoing) {
                    if matches!(edge.weight().kind, RepoGraphEdgeKind::ContainsDefinition) {
                        touched_indices.insert(edge.target());
                    }
                }
            }
        }

        // Project to the wire type. Skip nodes that are file rather
        // than symbol — `detect_changes` is symbol-centric. (File
        // nodes still surface as symbols' `file_path` field.)
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        let mut touched_symbols: Vec<DetectedTouchedSymbol> = Vec::new();
        for idx in &touched_indices {
            let node = graph.node(*idx);
            if !matches!(node.kind, RepoGraphNodeKind::Symbol) {
                continue;
            }
            // Prefer the node's own `file_path`; fall back to the
            // SCIP file association is already stored there at build.
            let Some(file_pb) = node.file_path.as_ref() else {
                continue;
            };
            let file_path = file_pb.display().to_string();
            // Pull the symbol's enclosing range — we have to re-read it
            // out of the per-file `symbol_ranges` sidecar via the
            // public `symbols_enclosing` API (no direct getter), so
            // probe a max-window to collect the entry.
            let (start_line, end_line) = shared::symbol_range_for_node(&graph, *idx, file_pb);

            let pagerank = pagerank_lookup.get(idx).copied().unwrap_or(0.0);
            let pagerank_tier = shared::bucket_pagerank(&tier_thresholds, pagerank);

            touched_symbols.push(DetectedTouchedSymbol {
                uid: format_node_key(&node.id),
                name: node.display_name.clone(),
                kind: node
                    .symbol_kind
                    .as_ref()
                    .map(|k| format!("{k:?}").to_lowercase())
                    .unwrap_or_else(|| format!("{:?}", node.kind).to_lowercase()),
                file_path,
                start_line,
                end_line,
                pagerank_tier,
                // PR C4 only detects modification — full add/delete
                // classification needs a from-sha graph build (see
                // `ChangeKind` doc).
                change_kind: ChangeKind::Modified,
            });
        }

        // Stable, reviewer-friendly order: tier desc, then file path,
        // then start line.
        touched_symbols.sort_by(|a, b| {
            shared::tier_rank(a.pagerank_tier)
                .cmp(&shared::tier_rank(b.pagerank_tier))
                .then_with(|| a.file_path.cmp(&b.file_path))
                .then_with(|| a.start_line.cmp(&b.start_line))
                .then_with(|| a.uid.cmp(&b.uid))
        });

        // Per-file rollup (sorted by file path; per-file order matches
        // the global order above).
        let mut by_file: BTreeMap<String, Vec<DetectedTouchedSymbol>> = BTreeMap::new();
        for sym in &touched_symbols {
            by_file
                .entry(sym.file_path.clone())
                .or_default()
                .push(sym.clone());
        }

        Ok(DetectedChangesResult {
            from_sha: effective_from,
            to_sha: effective_to,
            touched_symbols,
            by_file,
        })
    }

    pub(super) async fn api_surface(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        module_glob: Option<&str>,
        visibility: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApiSurfaceEntry>, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        use djinn_graph::scip_parser::ScipVisibility;
        use petgraph::Direction;

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let workspace_prefix = shared::active_workspace_prefix(&graph, workspace);

        let vis_filter = match visibility {
            None | Some("public") => Some(ScipVisibility::Public),
            Some("private") => Some(ScipVisibility::Private),
            Some("any") => None,
            Some(other) => {
                return Err(format!(
                    "invalid visibility '{other}': expected 'public', 'private', or 'any'"
                ));
            }
        };
        let module_matcher = match module_glob.filter(|s| !s.is_empty()) {
            Some(pattern) => Some(
                globset::Glob::new(pattern)
                    .map_err(|e| format!("invalid module_glob '{pattern}': {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let mut out: Vec<ApiSurfaceEntry> = Vec::new();
        for node_index in graph.graph().node_indices() {
            let node = graph.node(node_index);
            if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                continue;
            }
            if let Some(prefix) = workspace_prefix.as_deref()
                && !shared::repo_graph_node_matches_workspace(node, prefix)
            {
                continue;
            }
            if let Some(filter) = vis_filter
                && node.visibility != Some(filter)
            {
                continue;
            }
            let file_str = shared::repo_graph_node_file_path(node);
            if let Some(matcher) = &module_matcher {
                let Some(f) = &file_str else { continue };
                if !matcher.is_match(f) {
                    continue;
                }
            }
            let key = format_node_key(&node.id);
            // Self-crate = the SCIP `<tool> <scheme> <crate-name> ...` token.
            let own_crate = node.symbol.as_deref().and_then(shared::scip_crate_name);
            let mut used_outside_crate = false;
            let mut fan_in = 0usize;
            for edge in graph
                .graph()
                .edges_directed(node_index, Direction::Incoming)
            {
                fan_in += 1;
                if !used_outside_crate && own_crate.is_some() {
                    let src = graph.node(edge.source());
                    if let Some(src_sym) = src.symbol.as_deref()
                        && let Some(src_crate) = shared::scip_crate_name(src_sym)
                        && Some(src_crate) != own_crate
                    {
                        used_outside_crate = true;
                    }
                }
            }
            let fan_out = graph
                .graph()
                .edges_directed(node_index, Direction::Outgoing)
                .count();
            let doc_present = !node.documentation.is_empty()
                && node.documentation.iter().any(|l| !l.trim().is_empty());
            out.push(ApiSurfaceEntry {
                key,
                display_name: node.display_name.clone(),
                symbol_kind: node.symbol_kind.as_ref().map(|k| format!("{k:?}")),
                file: file_str,
                visibility: node.visibility.map(|v| v.as_str().to_string()),
                doc_present,
                fan_in,
                fan_out,
                used_outside_crate,
            });
        }
        // Stable order: highest fan-in first, then alpha by key.
        out.sort_by(|a, b| b.fan_in.cmp(&a.fan_in).then_with(|| a.key.cmp(&b.key)));
        out.truncate(limit);
        Ok(out)
    }

    pub(super) async fn boundary_check(
        &self,
        ctx: &ProjectCtx,
        rules: &[BoundaryRule],
        level: &str,
    ) -> Result<Vec<BoundaryViolation>, String> {
        if level == "crate" {
            return self.boundary_check_crate(ctx, rules).await;
        }

        use globset::Glob;

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Every submitted rule is treated as a forbidden edge.
        let compiled: Vec<(usize, globset::GlobMatcher, globset::GlobMatcher)> = rules
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let from = Glob::new(&r.from_glob)
                    .map_err(|e| format!("rule[{i}].from_glob '{}': {e}", r.from_glob))?
                    .compile_matcher();
                let to = Glob::new(&r.to_glob)
                    .map_err(|e| format!("rule[{i}].to_glob '{}': {e}", r.to_glob))?
                    .compile_matcher();
                Ok::<_, String>((i, from, to))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if compiled.is_empty() {
            return Ok(Vec::new());
        }

        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        let mut violations: Vec<BoundaryViolation> = Vec::new();
        for edge_ref in graph.graph().edge_references() {
            let src_node = graph.node(edge_ref.source());
            let dst_node = graph.node(edge_ref.target());
            let src_key = format_node_key(&src_node.id);
            let dst_key = format_node_key(&dst_node.id);
            // Skip the edge if either endpoint is filtered by exclusions.
            if exclusions.excludes(
                &src_key,
                src_node
                    .file_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .as_deref(),
                &src_node.display_name,
            ) || exclusions.excludes(
                &dst_key,
                dst_node
                    .file_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .as_deref(),
                &dst_node.display_name,
            ) {
                continue;
            }
            let src_match_target = src_node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| src_node.display_name.clone());
            let dst_match_target = dst_node
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| dst_node.display_name.clone());

            for (rule_index, from_m, to_m) in &compiled {
                if from_m.is_match(&src_match_target) && to_m.is_match(&dst_match_target) {
                    violations.push(BoundaryViolation {
                        rule_index: *rule_index,
                        from_key: src_key.clone(),
                        to_key: dst_key.clone(),
                        edge_kind: format!("{:?}", edge_ref.weight().kind),
                        from_file: src_node.file_path.as_ref().map(|p| p.display().to_string()),
                        to_file: dst_node.file_path.as_ref().map(|p| p.display().to_string()),
                        witness_path: Some(vec![src_key.clone(), dst_key.clone()]),
                    });
                }
            }
        }
        Ok(violations)
    }

    /// Crate-level boundary check: loads the warmed canonical graph +
    /// crate map, builds the `CrateGraph`, and matches each
    /// `CrateEdge` against the supplied rules.
    ///
    /// For crate-level matching, the glob patterns are matched against
    /// the bare crate name (e.g. `djinn-agent`). A glob like
    /// `**/djinn-agent/**` is normalised by stripping path-prefix/suffix
    /// wildcards so that the literal `djinn-agent` component is what the
    /// matcher tests against.
    async fn boundary_check_crate(
        &self,
        ctx: &ProjectCtx,
        rules: &[BoundaryRule],
    ) -> Result<Vec<BoundaryViolation>, String> {
        let (_project_root, index_tree_path) =
            djinn_graph::canonical_graph::normalize_graph_query_paths(&ctx.clone_path);

        let (graph, crate_map) = {
            let cache = djinn_graph::canonical_graph::GRAPH_CACHE.read().await;
            let cached = cache
                .as_ref()
                .filter(|c| c.project_path == index_tree_path)
                .ok_or("Graph not warmed for crate-level boundary check")?;
            (cached.graph.clone(), cached.crate_map.clone())
        };

        if crate_map.is_empty() {
            return Err("No crate mapping available".into());
        }

        let crate_graph = djinn_graph::repo_graph::build_crate_graph(&graph, crate_map.as_ref());
        Ok(crate_boundary_violations(&crate_graph.edges, rules))
    }

    pub(super) async fn hotspots(
        &self,
        ctx: &ProjectCtx,
        window_days: u32,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HotspotEntry>, String> {
        use djinn_db::CommitFileChangeRepository;
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        use std::collections::BTreeMap;

        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Fetch windowed churn from the DB-backed commit_file_changes table.
        // This removes the live git checkout dependency from hotspots.
        let days = window_days.clamp(1, 365);
        let since = shared::since_days_to_cutoff(Some(days));
        let churn_repo = CommitFileChangeRepository::new(self.state.db().clone());
        let churn_rows = churn_repo
            .churn(&ctx.id, 500, since.as_deref())
            .await
            .map_err(|e| format!("hotspots churn lookup: {e}"))?;

        let mut churn: BTreeMap<String, usize> = BTreeMap::new();
        for row in &churn_rows {
            if row.commit_count < 0 {
                continue;
            }
            churn.insert(row.file_path.clone(), row.commit_count as usize);
        }

        // If the selected window has no churn, perform an all-time availability
        // check. All-time rows present means valid zero churn for the window;
        // no all-time rows means churn data has not been harvested yet.
        if churn.is_empty() {
            let all_time_rows = churn_repo
                .churn(&ctx.id, 1, None)
                .await
                .map_err(|e| format!("hotspots churn lookup: {e}"))?;
            if all_time_rows.is_empty() {
                return Err(
                    "hotspots churn unavailable: no commit_file_changes rows for project; run mirror churn harvest".into(),
                );
            }
            return Ok(Vec::new());
        }

        // Build per-file centrality (Σ PR of owned symbols) and top-symbol list.
        // `RepoGraphRanking` is pagerank-sorted, so we can walk it directly
        // to pick up the highest-PR symbols per file.
        let mut per_file_centrality: BTreeMap<String, f64> = BTreeMap::new();
        let mut per_file_top: BTreeMap<String, Vec<(f64, String)>> = BTreeMap::new();
        for ranked in &ranking.nodes {
            if ranked.kind != RepoGraphNodeKind::Symbol {
                continue;
            }
            let node = graph.node(ranked.node_index);
            let Some(file) = node.file_path.as_ref().map(|p| p.display().to_string()) else {
                continue;
            };
            *per_file_centrality.entry(file.clone()).or_insert(0.0) += ranked.page_rank;
            let top = per_file_top.entry(file).or_default();
            if top.len() < 3 {
                top.push((ranked.page_rank, node.display_name.clone()));
            }
        }

        let file_matcher = match file_glob.filter(|s| !s.is_empty()) {
            Some(pattern) => Some(
                globset::Glob::new(pattern)
                    .map_err(|e| format!("invalid file_glob '{pattern}': {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        let mut out: Vec<HotspotEntry> = Vec::new();
        for (file, count) in churn {
            if let Some(matcher) = &file_matcher
                && !matcher.is_match(&file)
            {
                continue;
            }
            // Apply GraphExclusions — we key on file path for discovery,
            // and use the same path as display_name for the key check.
            if exclusions.excludes(&file, Some(&file), &file) {
                continue;
            }
            let centrality = per_file_centrality.get(&file).copied().unwrap_or(0.0);
            let top_symbols = per_file_top
                .get(&file)
                .map(|v| v.iter().map(|(_, n)| n.clone()).collect())
                .unwrap_or_default();
            out.push(HotspotEntry {
                file,
                churn: count,
                centrality,
                composite_score: count as f64 * centrality,
                top_symbols,
            });
        }
        out.sort_by(|a, b| {
            b.composite_score
                .partial_cmp(&a.composite_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.file.cmp(&b.file))
        });
        out.truncate(limit);
        Ok(out)
    }

    pub(super) async fn complexity(
        &self,
        ctx: &ProjectCtx,
        target: &str,
        sort_by: &str,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<djinn_control_plane::bridge::ComplexityResult, String> {
        use djinn_control_plane::bridge::{ComplexityResult, FunctionComplexityEntry};
        use djinn_graph::scip_parser::ScipSymbolKind;

        // Wire-shape contract: validate up front so an invalid `target`
        // or `sort_by` doesn't quietly degrade to a default ranking.
        match target {
            "functions" | "files" => {}
            other => {
                return Err(format!(
                    "invalid target '{other}': expected 'functions' or 'files'"
                ));
            }
        }
        match sort_by {
            "cognitive" | "cyclomatic" | "nloc" | "max_nesting" | "param_count" => {}
            other => {
                return Err(format!(
                    "invalid sort_by '{other}': expected 'cognitive', 'cyclomatic', \
                     'nloc', 'max_nesting', or 'param_count'"
                ));
            }
        }

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        let file_matcher = match file_glob.filter(|s| !s.is_empty()) {
            Some(pattern) => Some(
                globset::Glob::new(pattern)
                    .map_err(|e| format!("invalid file_glob '{pattern}': {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;
        let limit = limit.clamp(1, 200);

        // Walk every function-like node carrying a complexity payload
        // and collect the per-function tuple. The walker only emits
        // metrics for Function/Method/Constructor symbols (iter 26), so
        // the symbol_kind check is defensive — but it keeps the contract
        // explicit so a future widening on the walker side doesn't
        // silently broaden this op's surface.
        let mut functions: Vec<FunctionComplexityEntry> = Vec::new();
        for node_index in graph.graph().node_indices() {
            let node = graph.node(node_index);
            let Some(metrics) = node.complexity else {
                continue;
            };
            let Some(symbol_kind) = node.symbol_kind.as_ref() else {
                continue;
            };
            if !matches!(
                symbol_kind,
                ScipSymbolKind::Function | ScipSymbolKind::Method | ScipSymbolKind::Constructor
            ) {
                continue;
            }
            let Some(file_path) = node.file_path.as_ref() else {
                continue;
            };
            let file_str = file_path.display().to_string();
            if let Some(matcher) = &file_matcher
                && !matcher.is_match(&file_str)
            {
                continue;
            }
            let key = format_node_key(&node.id);
            if exclusions.excludes(&key, Some(&file_str), &node.display_name) {
                continue;
            }
            let (start_line, end_line) = graph
                .range_for_node(node_index, file_path)
                .unwrap_or((0, 0));
            functions.push(FunctionComplexityEntry {
                key,
                display_name: node.display_name.clone(),
                file: file_str,
                start_line,
                end_line,
                metrics: super::refactor::complexity_metrics_to_wire(metrics),
            });
        }

        match target {
            "functions" => {
                super::refactor::sort_function_complexity_entries(&mut functions, sort_by);
                functions.truncate(limit);
                Ok(ComplexityResult::Functions(functions))
            }
            // "files"
            _ => {
                let mut files = super::refactor::aggregate_files_complexity(&functions, sort_by);
                files.truncate(limit);
                Ok(ComplexityResult::Files(files))
            }
        }
    }

    pub(super) async fn refactor_candidates(
        &self,
        ctx: &ProjectCtx,
        since_days: Option<u32>,
        file_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RefactorCandidate>, String> {
        use djinn_db::CommitFileChangeRepository;
        use djinn_graph::scip_parser::ScipSymbolKind;
        use std::collections::HashMap;
        use std::path::PathBuf;

        // Iter 29: composite refactor-priority op. Joins three signals
        // (cognitive complexity, file-level churn, PageRank) on the
        // function-like canonical-graph nodes and returns a top-`limit`
        // ranking by mean z-score. See [`compute_refactor_candidates`]
        // for the pure ranking logic.
        let since_days_window = since_days.unwrap_or(90).clamp(1, 365);
        let limit = limit.clamp(1, 200);

        let (graph, ranking, _sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        let file_matcher = match file_glob.filter(|s| !s.is_empty()) {
            Some(pattern) => Some(
                globset::Glob::new(pattern)
                    .map_err(|e| format!("invalid file_glob '{pattern}': {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        // Build a `node_index → page_rank` lookup once. The
        // canonical ranking is pagerank-sorted, so a HashMap pass is
        // O(N) once and O(1) per candidate.
        let mut pagerank_for: HashMap<petgraph::graph::NodeIndex, f64> =
            HashMap::with_capacity(ranking.nodes.len());
        for ranked in &ranking.nodes {
            pagerank_for.insert(ranked.node_index, ranked.page_rank);
        }

        // Proposal qoxm: per-file cross-module co-change pressure from the
        // co-change sidecar. Sum each file's coupling score to partners in a
        // *different* crate/module; that summed score becomes the fourth
        // refactor-ranking axis, so a config file that changes lockstep with a
        // source file in another crate scores higher.
        let mut cross_module_cc: HashMap<PathBuf, f64> = HashMap::new();
        for cc in graph.cochange_edges() {
            let (Some(fa), Some(fb)) = (
                graph.node(cc.source).file_path.clone(),
                graph.node(cc.target).file_path.clone(),
            ) else {
                continue;
            };
            if super::refactor::module_key(&fa.to_string_lossy())
                == super::refactor::module_key(&fb.to_string_lossy())
            {
                continue;
            }
            *cross_module_cc.entry(fa).or_insert(0.0) += cc.confidence;
            *cross_module_cc.entry(fb).or_insert(0.0) += cc.confidence;
        }

        // Walk every function-like node carrying complexity. Skip the
        // same node-classes the iter-28 `complexity` op skips, plus
        // the test/external flags (the spec calls out filtering tests
        // / externals when those flags are present; our nodes always
        // have them).
        let mut candidate_inputs: Vec<super::refactor::RefactorCandidateInput> = Vec::new();
        for node_index in graph.graph().node_indices() {
            let node = graph.node(node_index);
            if node.is_external || node.is_test {
                continue;
            }
            let Some(metrics) = node.complexity else {
                continue;
            };
            let Some(symbol_kind) = node.symbol_kind.as_ref() else {
                continue;
            };
            if !matches!(
                symbol_kind,
                ScipSymbolKind::Function | ScipSymbolKind::Method | ScipSymbolKind::Constructor
            ) {
                continue;
            }
            let Some(file_path) = node.file_path.as_ref() else {
                continue;
            };
            let file_str = file_path.display().to_string();
            if let Some(matcher) = &file_matcher
                && !matcher.is_match(&file_str)
            {
                continue;
            }
            let key = format_node_key(&node.id);
            if exclusions.excludes(&key, Some(&file_str), &node.display_name) {
                continue;
            }
            let (start_line, end_line) = graph
                .range_for_node(node_index, file_path)
                .unwrap_or((0, 0));
            let page_rank = pagerank_for.get(&node_index).copied().unwrap_or(0.0);
            candidate_inputs.push(super::refactor::RefactorCandidateInput {
                key,
                display_name: node.display_name.clone(),
                file: file_str,
                start_line,
                end_line,
                cognitive: metrics.cognitive,
                cyclomatic: metrics.cyclomatic,
                page_rank,
                cross_module_cochange: cross_module_cc
                    .get(file_path)
                    .copied()
                    .unwrap_or(0.0),
            });
        }

        if candidate_inputs.is_empty() {
            return Ok(Vec::new());
        }

        // Fetch churn — file-level commit counts over the window.
        // The repository clamps `limit` to 500; for refactor_candidates
        // we want every relevant file's count, so we pass the upper
        // bound. Files outside the top 500 churned default to 0
        // (which yields a negative z_churn — correct for "this file
        // changes less than average").
        let since = shared::since_days_to_cutoff(Some(since_days_window));
        let churn_repo = CommitFileChangeRepository::new(self.state.db().clone());
        let churn_rows = churn_repo
            .churn(&ctx.id, 500, since.as_deref())
            .await
            .map_err(|e| format!("refactor_candidates churn lookup: {e}"))?;
        let mut churn_map: HashMap<PathBuf, u32> = HashMap::with_capacity(churn_rows.len());
        for row in churn_rows {
            let count = row.commit_count.max(0) as u32;
            churn_map.insert(PathBuf::from(row.file_path), count);
        }

        Ok(super::refactor::compute_refactor_candidates(
            &candidate_inputs,
            &churn_map,
            limit,
        ))
    }

    pub(super) async fn metrics_at(&self, ctx: &ProjectCtx) -> Result<MetricsAtResult, String> {
        use djinn_graph::repo_graph::RepoGraphNodeKind;
        use djinn_graph::scip_parser::ScipVisibility;
        use petgraph::Direction;
        use std::collections::BTreeMap;

        let (graph, _ranking, sccs) = djinn_graph::canonical_graph::load_canonical_graph(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        // Filter the node-set once; every downstream count uses it.
        let mut kept: Vec<petgraph::graph::NodeIndex> = Vec::new();
        let mut total_degree: Vec<usize> = Vec::new();
        let mut public_kept: Vec<petgraph::graph::NodeIndex> = Vec::new();
        for node_index in graph.graph().node_indices() {
            let node = graph.node(node_index);
            let file = node.file_path.as_ref().map(|p| p.display().to_string());
            let key = format_node_key(&node.id);
            if exclusions.excludes(&key, file.as_deref(), &node.display_name) {
                continue;
            }
            if node.is_route_or_tool() || graph.is_singleton_route_without_consumers(node_index) {
                continue;
            }
            kept.push(node_index);
            let td = graph
                .graph()
                .edges_directed(node_index, Direction::Incoming)
                .count()
                + graph
                    .graph()
                    .edges_directed(node_index, Direction::Outgoing)
                    .count();
            total_degree.push(td);
            if node.kind == RepoGraphNodeKind::Symbol
                && node.visibility == Some(ScipVisibility::Public)
            {
                public_kept.push(node_index);
            }
        }

        // p95 of total_degree across kept nodes → god-object floor.
        let mut td_sorted = total_degree.clone();
        td_sorted.sort_unstable();
        let p95_floor = if td_sorted.is_empty() {
            0
        } else {
            let idx = ((td_sorted.len() as f64) * 0.95).ceil() as usize;
            td_sorted[idx.saturating_sub(1).min(td_sorted.len() - 1)]
        };
        let god_object_count = if p95_floor == 0 {
            0
        } else {
            total_degree.iter().filter(|d| **d >= p95_floor).count()
        };

        // Edge count over kept nodes only — both endpoints must survive.
        let kept_set: std::collections::HashSet<petgraph::graph::NodeIndex> =
            kept.iter().copied().collect();
        let edge_count = graph
            .graph()
            .edge_references()
            .filter(|e| kept_set.contains(&e.source()) && kept_set.contains(&e.target()))
            .count();

        // Cycles — exclude SCCs whose kept members drop below size 2.
        // We surface three distinct cycle counts so the user can tell
        // which scope the number refers to:
        //   - cycle_count        : all-scopes raw count (includes
        //                          tautological file↔symbol pairs)
        //   - cycle_count_symbol_only : symbol-only subgraph (matches
        //                          the `cycles` op default kind_filter)
        //   - cycle_count_file_only   : file-only subgraph (file→file
        //                          import cycles; non-DAG languages)
        let mut cycles_by_size_histogram: BTreeMap<usize, usize> = BTreeMap::new();
        let mut cycle_count = 0usize;
        for component in sccs.full.iter() {
            let surviving = component
                .iter()
                .filter(|idx| kept_set.contains(idx))
                .count();
            if surviving >= 2 {
                cycle_count += 1;
                *cycles_by_size_histogram.entry(surviving).or_insert(0) += 1;
            }
        }
        let cycle_count_symbol_only = sccs
            .symbol
            .iter()
            .filter(|c| c.iter().filter(|idx| kept_set.contains(idx)).count() >= 2)
            .count();
        let cycle_count_file_only = sccs
            .file
            .iter()
            .filter(|c| c.iter().filter(|idx| kept_set.contains(idx)).count() >= 2)
            .count();

        // Orphan count — defer to graph.orphans(), then strip via exclusions.
        let orphans = graph.orphans(None, None, usize::MAX);
        let orphan_count = orphans
            .into_iter()
            .filter(|idx| kept_set.contains(idx))
            .count();

        let public_api_count = public_kept.len();
        let docs_present = public_kept
            .iter()
            .filter(|idx| {
                let n = graph.node(**idx);
                !n.documentation.is_empty() && n.documentation.iter().any(|l| !l.trim().is_empty())
            })
            .count();
        let doc_coverage_pct = if public_api_count == 0 {
            0.0
        } else {
            100.0 * (docs_present as f64) / (public_api_count as f64)
        };

        // Resolve the pinned commit via repo_graph_cache. Best-effort;
        // fall back to an empty string if the lookup fails.
        let mut pinned = String::new();
        use djinn_db::RepoGraphCacheRepository;
        let cache_repo = RepoGraphCacheRepository::new(self.state.db().clone());
        if let Ok(Some(row)) = cache_repo.latest_for_project(&ctx.id).await {
            pinned = row.commit_sha;
        }

        Ok(MetricsAtResult {
            commit: pinned,
            node_count: kept.len(),
            edge_count,
            cycle_count,
            cycle_count_symbol_only,
            cycle_count_file_only,
            cycles_by_size_histogram,
            god_object_count,
            orphan_count,
            public_api_count,
            doc_coverage_pct,
        })
    }
}

/// Normalise a glob intended to match a crate name.
///
/// Crate-level boundary rules are typically written as path-style globs
/// (e.g. `**/djinn-agent/**`) because they share the same format as
/// file-level rules. When matching against bare crate names
/// (e.g. `djinn-agent`), we strip leading/trailing `**/`, `*/`, and `/**`
/// wildcards so the literal component is what remains.
///
/// Examples:
/// - `**/djinn-agent/**` → `djinn-agent`
/// - `**/djinn-*/**`     → `djinn-*`
/// - `djinn-agent`        → `djinn-agent`
/// - `*`                  → `*`
fn normalise_crate_glob(glob: &str) -> String {
    let mut s = glob.trim().to_string();
    // Strip leading wildcard segments: `**/`, `*/`, or just `**`.
    while s.starts_with("**/") || s.starts_with("*/") {
        s = s[s.find('/').unwrap() + 1..].to_string();
    }
    if s == "**" || s == "*" {
        return s;
    }
    // Strip trailing wildcard segments: `/**` or `/*`.
    if s.ends_with("/**") {
        s = s[..s.len() - 3].to_string();
    } else if s.ends_with("/*") {
        s = s[..s.len() - 2].to_string();
    }
    s
}

/// Match compiled boundary rules against crate-level edges.
///
/// Each rule's `from_glob`/`to_glob` are normalised via
/// [`normalise_crate_glob`] so that path-style globs like
/// `**/djinn-agent/**` match against the bare crate name
/// `djinn-agent`. Every rule is treated as a forbidden edge; matching
/// edges are reported as [`BoundaryViolation`] entries with
/// `edge_kind = "crate_depends_on"`.
fn crate_boundary_violations(
    edges: &[djinn_graph::repo_graph::CrateEdge],
    rules: &[BoundaryRule],
) -> Vec<BoundaryViolation> {
    use globset::Glob;

    let compiled: Vec<(usize, globset::GlobMatcher, globset::GlobMatcher)> = rules
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let from = Glob::new(&normalise_crate_glob(&r.from_glob))
                .map_err(|e| format!("rule[{i}].from_glob '{}': {e}", r.from_glob))
                .ok()?
                .compile_matcher();
            let to = Glob::new(&normalise_crate_glob(&r.to_glob))
                .map_err(|e| format!("rule[{i}].to_glob '{}': {e}", r.to_glob))
                .ok()?
                .compile_matcher();
            Some((i, from, to))
        })
        .collect();

    let mut violations = Vec::new();
    for edge in edges {
        for (rule_index, from_m, to_m) in &compiled {
            if from_m.is_match(&edge.source) && to_m.is_match(&edge.target) {
                violations.push(BoundaryViolation {
                    rule_index: *rule_index,
                    from_key: edge.source.clone(),
                    to_key: edge.target.clone(),
                    edge_kind: "crate_depends_on".to_string(),
                    from_file: None,
                    to_file: None,
                    witness_path: Some(vec![edge.source.clone(), edge.target.clone()]),
                });
            }
        }
    }
    violations
}

#[cfg(test)]
mod boundary_check_crate_tests {
    use super::*;
    use djinn_graph::repo_graph::CrateEdge;

    #[test]
    fn test_normalise_crate_glob() {
        assert_eq!(normalise_crate_glob("**/djinn-agent/**"), "djinn-agent");
        assert_eq!(normalise_crate_glob("**/djinn-agent"), "djinn-agent");
        assert_eq!(normalise_crate_glob("djinn-agent/**"), "djinn-agent");
        assert_eq!(normalise_crate_glob("djinn-agent"), "djinn-agent");
        assert_eq!(normalise_crate_glob("**/djinn-*/**"), "djinn-*");
        assert_eq!(normalise_crate_glob("*"), "*");
        assert_eq!(normalise_crate_glob("**"), "**");
        assert_eq!(normalise_crate_glob("*/djinn-agent"), "djinn-agent");
    }

    /// boundary_check with level=crate returns violations when rules
    /// match crate-level edges.
    #[test]
    fn crate_boundary_violations_returns_matches() {
        let edges = vec![
            CrateEdge {
                source: "crate-b".to_string(),
                target: "crate-a".to_string(),
                weight: 1.0,
                edge_count: 3,
            },
            CrateEdge {
                source: "djinn-agent".to_string(),
                target: "djinn-control-plane".to_string(),
                weight: 5.0,
                edge_count: 10,
            },
        ];

        // Rule: djinn-agent must not depend on djinn-control-plane.
        let rules = vec![BoundaryRule {
            from_glob: "**/djinn-agent/**".to_string(),
            to_glob: "**/djinn-control-plane/**".to_string(),
            description: Some("agent must not depend on control-plane".to_string()),
        }];

        let violations = crate_boundary_violations(&edges, &rules);
        assert_eq!(
            violations.len(),
            1,
            "should flag exactly the agent → control-plane edge"
        );
        assert_eq!(violations[0].rule_index, 0);
        assert_eq!(violations[0].from_key, "djinn-agent");
        assert_eq!(violations[0].to_key, "djinn-control-plane");
        assert_eq!(violations[0].edge_kind, "crate_depends_on");
        assert!(violations[0].from_file.is_none());
        assert!(violations[0].to_file.is_none());
        assert_eq!(
            violations[0].witness_path.as_deref(),
            Some(&["djinn-agent".to_string(), "djinn-control-plane".to_string()][..])
        );
    }

    /// boundary_check with level=crate returns empty when no
    /// violations (rules don't match any edge).
    #[test]
    fn crate_boundary_violations_returns_empty_when_no_match() {
        let edges = vec![CrateEdge {
            source: "crate-a".to_string(),
            target: "crate-b".to_string(),
            weight: 1.0,
            edge_count: 2,
        }];

        // Rule: crate-c must not depend on crate-d — doesn't match.
        let rules = vec![BoundaryRule {
            from_glob: "**/crate-c/**".to_string(),
            to_glob: "**/crate-d/**".to_string(),
            description: None,
        }];

        let violations = crate_boundary_violations(&edges, &rules);
        assert!(
            violations.is_empty(),
            "no edges should match the rule; got {violations:?}"
        );
    }
}
