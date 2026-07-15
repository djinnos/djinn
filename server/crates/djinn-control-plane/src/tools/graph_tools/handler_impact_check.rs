use super::*;

// ── `impact_check` helper methods on `DjinnMcpServer` ──────────────────────
//
// Extracted from the single dense `code_graph_impact_check` body
// (epic uajf, task 1zcr).  Each helper owns one concern: request
// validation, stale-graph response, fresh-graph response, and
// per-target impact aggregation.

impl DjinnMcpServer {
    /// Validate the `impact_check` request and collect the
    /// `scope_crates` set.  Returns a `String` error when the
    /// `impact_targets` list is missing or empty (matching the
    /// pre-refactor error strings verbatim).
    pub(super) fn validate_impact_check_request<'a>(
        &self,
        params: &'a CodeGraphParams,
    ) -> Result<(&'a [String], std::collections::HashSet<String>), String> {
        let targets = params.impact_targets.as_deref().ok_or_else(|| {
            "impact_check requires `impact_targets` — a non-empty list of \
             symbol keys, file paths, or crate names to analyse"
                .to_string()
        })?;
        if targets.is_empty() {
            return Err("impact_check requires at least one entry in `impact_targets`".to_string());
        }
        let scope_crates: std::collections::HashSet<String> = params
            .scope_crates
            .as_deref()
            .unwrap_or_default()
            .iter()
            .cloned()
            .collect();
        Ok((targets, scope_crates))
    }

    /// Build the `ImpactCheckResponse` returned when the canonical
    /// graph is stale or missing.  The recommendation is the strict
    /// `needs_spike` value, `low_confidence` is set to `true`, and
    /// `next_step` carries the warm-the-graph hint string.
    /// Staleness metadata is attached when the caller supplied a
    /// non-empty `current_head`, matching the pre-refactor contract.
    pub(super) fn build_stale_impact_check_response(
        &self,
        staleness_info: &ImpactCheckStaleness,
    ) -> ImpactCheckResponse {
        let staleness = if !staleness_info.caller_commit.is_empty() {
            Some(GraphStaleness::compute(
                &staleness_info.caller_commit,
                staleness_info.cached_commit.as_deref(),
            ))
        } else {
            None
        };
        ImpactCheckResponse {
            affected_crates: Vec::new(),
            affected_files: Vec::new(),
            affected_symbols: Vec::new(),
            safe_independent_slice: false,
            recommendation: "needs_spike".to_string(),
            low_confidence: true,
            next_step: Some(
                "Graph is stale or missing.  Warm the graph for this \
                 project and retry, or run a tech spike to manually \
                 verify compile-time consumers."
                    .to_string(),
            ),
            graph_staleness: staleness,
            coverage: None,
        }
    }

    /// Format the final `ImpactCheckResponse` for a fresh-graph
    /// preflight.  Staleness metadata is attached when the caller
    /// supplied a non-empty `current_head` (mirrors the pre-refactor
    /// behaviour so the wire shape is identical).
    pub(super) fn build_impact_check_response(
        &self,
        affected_crates: Vec<String>,
        affected_files: Vec<String>,
        affected_symbols: Vec<String>,
        safe_independent_slice: bool,
        recommendation: &'static str,
        staleness_info: &ImpactCheckStaleness,
    ) -> ImpactCheckResponse {
        let staleness = if !staleness_info.caller_commit.is_empty() {
            Some(GraphStaleness::compute(
                &staleness_info.caller_commit,
                staleness_info.cached_commit.as_deref(),
            ))
        } else {
            None
        };
        ImpactCheckResponse {
            affected_crates,
            affected_files,
            affected_symbols,
            safe_independent_slice,
            recommendation: recommendation.to_string(),
            low_confidence: false,
            next_step: None,
            graph_staleness: staleness,
            coverage: None,
        }
    }

    /// glqk: build the `impact_check` response when an unindexed workspace
    /// intersects the analysed scope. The verdict is escalated to `needs_spike`
    /// with `low_confidence: true`, the offending workspaces are named in
    /// `next_step`, and the coverage advisory is attached inline (so the
    /// escalation is self-explaining even to a caller that ignores the
    /// dispatch-layer advisory). The affected sets found SO FAR are preserved so
    /// the caller still sees the partial (untrustworthy) analysis.
    pub(super) fn build_uncovered_impact_check_response(
        &self,
        affected_crates: Vec<String>,
        affected_files: Vec<String>,
        affected_symbols: Vec<String>,
        coverage_gaps: &[CoverageAdvisoryWorkspace],
        staleness_info: &ImpactCheckStaleness,
    ) -> ImpactCheckResponse {
        let staleness = if !staleness_info.caller_commit.is_empty() {
            Some(GraphStaleness::compute(
                &staleness_info.caller_commit,
                staleness_info.cached_commit.as_deref(),
            ))
        } else {
            None
        };
        let names: Vec<String> = coverage_gaps
            .iter()
            .map(|g| format!("{} ({}, {})", g.workspace_slug, g.language, g.status))
            .collect();
        let message = format!(
            "Unindexed workspace(s) intersect the analysed scope — {}. \
             Callers in these workspaces are invisible to the graph, so this \
             preflight cannot prove the slice is safe. Run a spike (grep the \
             workspace) before removing/renaming.",
            names.join("; ")
        );
        ImpactCheckResponse {
            affected_crates,
            affected_files,
            affected_symbols,
            safe_independent_slice: false,
            recommendation: "needs_spike".to_string(),
            low_confidence: true,
            next_step: Some(message.clone()),
            graph_staleness: staleness,
            coverage: Some(CoverageAdvisory {
                message,
                unindexed_workspaces: coverage_gaps.to_vec(),
            }),
        }
    }

    /// Aggregate a single target's impact into the accumulator.
    ///
    /// Crate-level targets (the target string is a known crate)
    /// are resolved via the precomputed crate graph edges.
    /// Symbol/file targets fall through to the bridge's `impact()`
    /// call; the bridge already filters `is_external` nodes, so
    /// every entry in the result is a workspace-internal consumer.
    /// Bridge errors and "target not found" responses are skipped
    /// gracefully so other targets still contribute — matches the
    /// pre-refactor contract verbatim.
    pub(super) async fn aggregate_target_impact(
        &self,
        ctx: &ProjectCtx,
        params: &CodeGraphParams,
        target: &str,
        depth: usize,
        crate_index: &CrateIndex<'_>,
        aggregator: &mut ImpactAggregator,
    ) {
        if crate_index.is_known_crate(target) {
            // Crate-level: find inbound edges where the target is
            // the consumer (edge.target == target).
            aggregator.add_crate_consumers(crate_index.edges(), target);
            return;
        }
        // Symbol/file: run impact() via the bridge.  The bridge
        // already filters `is_external` nodes, so every entry
        // in the result is a workspace-internal consumer.
        let impact = self
            .state
            .repo_graph()
            .impact(
                ctx,
                params.workspace.as_deref(),
                target,
                depth,
                None,
                params.min_confidence,
            )
            .await;
        match impact {
            Ok(ImpactResult::Detailed(entries)) => {
                aggregator.add_detailed_entries(&entries, crate_index);
            }
            Ok(ImpactResult::Grouped(groups)) => {
                // Grouped results only have file paths.
                aggregator.add_grouped_files(&groups, crate_index);
            }
            Err(_) => {
                // Target not found or bridge error — skip
                // gracefully so other targets still contribute.
            }
        }
    }
}

// ── `impact_check` helper types ──────────────────────────────────────────────
//
// These structs back the per-target aggregation done by
// `code_graph_impact_check`.  They live in the module (not the
// impl block) so they can be constructed by the aggregation helper
// without taking `&self`, which keeps the per-target dispatch loop
// flat.  Both structs are crate-private and never escape the
// handler module.

/// Pre-computed indexes over the workspace crate graph used by the
/// `impact_check` aggregation loop.  Built once from the
/// `CrateGraphResponse` and re-used for every target.
///
/// `known_crates` is the set of crate names the graph knows about;
/// `crate_dirs` is the `(crate_name, manifest_dir_prefix)` table
/// used to map a file path back to its owning crate.  The
/// `<external>` pseudo-crate is excluded from `crate_dirs` so a
/// path that doesn't start under any real crate never gets a false
/// mapping.  `edges` is the cross-crate edge list used by the
/// crate-level branch (when a target is itself a known crate name).
pub(super) struct CrateIndex<'a> {
    known_crates: std::collections::HashSet<&'a str>,
    crate_dirs: Vec<(String, String)>,
    edges: &'a [CrateEdgeEntry],
}

impl<'a> CrateIndex<'a> {
    /// Build the index from a `CrateGraphResponse` returned by the
    /// bridge.  Extracts the crate name set, the per-crate
    /// directory prefixes (excluding the synthetic `<external>`
    /// crate), and the full edge list.
    pub(super) fn from_crate_graph(crate_result: &'a CrateGraphResponse) -> Self {
        let known_crates: std::collections::HashSet<&'a str> = crate_result
            .crates
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        let crate_dirs: Vec<(String, String)> = crate_result
            .crates
            .iter()
            .filter_map(|c| {
                let manifest = std::path::Path::new(&c.manifest_path);
                let dir = manifest.parent()?.to_string_lossy().into_owned();
                if c.name == "<external>" {
                    None
                } else {
                    Some((c.name.clone(), dir))
                }
            })
            .collect();
        Self {
            known_crates,
            crate_dirs,
            edges: &crate_result.edges,
        }
    }

    /// `true` when `name` is a workspace-internal crate known to the
    /// graph.  The `<external>` pseudo-crate is included here so the
    /// caller can choose whether to surface it; the aggregation
    /// loop explicitly drops it from the result set later.
    pub(super) fn is_known_crate(&self, name: &str) -> bool {
        self.known_crates.contains(name)
    }

    /// Map a file path to the set of crates whose manifest
    /// directory is a prefix of `file`.  Iterates the pre-computed
    /// `crate_dirs` table — paths that don't fall under any real
    /// crate return an empty iterator, which is the pre-refactor
    /// behaviour (a path outside the workspace simply doesn't
    /// contribute a crate mapping).
    pub(super) fn crates_for_file<'b>(
        &'b self,
        file: &'b str,
    ) -> impl Iterator<Item = &'b String> + 'b {
        self.crate_dirs
            .iter()
            .filter(move |(_, dir_prefix)| file.starts_with(dir_prefix.as_str()))
            .map(|(crate_name, _)| crate_name)
    }

    /// Iterate the cross-crate edges.  Used by the crate-level
    /// branch of `aggregate_target_impact` to find inbound
    /// consumer crates.
    pub(super) fn edges(&self) -> &[CrateEdgeEntry] {
        self.edges
    }

    /// glqk: the manifest directory prefix for a crate name, if known.
    /// Used to map a scope/affected crate back to its owning workspace so
    /// `impact_check` can tell whether an unindexed workspace actually
    /// intersects the analysed scope.
    pub(super) fn dir_prefix_for_crate(&self, name: &str) -> Option<&str> {
        self.crate_dirs
            .iter()
            .find(|(crate_name, _)| crate_name == name)
            .map(|(_, dir)| dir.as_str())
    }
}

/// `true` when `child` (a repo-relative path) lies under `root` (a repo-relative
/// workspace root). An empty `root` is the repo root and contains everything.
fn path_is_under(child: &str, root: &str) -> bool {
    if root.is_empty() {
        return true;
    }
    let root = root.trim_end_matches('/');
    child == root || child.starts_with(&format!("{root}/"))
}

impl DjinnMcpServer {
    /// glqk: gap workspaces (from `project_workspace_coverage`) whose root
    /// contains at least one of the `relevant_crates` (the scope ∪ affected
    /// crates). A non-empty result means the impact analysis cannot be trusted
    /// — an unindexed workspace intersecting the scope could hide callers — so
    /// `impact_check` escalates its verdict to `needs_spike`.
    ///
    /// Cheap: reads coverage rows only, never the graph blob. Returns the
    /// intersecting gap workspaces (deduplicated by slug) for naming in the
    /// response.
    pub(super) async fn impact_check_coverage_gaps(
        &self,
        project_id: &str,
        crate_index: &CrateIndex<'_>,
        relevant_crates: &std::collections::HashSet<String>,
    ) -> Vec<CoverageAdvisoryWorkspace> {
        let rows = match djinn_db::ProjectWorkspaceCoverageRepository::new(self.state.db().clone())
            .list_for_project(project_id)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "impact_check coverage gate: list_for_project failed; not escalating"
                );
                return Vec::new();
            }
        };

        // Directory prefixes of the crates in scope, resolved once.
        let relevant_dirs: Vec<&str> = relevant_crates
            .iter()
            .filter_map(|name| crate_index.dir_prefix_for_crate(name))
            .collect();

        let mut seen = std::collections::HashSet::new();
        let mut gaps = Vec::new();
        for row in rows {
            if !djinn_db::coverage_status_is_gap(&row.status) {
                continue;
            }
            let intersects = relevant_dirs
                .iter()
                .any(|dir| path_is_under(dir, &row.workspace_root));
            if !intersects {
                continue;
            }
            if seen.insert(row.workspace_slug.clone()) {
                gaps.push(CoverageAdvisoryWorkspace {
                    workspace_slug: row.workspace_slug,
                    language: row.language,
                    status: row.status,
                    detail: row.detail,
                });
            }
        }
        gaps
    }
}

/// Small accumulator collecting the `affected_*` sets during the
/// per-target impact traversal.  Carries a `HashSet` for each
/// returned field so duplicates across targets are collapsed, then
/// `finalized()` drains them into the `Vec` shape `ImpactCheckResponse`
/// expects.
pub(super) struct ImpactAggregator {
    pub(super) affected_crates: std::collections::HashSet<String>,
    pub(super) affected_files: std::collections::HashSet<String>,
    pub(super) affected_symbols: std::collections::HashSet<String>,
}

impl ImpactAggregator {
    pub(super) fn new() -> Self {
        Self {
            affected_crates: std::collections::HashSet::new(),
            affected_files: std::collections::HashSet::new(),
            affected_symbols: std::collections::HashSet::new(),
        }
    }

    /// Insert every workspace-internal consumer crate that has the
    /// target crate as a dependency (via `crate_graph.edges`,
    /// excluding the `<external>` source).  Mirrors the pre-refactor
    /// inlined edge walk.
    pub(super) fn add_crate_consumers(&mut self, crate_edges: &[CrateEdgeEntry], target: &str) {
        for edge in crate_edges {
            if edge.target == *target && edge.source != "<external>" {
                self.affected_crates.insert(edge.source.clone());
            }
        }
    }

    /// Insert every symbol and (when present) its file path from a
    /// `Detailed` impact result, then map the file path back to its
    /// owning crate(s).  Mirrors the pre-refactor inlined `Detailed`
    /// arm.
    pub(super) fn add_detailed_entries(
        &mut self,
        entries: &[ImpactEntry],
        crate_index: &CrateIndex<'_>,
    ) {
        for entry in entries {
            self.affected_symbols.insert(entry.key.clone());
            if let Some(ref fp) = entry.file_path {
                self.affected_files.insert(fp.clone());
                self.insert_crates_for_file(fp, crate_index);
            }
        }
    }

    /// Insert every file path from a `Grouped` impact result and
    /// map it back to its owning crate(s).  Mirrors the pre-refactor
    /// inlined `Grouped` arm.
    pub(super) fn add_grouped_files(
        &mut self,
        groups: &[FileGroupEntry],
        crate_index: &CrateIndex<'_>,
    ) {
        for group in groups {
            self.affected_files.insert(group.file.clone());
            self.insert_crates_for_file(&group.file, crate_index);
        }
    }

    pub(super) fn insert_crates_for_file(&mut self, file: &str, crate_index: &CrateIndex<'_>) {
        for crate_name in crate_index.crates_for_file(file) {
            self.affected_crates.insert(crate_name.clone());
        }
    }

    /// Drop the synthetic `<external>` crate from the result set
    /// (the pre-refactor guard) and drain the sets into the
    /// `Vec`-shape the response DTO expects.  Order is non-deterministic
    /// (HashSet iteration) — matches the pre-refactor contract.
    pub(super) fn finalized(mut self) -> (Vec<String>, Vec<String>, Vec<String>) {
        self.affected_crates.remove("<external>");
        let affected_crates = self.affected_crates.into_iter().collect();
        let affected_files = self.affected_files.into_iter().collect();
        let affected_symbols = self.affected_symbols.into_iter().collect();
        (affected_crates, affected_files, affected_symbols)
    }
}

/// Pure helper that derives the `safe_independent_slice` boolean and
/// the recommendation string from the aggregated affected-crate set
/// and the caller-supplied `scope_crates` set.  Preserves the
/// pre-refactor exact strings: `ok_independent`, `chain_tasks`,
/// `atomic_cutover`.
pub(super) fn derive_safe_slice_and_recommendation(
    affected_crates: &[String],
    scope_crates: &std::collections::HashSet<String>,
) -> (bool, &'static str) {
    let safe_independent_slice = if affected_crates.is_empty() {
        // No workspace-internal consumers — nothing to break.
        true
    } else if scope_crates.is_empty() {
        // No scope provided — can't verify consumers are within
        // scope, so assume not safe.
        false
    } else {
        // Safe only when every affected crate is inside the
        // caller's proposed slice.
        affected_crates.iter().all(|c| scope_crates.contains(c))
    };
    let recommendation = if safe_independent_slice {
        if affected_crates.is_empty() {
            // No consumers at all — each task can ship independently.
            "ok_independent"
        } else {
            // Consumers exist but they're all within the proposed
            // slice — tasks need explicit ordering.
            "chain_tasks"
        }
    } else {
        // Consumers outside the proposed slice — must be a single
        // atomic cutover.
        "atomic_cutover"
    };
    (safe_independent_slice, recommendation)
}

// ── `impact_check` staleness entry point ────────────────────────────────────
//
// kfgh / epic z3en: the planner-facing `impact_check` MCP tool (built in
// sibling task xkqs) MUST short-circuit with `needs_spike` whenever the
// canonical graph is stale — a stale consumer set would defeat the entire
// purpose of preflight. This helper is the single entry point that
// `impact_check` calls before doing any consumer computation.
//
// `code_graph` ops share the same staleness primitive via
// [`check_impact_staleness`]: that path attaches a [`GraphStaleness`]
// struct (lenient on missing) to every response, while `impact_check`
// short-circuits on the same boolean (strict on missing). Both paths
// read `pinned_commit` via the same bridge call (`RepoGraphOps::status`)
// so the staleness signal stays anchored to a single source.

/// Snapshot of the canonical graph staleness signal at the moment an
/// `impact_check` call begins. The boolean is the strict form
/// (`true` when the graph is missing, un-pinned, or out-of-sync with
/// the caller's HEAD). The strings are the trimmed echoes so the
/// `impact_check` response can surface them in `next_step` hints.
#[derive(Debug, Clone)]
pub(super) struct ImpactCheckStaleness {
    /// `true` when `cached_commit` is missing/blank or differs from
    /// `caller_commit`. Drives the `needs_spike` short-circuit in
    /// `impact_check`.
    pub is_stale: bool,
    /// Trimmed caller-supplied commit, or `""` if the caller omitted
    /// `current_head` (in which case `is_stale` is `true` because we
    /// have no anchor for comparison).
    pub caller_commit: String,
    /// Trimmed cached graph commit, or `None` when the graph has no
    /// pinned commit (un-warmed).
    pub cached_commit: Option<String>,
}

impl ImpactCheckStaleness {
    /// `true` when the caller did not supply a `current_head` AND the
    /// graph has no pinned commit. This is the "completely unanchored"
    /// case — both sides are missing, so we cannot answer and must
    /// spike. Distinct from `is_stale` which is the canonical
    /// missing/blank/mismatch signal.
    #[allow(dead_code)] // diagnostic tested in unit tests; not consumed by the handler flow yet
    pub fn is_completely_unanchored(&self) -> bool {
        self.caller_commit.is_empty() && self.cached_commit.is_none()
    }
}

/// Run the staleness check for an `impact_check` call.
///
/// Performs the same `RepoGraphOps::status` peek that `attach_graph_staleness`
/// uses for `code_graph` ops, then funnels both inputs through the shared
/// [`check_impact_staleness`] primitive so `impact_check` and `code_graph`
/// never drift on the staleness semantics.
///
/// Contract for callers (the `impact_check` handler built by sibling
/// task xkqs):
///
/// 1. Call this helper BEFORE computing any consumers.
/// 2. If [`ImpactCheckStaleness::is_stale`] is `true`, return
///    `recommendation = "needs_spike"` and a low-confidence flag without
///    computing consumers.
/// 3. Otherwise proceed with the standard `impact_check` flow using
///    [`ImpactCheckStaleness::caller_commit`] / `cached_commit` to
///    surface freshness metadata in the response.
///
/// `caller_head` is the (raw, pre-trim) caller commit. An empty string
/// is allowed and yields `is_stale = true` (no anchor on the caller's
/// side).
pub(super) async fn check_impact_check_staleness(
    graph: &dyn crate::bridge::RepoGraphOps,
    ctx: &crate::bridge::ProjectCtx,
    caller_head: &str,
) -> ImpactCheckStaleness {
    let cached = match graph.status(ctx).await {
        Ok(status) => status.pinned_commit,
        Err(e) => {
            // A failed status lookup is the same as an un-pinned graph
            // for impact preflight: we have no anchor, so we MUST
            // surface `is_stale=true` and let the caller decide
            // whether to spike. We do NOT silently fall through to
            // the un-stale default — that would defeat the freshness
            // signal. Logged at debug so we can correlate with
            // upstream warmer failures without spamming warn logs.
            tracing::debug!(
                error = %e,
                "impact_check staleness: status lookup failed; treating as un-pinned"
            );
            None
        }
    };
    let (is_stale, caller_commit, cached_commit) =
        check_impact_staleness(caller_head, cached.as_deref());
    ImpactCheckStaleness {
        is_stale,
        caller_commit,
        cached_commit,
    }
}
