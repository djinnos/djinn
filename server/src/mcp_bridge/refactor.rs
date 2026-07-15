use djinn_control_plane::bridge::ComplexityMetrics as WireComplexityMetrics;
use std::collections::HashMap;
use std::path::PathBuf;

/// Proposal qoxm: per-file cross-module co-change pressure from the
/// co-change sidecar. Sum each file's coupling score to partners in a
/// *different* crate/module; that summed score becomes the fourth
/// refactor-ranking axis, so a config file that changes lockstep with a
/// source file in another crate scores higher.
pub(crate) fn cross_module_cochange_pressure(
    graph: &djinn_graph::repo_graph::RepoDependencyGraph,
) -> HashMap<PathBuf, f64> {
    let mut cross_module_cc: HashMap<PathBuf, f64> = HashMap::new();
    for cc in graph.cochange_edges() {
        let (Some(fa), Some(fb)) = (
            graph.node(cc.source).file_path.clone(),
            graph.node(cc.target).file_path.clone(),
        ) else {
            continue;
        };
        if module_key(&fa.to_string_lossy()) == module_key(&fb.to_string_lossy()) {
            continue;
        }
        *cross_module_cc.entry(fa).or_insert(0.0) += cc.confidence;
        *cross_module_cc.entry(fb).or_insert(0.0) += cc.confidence;
    }
    cross_module_cc
}

pub(crate) fn complexity_metrics_to_wire(
    m: djinn_graph::complexity::ComplexityMetrics,
) -> WireComplexityMetrics {
    WireComplexityMetrics {
        cyclomatic: m.cyclomatic,
        cognitive: m.cognitive,
        nloc: m.nloc,
        max_nesting: m.max_nesting,
        param_count: m.param_count,
    }
}

/// Iter 28: sort `entries` in-place by the requested `sort_by` field
/// descending, with a deterministic alpha tie-break. Extracted so the
/// op's pure ranking logic is unit-testable without the canonical
/// graph round-trip.
pub(crate) fn sort_function_complexity_entries(
    entries: &mut [djinn_control_plane::bridge::FunctionComplexityEntry],
    sort_by: &str,
) {
    entries.sort_by(|a, b| {
        let cmp = match sort_by {
            "cyclomatic" => b.metrics.cyclomatic.cmp(&a.metrics.cyclomatic),
            "nloc" => b.metrics.nloc.cmp(&a.metrics.nloc),
            "max_nesting" => b.metrics.max_nesting.cmp(&a.metrics.max_nesting),
            "param_count" => b.metrics.param_count.cmp(&a.metrics.param_count),
            // "cognitive" (default)
            _ => b.metrics.cognitive.cmp(&a.metrics.cognitive),
        };
        cmp.then_with(|| a.display_name.cmp(&b.display_name))
            .then_with(|| a.key.cmp(&b.key))
    });
}

/// Iter 28: roll a per-function entry list up into per-file aggregates,
/// then sort by the file-level analog of `sort_by`. Pure on its inputs
/// for unit-test isolation.
pub(crate) fn aggregate_files_complexity(
    entries: &[djinn_control_plane::bridge::FunctionComplexityEntry],
    sort_by: &str,
) -> Vec<djinn_control_plane::bridge::FileComplexityEntry> {
    use djinn_control_plane::bridge::FileComplexityEntry;
    use std::collections::BTreeMap;

    struct FileAgg {
        function_count: u32,
        total_cognitive: u32,
        total_cyclomatic: u32,
        total_nloc: u32,
        max_function_cognitive: u16,
        max_function_name: String,
    }
    let mut by_file: BTreeMap<String, FileAgg> = BTreeMap::new();
    for entry in entries {
        let agg = by_file.entry(entry.file.clone()).or_insert(FileAgg {
            function_count: 0,
            total_cognitive: 0,
            total_cyclomatic: 0,
            total_nloc: 0,
            max_function_cognitive: 0,
            max_function_name: String::new(),
        });
        agg.function_count = agg.function_count.saturating_add(1);
        agg.total_cognitive = agg
            .total_cognitive
            .saturating_add(u32::from(entry.metrics.cognitive));
        agg.total_cyclomatic = agg
            .total_cyclomatic
            .saturating_add(u32::from(entry.metrics.cyclomatic));
        agg.total_nloc = agg.total_nloc.saturating_add(u32::from(entry.metrics.nloc));
        if entry.metrics.cognitive > agg.max_function_cognitive
            || (entry.metrics.cognitive == agg.max_function_cognitive
                && (agg.max_function_name.is_empty() || entry.display_name < agg.max_function_name))
        {
            agg.max_function_cognitive = entry.metrics.cognitive;
            agg.max_function_name = entry.display_name.clone();
        }
    }

    let mut files: Vec<FileComplexityEntry> = by_file
        .into_iter()
        .map(|(file, agg)| FileComplexityEntry {
            file,
            function_count: agg.function_count,
            total_cognitive: agg.total_cognitive,
            total_cyclomatic: agg.total_cyclomatic,
            total_nloc: agg.total_nloc,
            max_function_cognitive: agg.max_function_cognitive,
            max_function_name: agg.max_function_name,
        })
        .collect();

    files.sort_by(|a, b| {
        let cmp = match sort_by {
            "cyclomatic" => b.total_cyclomatic.cmp(&a.total_cyclomatic),
            "nloc" => b.total_nloc.cmp(&a.total_nloc),
            // `max_nesting` doesn't have a per-file aggregate — use
            // the worst-function cognitive as the proxy. `param_count`
            // collapses to "how many function-likes live here" since
            // formal params don't sum meaningfully across functions.
            "max_nesting" => b.max_function_cognitive.cmp(&a.max_function_cognitive),
            "param_count" => b.function_count.cmp(&a.function_count),
            // "cognitive" (default)
            _ => b.total_cognitive.cmp(&a.total_cognitive),
        };
        cmp.then_with(|| a.file.cmp(&b.file))
    });
    files
}
/// Iter 29: pure helper for the `refactor_candidates` op. Takes a
/// candidate set already filtered to function-like nodes with
/// complexity payloads, plus a `(file → file-level commit count)`
/// map. Computes per-axis means + population stddevs (divide by N to
/// keep zero-stddev handling tight), produces z-scores, sorts by the
/// mean composite descending, truncates to `limit`, and stamps a
/// `tier` label per the post-cap rule (top 10% high / next 15% medium
/// / rest low). Sets fewer than 10 entries collapse to all-high
/// (degenerate small project).
///
/// Extracted so the ranking logic is unit-testable without spinning
/// up an `AppState` / canonical graph.
pub(crate) fn compute_refactor_candidates(
    candidates: &[RefactorCandidateInput],
    churn_map: &std::collections::HashMap<std::path::PathBuf, u32>,
    limit: usize,
) -> Vec<djinn_control_plane::bridge::RefactorCandidate> {
    use djinn_control_plane::bridge::RefactorCandidate;

    if candidates.is_empty() {
        return Vec::new();
    }

    // Resolve churn for every candidate up front so the mean/stddev
    // pass and the per-row z-score pass walk the same numbers.
    let churn_for: Vec<u32> = candidates
        .iter()
        .map(|c| {
            churn_map
                .get(std::path::Path::new(&c.file))
                .copied()
                .unwrap_or(0)
        })
        .collect();

    let n = candidates.len() as f64;
    let mean_cog: f64 = candidates
        .iter()
        .map(|c| f64::from(c.cognitive))
        .sum::<f64>()
        / n;
    let mean_churn: f64 = churn_for.iter().map(|c| f64::from(*c)).sum::<f64>() / n;
    let mean_pr: f64 = candidates.iter().map(|c| c.page_rank).sum::<f64>() / n;
    // Proposal qoxm: fourth axis — cross-module co-change pressure.
    let mean_cc: f64 = candidates
        .iter()
        .map(|c| c.cross_module_cochange)
        .sum::<f64>()
        / n;

    // Population stddev (divide by N) — a sample stddev would need
    // N>=2 and special-case 1-element sets; population stays correct
    // at every N and zero-variance sets cleanly degenerate to 0.
    let var_cog: f64 = candidates
        .iter()
        .map(|c| (f64::from(c.cognitive) - mean_cog).powi(2))
        .sum::<f64>()
        / n;
    let var_churn: f64 = churn_for
        .iter()
        .map(|c| (f64::from(*c) - mean_churn).powi(2))
        .sum::<f64>()
        / n;
    let var_pr: f64 = candidates
        .iter()
        .map(|c| (c.page_rank - mean_pr).powi(2))
        .sum::<f64>()
        / n;
    let var_cc: f64 = candidates
        .iter()
        .map(|c| (c.cross_module_cochange - mean_cc).powi(2))
        .sum::<f64>()
        / n;
    let std_cog = var_cog.sqrt();
    let std_churn = var_churn.sqrt();
    let std_pr = var_pr.sqrt();
    let std_cc = var_cc.sqrt();

    let z =
        |x: f64, mean: f64, std: f64| -> f64 { if std > 1e-9 { (x - mean) / std } else { 0.0 } };

    let mut out: Vec<RefactorCandidate> = candidates
        .iter()
        .zip(churn_for.iter())
        .map(|(c, churn)| {
            let z_cog = z(f64::from(c.cognitive), mean_cog, std_cog);
            let z_churn = z(f64::from(*churn), mean_churn, std_churn);
            let z_pr = z(c.page_rank, mean_pr, std_pr);
            // Proposal qoxm: fold cross-module co-change in as a fourth axis so
            // files that change lockstep with other crates/modules rank higher.
            let z_cc = z(c.cross_module_cochange, mean_cc, std_cc);
            let composite = (z_cog + z_churn + z_pr + z_cc) / 4.0;
            RefactorCandidate {
                key: c.key.clone(),
                uid: c.key.clone(),
                display_name: c.display_name.clone(),
                file: c.file.clone(),
                start_line: c.start_line,
                end_line: c.end_line,
                composite_score: composite,
                // Tier filled in below post-sort.
                tier: String::new(),
                cognitive: c.cognitive,
                cyclomatic: c.cyclomatic,
                churn_commits: *churn,
                page_rank: c.page_rank,
                z_cognitive: z_cog,
                z_churn,
                z_page_rank: z_pr,
            }
        })
        .collect();

    // Sort by composite desc, then a deterministic tiebreaker on
    // display_name then key so equal-score sets ship in stable order.
    out.sort_by(|a, b| {
        b.composite_score
            .partial_cmp(&a.composite_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.display_name.cmp(&b.display_name))
            .then_with(|| a.key.cmp(&b.key))
    });
    out.truncate(limit);
    assign_refactor_tiers(&mut out);
    out
}

/// Proposal qoxm: coarse module/crate identity for a file path, used to decide
/// whether a co-change partner counts as *cross-module*. Prefers a Cargo-style
/// `crates/<name>/` segment (matching the djinn workspace layout and the galaxy
/// crate coloring); otherwise falls back to the top-level path component. Two
/// files are "cross-module" when their keys differ.
pub(crate) fn module_key(path: &str) -> &str {
    let bytes = path.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = path[search_from..].find("crates/") {
        let at = search_from + rel;
        let anchored = at == 0 || bytes[at - 1] == b'/';
        if anchored {
            let after = &path[at + "crates/".len()..];
            let name = after.split('/').next().unwrap_or("");
            if !name.is_empty() {
                return name;
            }
        }
        search_from = at + "crates/".len();
    }
    path.split('/').next().unwrap_or("")
}

/// Per-candidate input row for [`compute_refactor_candidates`]. Plain
/// data so the helper stays decoupled from `RepoDependencyGraph` /
/// canonical-graph types and can be exercised by unit tests directly.
#[derive(Debug, Clone)]
pub(crate) struct RefactorCandidateInput {
    pub(crate) key: String,
    pub(crate) display_name: String,
    pub(crate) file: String,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    pub(crate) cognitive: u16,
    pub(crate) cyclomatic: u16,
    pub(crate) page_rank: f64,
    /// Proposal qoxm: cross-module commit co-change pressure for this
    /// candidate's file — the summed coupling score of every co-change partner
    /// that lives in a *different* crate/module. A config file that keeps
    /// changing lockstep with a source file in another crate is a hidden-
    /// coupling refactor target that static complexity/churn/pagerank miss, so
    /// this feeds the composite as a fourth z-scored axis. `0.0` when the file
    /// has no cross-module co-change (or when the coupling index is empty).
    pub(crate) cross_module_cochange: f64,
}

/// Iter 29: stamp a `tier` label on each entry of an already-sorted
/// `refactor_candidates` set. Top 10% = `"high"`, next 15% = `"medium"`,
/// rest = `"low"`. Tier counts are rounded to nearest integer; for
/// `limit=30` that's 3 high / 4 medium / 23 low. Sets with fewer than
/// 10 candidates collapse to all-high (degenerate small project).
pub(crate) fn assign_refactor_tiers(out: &mut [djinn_control_plane::bridge::RefactorCandidate]) {
    if out.is_empty() {
        return;
    }
    let n = out.len();
    if n < 10 {
        for entry in out.iter_mut() {
            entry.tier = "high".to_string();
        }
        return;
    }
    let high_cnt = ((n as f64) * 0.10).round() as usize;
    let medium_cnt = ((n as f64) * 0.15).round() as usize;
    let high_cnt = high_cnt.min(n);
    let medium_cnt = medium_cnt.min(n - high_cnt);
    for (i, entry) in out.iter_mut().enumerate() {
        entry.tier = if i < high_cnt {
            "high".to_string()
        } else if i < high_cnt + medium_cnt {
            "medium".to_string()
        } else {
            "low".to_string()
        };
    }
}
