use super::*;

// ─── PR E3: diff-aware reviewer context (`detect_changes` + `impact`) ───────

/// Risk bucket for a touched symbol — mirrors PR C3's `ImpactRisk`
/// classification (`crates/djinn-control-plane/src/tools/graph_tools.rs:242`).
/// Re-implemented here because the C3 type is crate-private; the
/// classification thresholds are the binding contract, not the type
/// identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReviewerDiffRisk {
    Low,
    Medium,
    High,
    Critical,
}

impl ReviewerDiffRisk {
    fn label(self) -> &'static str {
        match self {
            ReviewerDiffRisk::Critical => "CRITICAL",
            ReviewerDiffRisk::High => "HIGH",
            ReviewerDiffRisk::Medium => "MEDIUM",
            ReviewerDiffRisk::Low => "LOW",
        }
    }

    /// PR C3 thresholds (top-down: critical first wins). `direct` /
    /// `total` / `modules` are OR-combined within each tier.
    fn classify(direct: usize, total: usize, modules: usize) -> Self {
        if direct >= 20 || total >= 200 || modules >= 10 {
            ReviewerDiffRisk::Critical
        } else if direct >= 10 || total >= 80 || modules >= 5 {
            ReviewerDiffRisk::High
        } else if direct >= 3 || total >= 20 || modules >= 2 {
            ReviewerDiffRisk::Medium
        } else {
            ReviewerDiffRisk::Low
        }
    }

    /// Sort key — descending (Critical first). `Reverse` would also
    /// work; this avoids importing an extra type for one call site.
    fn rank(self) -> u8 {
        match self {
            ReviewerDiffRisk::Critical => 3,
            ReviewerDiffRisk::High => 2,
            ReviewerDiffRisk::Medium => 1,
            ReviewerDiffRisk::Low => 0,
        }
    }
}

/// Bucket a file path into a "module" key (first two path segments).
/// Mirrors the PR C3 `module_bucket` helper. Repo-relative paths come
/// in pre-stripped, so we slice them as-is.
fn reviewer_module_bucket(file_path: &str) -> String {
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

/// Compute (direct, total, modules) for a detailed `ImpactResult` slice
/// — the inputs to PR C3 risk classification.
fn reviewer_impact_metrics(
    entries: &[djinn_control_plane::bridge::ImpactEntry],
) -> (usize, usize, usize) {
    use std::collections::HashSet;
    let direct = entries.iter().filter(|e| e.depth == 1).count();
    let total = entries.len();
    let mut buckets: HashSet<String> = HashSet::new();
    for entry in entries {
        if let Some(path) = entry.file_path.as_deref() {
            buckets.insert(reviewer_module_bucket(path));
        }
    }
    (direct, total, buckets.len())
}

/// One row of the reviewer diff bullet list, pre-sort.
struct ReviewerDiffRow {
    name: String,
    file_path: String,
    risk: ReviewerDiffRisk,
    direct: usize,
    modules: usize,
}

/// Build the auto-injected `code_graph detect_changes` block for the
/// reviewer role.
///
/// Returns `None` when the reviewer role is not in the
/// `DJINN_AUTO_CODE_CONTEXT_ROLES` allowlist, when both `from_sha` and
/// `to_sha` are missing (and there are no `changed_files` to fall back
/// on), when the canonical graph is unavailable, or when the detected
/// change set is empty.
///
/// For each touched symbol, calls `impact` (depth =
/// [`REVIEWER_DIFF_IMPACT_DEPTH`]) to derive PR C3-style risk, direct
/// caller count, and module count. Bullets are sorted CRITICAL → HIGH
/// → MEDIUM → LOW, then by descending direct caller count, capped at
/// [`REVIEWER_DIFF_CONTEXT_MAX_BULLETS`] entries and
/// [`REVIEWER_DIFF_CONTEXT_BUDGET_CHARS`] characters via
/// `truncate::smart_truncate`.
pub async fn build_reviewer_diff_context(
    role_name: &str,
    task: &Task,
    app_state: &SlotContext,
    project_path: &str,
    from_sha: Option<&str>,
    to_sha: Option<&str>,
) -> Option<String> {
    if !is_role_auto_code_context_enabled(role_name) {
        return None;
    }
    // SHA range is required — the `detect_changes` op accepts a
    // changed_files fallback, but that lives outside the reviewer
    // dispatch flow today. If we don't have at least one of from/to,
    // skip silently.
    if from_sha.is_none() && to_sha.is_none() {
        return None;
    }

    let graph_ops = app_state.repo_graph_ops.clone()?;
    let ctx = djinn_control_plane::bridge::ProjectCtx {
        id: task.project_id.clone(),
        clone_path: project_path.to_string(),
        workspace: None,
        sub_path: None,
    };

    let detected = match graph_ops.detect_changes(&ctx, from_sha, to_sha, &[]).await {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(
                role = role_name,
                task_id = %task.short_id,
                from = ?from_sha,
                to = ?to_sha,
                error = %e,
                "build_reviewer_diff_context: detect_changes() failed; skipping"
            );
            return None;
        }
    };
    if detected.touched_symbols.is_empty() {
        return None;
    }

    let mut rows: Vec<ReviewerDiffRow> = Vec::new();
    for sym in detected.touched_symbols.iter() {
        if rows.len() >= REVIEWER_DIFF_CONTEXT_MAX_BULLETS {
            break;
        }
        // `impact` is BFS-bound; ask for the detailed shape (no group_by)
        // so we can compute (direct, total, modules) ourselves.
        let impact = match graph_ops
            .impact(
                &ctx,
                ctx.workspace.as_deref(),
                &sym.uid,
                REVIEWER_DIFF_IMPACT_DEPTH,
                None,
                None,
            )
            .await
        {
            Ok(djinn_control_plane::bridge::ImpactResult::Detailed(v)) => v,
            // Grouped path: shouldn't happen with `group_by=None` but
            // defensively skip — the metrics derivation below assumes
            // detailed entries.
            Ok(djinn_control_plane::bridge::ImpactResult::Grouped(_)) => Vec::new(),
            Err(e) => {
                tracing::debug!(
                    role = role_name,
                    uid = %sym.uid,
                    error = %e,
                    "build_reviewer_diff_context: impact() failed; \
                     using zero-impact fallback"
                );
                Vec::new()
            }
        };
        let (direct, total, modules) = reviewer_impact_metrics(&impact);
        let risk = ReviewerDiffRisk::classify(direct, total, modules);

        rows.push(ReviewerDiffRow {
            name: sym.name.clone(),
            file_path: sym.file_path.clone(),
            risk,
            direct,
            modules,
        });
    }

    if rows.is_empty() {
        return None;
    }

    // Sort: highest risk first; tie-break on direct caller count desc.
    rows.sort_by(|a, b| {
        b.risk
            .rank()
            .cmp(&a.risk.rank())
            .then_with(|| b.direct.cmp(&a.direct))
    });

    let mut lines: Vec<String> = Vec::new();
    lines.push("## Changed symbols (HIGH risk first)".to_string());
    lines.push(String::new());
    for row in &rows {
        lines.push(format!(
            "- `{name}` ({risk} risk, {direct} direct callers, {modules} modules)\n  - file: {file}",
            name = row.name,
            risk = row.risk.label(),
            direct = row.direct,
            modules = row.modules,
            file = row.file_path,
        ));
    }

    let body = lines.join("\n");
    Some(crate::truncate::smart_truncate(
        &body,
        REVIEWER_DIFF_CONTEXT_BUDGET_CHARS,
    ))
}
