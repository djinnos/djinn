use super::*;

impl RepoGraphBridge {
    /// Symbols with zero incoming edges from the entry-point set.
    ///
    /// PR F1 cut-over: entry-point detection now lives in
    /// [`djinn_graph::entry_points`] and stamps `EntryPointOf` edges at
    /// build time. This method just asks "does the symbol have any
    /// incoming `EntryPointOf` edge?" — the per-language test / main /
    /// HTTP-route heuristics are handled centrally by the detector.
    /// "Crate root re-export surface" is still inferred locally from
    /// the file path (`**/src/lib.rs` or `**/src/main.rs`) so a
    /// `pub fn` re-exported from the crate root isn't flagged dead just
    /// because no in-tree caller hits it.
    pub(super) async fn dead_symbols(
        &self,
        ctx: &ProjectCtx,
        confidence: &str,
        limit: usize,
    ) -> Result<Vec<DeadSymbolEntry>, String> {
        use djinn_graph::repo_graph::{RepoGraphEdgeKind, RepoGraphNodeKind};
        use djinn_graph::scip_parser::ScipVisibility;
        use petgraph::Direction;
        use std::collections::HashSet;

        if !matches!(confidence, "high" | "med" | "low") {
            return Err(format!(
                "invalid confidence '{confidence}': expected 'high', 'med', or 'low'"
            ));
        }

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        // Crate-root public-surface heuristic still runs locally — the
        // detector doesn't tag every public symbol re-exported from
        // `src/lib.rs` because that would over-fire for non-library
        // crates. We layer it in here so a `pub fn` at the crate root
        // is still considered an entry point.
        let crate_root_lib = globset::Glob::new("**/src/lib.rs")
            .map_err(|e| e.to_string())?
            .compile_matcher();
        let crate_root_main = globset::Glob::new("**/src/main.rs")
            .map_err(|e| e.to_string())?
            .compile_matcher();

        let mut entry_set: HashSet<petgraph::graph::NodeIndex> = HashSet::new();
        for idx in graph.graph().node_indices() {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                continue;
            }
            // PR F1: any node with an incoming `EntryPointOf` edge is
            // an entry point.
            let has_entry_point_edge = graph
                .graph()
                .edges_directed(idx, Direction::Incoming)
                .any(|e| e.weight().kind == RepoGraphEdgeKind::EntryPointOf);
            if has_entry_point_edge {
                entry_set.insert(idx);
                continue;
            }
            // Crate-root public-surface fallback (file-path heuristic
            // not covered by the detector).
            let file_str = node.file_path.as_ref().map(|p| p.display().to_string());
            let crate_root_public = node.visibility == Some(ScipVisibility::Public)
                && file_str
                    .as_deref()
                    .map(|f| crate_root_lib.is_match(f) || crate_root_main.is_match(f))
                    .unwrap_or(false);
            if crate_root_public {
                entry_set.insert(idx);
            }
        }

        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        let mut out: Vec<DeadSymbolEntry> = Vec::new();
        for idx in graph.graph().node_indices() {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                continue;
            }
            if entry_set.contains(&idx) {
                continue;
            }

            let mut has_any_incoming = false;
            let mut has_relationship_ref_or_impl = false;
            let mut has_relationship_impl = false;
            for edge in graph.graph().edges_directed(idx, Direction::Incoming) {
                match edge.weight().kind {
                    RepoGraphEdgeKind::ContainsDefinition | RepoGraphEdgeKind::DeclaredInFile => {}
                    // PR F1: `EntryPointOf` is metadata, not a caller
                    // signal. Symbols with this edge already short-
                    // circuit above via `entry_set`; non-entry symbols
                    // shouldn't carry one, but skip defensively.
                    RepoGraphEdgeKind::EntryPointOf => {}
                    RepoGraphEdgeKind::Implements => {
                        has_any_incoming = true;
                        has_relationship_ref_or_impl = true;
                        has_relationship_impl = true;
                    }
                    RepoGraphEdgeKind::Extends => {
                        has_any_incoming = true;
                        has_relationship_ref_or_impl = true;
                    }
                    _ => {
                        has_any_incoming = true;
                    }
                }
            }
            // Tiers (strictest → loosest):
            // * `high` — exclude anything with an incoming impl *or*
            //   relationship-ref edge (they're likely dyn-dispatch callers).
            // * `med`  — exclude anything with an incoming impl edge.
            // * `low`  — keep any symbol with zero incoming "real" edges,
            //   regardless of relationship hints.
            let keep = match confidence {
                "low" => !has_any_incoming,
                "med" => !has_any_incoming && !has_relationship_impl,
                "high" => !has_any_incoming && !has_relationship_ref_or_impl,
                _ => unreachable!(),
            };
            if !keep {
                continue;
            }

            let key = format_node_key(&node.id);
            let file = node.file_path.as_ref().map(|p| p.display().to_string());
            if exclusions.excludes(&key, file.as_deref(), &node.display_name) {
                continue;
            }
            out.push(DeadSymbolEntry {
                key,
                display_name: node.display_name.clone(),
                symbol_kind: node.symbol_kind.as_ref().map(|k| format!("{k:?}")),
                file,
                visibility: node.visibility.map(|v| v.as_str().to_string()),
                confidence: confidence.to_string(),
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub(super) async fn deprecated_callers(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
    ) -> Result<Vec<DeprecatedHit>, String> {
        use djinn_graph::repo_graph::{RepoGraphEdgeKind, RepoGraphNodeKind};
        use petgraph::Direction;

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let exclusions = self.state.mcp_state_graph_exclusions(&ctx.id).await;

        let mut out: Vec<DeprecatedHit> = Vec::new();
        for idx in graph.graph().node_indices() {
            let node = graph.node(idx);
            if node.kind != RepoGraphNodeKind::Symbol || node.is_external {
                continue;
            }
            // v1: text-scan signature + documentation for deprecation markers.
            // The SCIP parser does not yet set an explicit `deprecated` flag —
            // extending `ScipSymbol` to carry one is left for a later pass.
            if !shared::is_deprecated_text(node.signature.as_deref(), &node.documentation) {
                continue;
            }
            let dep_key = format_node_key(&node.id);
            let dep_file = node.file_path.as_ref().map(|p| p.display().to_string());
            if exclusions.excludes(&dep_key, dep_file.as_deref(), &node.display_name) {
                continue;
            }
            let mut callers: Vec<CallerRef> = Vec::new();
            for edge in graph.graph().edges_directed(idx, Direction::Incoming) {
                match edge.weight().kind {
                    // PR A3: `Reads` / `Writes` are split-out variants of the
                    // legacy `SymbolReference` edge; they still count as
                    // "this caller touches the deprecated symbol".
                    RepoGraphEdgeKind::SymbolReference
                    | RepoGraphEdgeKind::Reads
                    | RepoGraphEdgeKind::Writes
                    | RepoGraphEdgeKind::Extends
                    | RepoGraphEdgeKind::FileReference => {
                        let src = graph.node(edge.source());
                        let src_key = format_node_key(&src.id);
                        let src_file = src.file_path.as_ref().map(|p| p.display().to_string());
                        if exclusions.excludes(&src_key, src_file.as_deref(), &src.display_name) {
                            continue;
                        }
                        callers.push(CallerRef {
                            key: src_key,
                            display_name: src.display_name.clone(),
                            file: src_file,
                        });
                    }
                    _ => {}
                }
            }
            out.push(DeprecatedHit {
                deprecated_symbol: dep_key,
                deprecated_display_name: node.display_name.clone(),
                deprecated_file: dep_file,
                callers,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub(super) async fn touches_hot_path(
        &self,
        ctx: &ProjectCtx,
        workspace: Option<&str>,
        seed_entries: &[String],
        seed_sinks: &[String],
        symbols: &[String],
    ) -> Result<Vec<HotPathHit>, String> {
        use std::collections::{HashMap, HashSet};

        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;

        if seed_entries.is_empty() || seed_sinks.is_empty() || symbols.is_empty() {
            return Ok(Vec::new());
        }

        // Resolve entry/sink seeds inside the requested workspace only.
        // The queried symbols remain unscoped so callers can ask whether
        // cross-workspace nodes sit on any in-workspace entry→sink path.
        let resolve_seed = |key: &str| -> Option<petgraph::graph::NodeIndex> {
            shared::resolve_node_or_err_for_workspace_seed(&graph, key, workspace).ok()
        };
        let resolve_symbol = |key: &str| -> Option<petgraph::graph::NodeIndex> {
            resolve_node_or_err(&graph, key).ok()
        };
        let entry_ix: Vec<petgraph::graph::NodeIndex> = seed_entries
            .iter()
            .filter_map(|k| resolve_seed(k))
            .collect();
        let sink_ix: Vec<petgraph::graph::NodeIndex> =
            seed_sinks.iter().filter_map(|k| resolve_seed(k)).collect();

        let pair_cap = 400usize;
        let total_pairs = entry_ix.len() * sink_ix.len();
        let truncated = total_pairs > pair_cap;
        if truncated {
            tracing::warn!(
                project_id = %ctx.id,
                total_pairs,
                cap = pair_cap,
                "touches_hot_path: pair count exceeds cap; truncating",
            );
        }

        // Precompute shortest paths, capping at pair_cap. Paths collected
        // as Vec<NodeIndex> for membership tests, and cached as formatted
        // keys for the first `example_path` hit per symbol.
        let mut paths: Vec<Vec<petgraph::graph::NodeIndex>> = Vec::new();
        let mut count = 0usize;
        'outer: for &e in &entry_ix {
            for &s in &sink_ix {
                if count >= pair_cap {
                    break 'outer;
                }
                count += 1;
                if let Some(p) = graph.shortest_path(e, s, None) {
                    paths.push(p);
                }
            }
        }

        // Build a lookup symbol-key → NodeIndex, then walk the path
        // list once per queried symbol (O(Q × P × |path|), P ≤ 400).
        let mut queried: HashMap<String, petgraph::graph::NodeIndex> = HashMap::new();
        for k in symbols {
            if let Some(idx) = resolve_symbol(k) {
                queried.insert(k.clone(), idx);
            }
        }

        let mut out: Vec<HotPathHit> = Vec::new();
        for k in symbols {
            let Some(idx) = queried.get(k).copied() else {
                out.push(HotPathHit {
                    symbol: k.clone(),
                    on_path_count: 0,
                    example_path: None,
                });
                continue;
            };
            let mut hits = 0usize;
            let mut example: Option<Vec<String>> = None;
            for path in &paths {
                let set: HashSet<petgraph::graph::NodeIndex> = path.iter().copied().collect();
                if set.contains(&idx) {
                    hits += 1;
                    if example.is_none() {
                        example = Some(
                            path.iter()
                                .map(|i| format_node_key(&graph.node(*i).id))
                                .collect(),
                        );
                    }
                }
            }
            out.push(HotPathHit {
                symbol: k.clone(),
                on_path_count: hits,
                example_path: example,
            });
        }
        Ok(out)
    }

    pub(super) async fn coupling(
        &self,
        ctx: &ProjectCtx,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingEntry>, String> {
        use djinn_control_plane::bridge::CouplingEntry;
        use djinn_db::CommitFileChangeRepository;

        let repo = CommitFileChangeRepository::new(self.state.db().clone());
        let rows = repo
            .top_coupled(&ctx.id, file_path, limit.max(1))
            .await
            .map_err(|e| format!("coupling lookup: {e}"))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let samples: Vec<String> = row
                .supporting_commit_samples()
                .into_iter()
                .take(3)
                .collect();
            out.push(CouplingEntry {
                file_path: row.file_path,
                co_edit_count: row.co_edit_count.max(0) as usize,
                last_co_edit: row.last_co_edit,
                supporting_commit_samples: samples,
            });
        }
        Ok(out)
    }

    pub(super) async fn churn(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
    ) -> Result<Vec<djinn_control_plane::bridge::ChurnEntry>, String> {
        use djinn_control_plane::bridge::ChurnEntry;
        use djinn_db::CommitFileChangeRepository;

        let since = shared::since_days_to_cutoff(since_days);
        let repo = CommitFileChangeRepository::new(self.state.db().clone());
        let rows = repo
            .churn(&ctx.id, limit.max(1), since.as_deref())
            .await
            .map_err(|e| format!("churn lookup: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|row| ChurnEntry {
                file_path: row.file_path,
                commit_count: row.commit_count.max(0) as usize,
                insertions: row.insertions.max(0) as usize,
                deletions: row.deletions.max(0) as usize,
                last_commit_at: row.last_commit_at,
            })
            .collect())
    }

    pub(super) async fn coupling_hotspots(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CoupledPairEntry>, String> {
        use djinn_control_plane::bridge::CoupledPairEntry;
        use djinn_db::CommitFileChangeRepository;

        let since = shared::since_days_to_cutoff(since_days);
        let repo = CommitFileChangeRepository::new(self.state.db().clone());
        let rows = repo
            .top_coupled_pairs(
                &ctx.id,
                limit.max(1),
                since.as_deref(),
                max_files_per_commit,
            )
            .await
            .map_err(|e| format!("coupling_hotspots lookup: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|row| CoupledPairEntry {
                file_a: row.file_a,
                file_b: row.file_b,
                co_edits: row.co_edits.max(0) as usize,
                last_co_edit: row.last_co_edit,
            })
            .collect())
    }

    pub(super) async fn coupling_hubs(
        &self,
        ctx: &ProjectCtx,
        limit: usize,
        since_days: Option<u32>,
        max_files_per_commit: usize,
    ) -> Result<Vec<djinn_control_plane::bridge::CouplingHubEntry>, String> {
        use djinn_control_plane::bridge::CouplingHubEntry;
        use djinn_db::CommitFileChangeRepository;

        let since = shared::since_days_to_cutoff(since_days);
        let repo = CommitFileChangeRepository::new(self.state.db().clone());
        // Over-fetch 2000 pairs for stable hub aggregation — the SQL
        // sort is the work here, the limit is cheap.
        let rows = repo
            .coupling_hubs(
                &ctx.id,
                limit.max(1),
                since.as_deref(),
                max_files_per_commit,
                2000,
            )
            .await
            .map_err(|e| format!("coupling_hubs lookup: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|row| CouplingHubEntry {
                file_path: row.file_path,
                total_coupling: row.total_coupling.max(0) as usize,
                partner_count: row.partner_count.max(0) as usize,
            })
            .collect())
    }

    pub(super) async fn resolve(
        &self,
        ctx: &ProjectCtx,
        key: &str,
        kind_hint: Option<&str>,
    ) -> Result<ResolveOutcome, String> {
        // Pre-resolve the caller's key against the live graph. We honour
        // `DJINN_CODE_GRAPH_AMBIGUITY` inside `resolve_node_with_hint` so
        // the bridge layer doesn't need to gate the variant separately.
        let graph = djinn_graph::canonical_graph::load_canonical_graph_only(
            &self.state,
            &ctx.id,
            &ctx.clone_path,
        )
        .await?;
        let outcome = match resolve_node_with_hint(&graph, key, kind_hint) {
            super::super::graph_neighbors::ResolveOutcome::Found(idx) => {
                let node = graph.node(idx);
                ResolveOutcome::Found(format_node_key(&node.id))
            }
            super::super::graph_neighbors::ResolveOutcome::Ambiguous(candidates) => {
                ResolveOutcome::Ambiguous(candidates)
            }
            super::super::graph_neighbors::ResolveOutcome::NotFound => ResolveOutcome::NotFound,
        };
        Ok(outcome)
    }
}
