use super::*;

// ── Risk classification (PR C3) ─────────────────────────────────────────────────

/// PR C3: bucket a file path into a "module" key by taking its first
/// two path segments. The plan calls for "first 2 segments after repo
/// root"; the file paths returned by the graph layer are already
/// repo-relative, so we slice them as-is.
///
/// - `src/auth/User.rs` → `"src/auth"`
/// - `crates/djinn-control-plane/src/lib.rs` → `"crates/djinn-control-plane"`
/// - `Cargo.toml` (single segment) → `"Cargo.toml"` (degenerate; the
///   single-file repo case still counts as one module)
pub(crate) fn module_bucket(file_path: &str) -> String {
    let normalized = file_path.replace('\\', "/");
    let mut iter = normalized.split('/').filter(|s| !s.is_empty());
    let first = iter.next();
    let second = iter.next();
    match (first, second) {
        (Some(a), Some(b)) => format!("{a}/{b}"),
        (Some(a), None) => a.to_string(),
        _ => file_path.to_string(),
    }
}

/// PR C3 metrics tuple — `(direct_count, total_impacted, module_count)`.
/// Computed once and shared between risk classification and the summary
/// string so the two never disagree.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ImpactMetrics {
    pub(crate) direct: usize,
    pub(crate) total: usize,
    pub(crate) modules: usize,
}

/// Compute risk metrics from a detailed `ImpactEntry` slice. `direct`
/// counts entries at depth 1 (BFS root has depth 0 and isn't emitted),
/// `total` is the full impacted set, `modules` is the unique module
/// bucket count over all entries that carry a `file_path`. Entries
/// without a `file_path` (external/virtual symbols) don't contribute
/// to the module count — the plan treats them as "no module signal".
pub(crate) fn metrics_from_detailed(entries: &[ImpactEntry]) -> ImpactMetrics {
    use std::collections::HashSet;
    let direct = entries.iter().filter(|e| e.depth == 1).count();
    let total = entries.len();
    let mut buckets: HashSet<String> = HashSet::new();
    for entry in entries {
        if let Some(path) = entry.file_path.as_deref() {
            buckets.insert(module_bucket(path));
        }
    }
    ImpactMetrics {
        direct,
        total,
        modules: buckets.len(),
    }
}

/// Compute risk metrics from the per-file rollup (`group_by=file`).
/// `total` is the sum of `occurrence_count` across groups; `direct` is
/// the count of entries with `max_depth == 1` aggregated across groups
/// — but since the rollup loses per-entry depth granularity, we
/// approximate `direct` as the sum of `occurrence_count` over groups
/// whose `max_depth == 1`, which is exact iff the only depth-1 entries
/// land in single-direct-only files (rare). For the wider (multi-depth)
/// case the approximation under-counts; the detailed path remains the
/// authoritative source. `modules` is the unique two-segment bucket
/// count across the listed files.
pub(crate) fn metrics_from_grouped(groups: &[FileGroupEntry]) -> ImpactMetrics {
    use std::collections::HashSet;
    let total: usize = groups.iter().map(|g| g.occurrence_count).sum();
    let direct: usize = groups
        .iter()
        .filter(|g| g.max_depth == 1)
        .map(|g| g.occurrence_count)
        .sum();
    let mut buckets: HashSet<String> = HashSet::new();
    for g in groups {
        buckets.insert(module_bucket(&g.file));
    }
    ImpactMetrics {
        direct,
        total,
        modules: buckets.len(),
    }
}

/// Format the 1-line plan-mandated summary. Stable phrasing —
/// reviewer prompts in PR E3 lift this verbatim.
pub(crate) fn impact_summary(metrics: ImpactMetrics) -> String {
    if metrics.direct == 0 && metrics.total == 0 {
        return "no direct callers in current graph snapshot".to_string();
    }
    format!(
        "{} direct caller(s) across {} module(s)",
        metrics.direct, metrics.modules
    )
}
