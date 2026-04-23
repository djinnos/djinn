//! `pr_review_context` meta-tool — collects the base-graph signals that
//! matter for a pull-request review into a single bounded response.
//!
//! Runs entirely against the already-warmed canonical graph on the
//! project's base branch. No head graph is built; no diff text is
//! parsed. See [`PrReviewContextResponse::limitations_note`] for the
//! exact v1 constraints.

use std::process::Command;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bridge::{
    BoundaryRule, BoundaryViolation, ChangedRange, CycleGroup, DeprecatedHit, HotPathHit,
    HotspotEntry, ImpactEntry, ImpactResult, TouchedSymbol,
};
use crate::server::DjinnMcpServer;
use crate::tools::task_tools::{ErrorOr, ErrorResponse};

// ── Request types ──────────────────────────────────────────────────────────────

/// Input parameters for the `pr_review_context` meta-tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PrReviewContextParams {
    /// Absolute path to the project root. Used both for `git rev-parse`
    /// and as the lookup key into the warmed canonical graph.
    pub project_path: String,
    /// Hunks parsed from `git diff --unified=0 base..head`. Must be
    /// non-empty for the tool to return useful signal.
    pub changed_ranges: Vec<ChangedRange>,
    /// Entry-point SCIP keys (route handlers, `main`, etc.) used when
    /// computing [`PrReviewContextResponse::hot_path_overlap`].
    #[serde(default)]
    pub seed_entries: Vec<String>,
    /// Sink SCIP keys (DB queries, external APIs, etc.) used when
    /// computing [`PrReviewContextResponse::hot_path_overlap`].
    #[serde(default)]
    pub seed_sinks: Vec<String>,
    /// Architecture boundary rules. When empty, boundary analysis is
    /// skipped and [`PrReviewContextResponse::touched_boundary_violations`]
    /// is empty.
    #[serde(default)]
    pub boundary_rules: Vec<BoundaryRule>,
    /// Churn look-back window for the hotspot overlap. Defaults to 90.
    #[serde(default)]
    pub hotspots_window_days: Option<u32>,
    /// Per-list result caps. Missing fields fall back to the defaults
    /// encoded in `ResolvedCaps::default_caps`.
    #[serde(default)]
    pub caps: Option<PrReviewCaps>,
}

/// Per-list caps overriding the built-in defaults. Any field left
/// `None` uses the default from `default_caps`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct PrReviewCaps {
    pub touched_symbols: Option<usize>,
    pub blast_radius: Option<usize>,
    pub hotspot_overlap: Option<usize>,
    pub touched_cycles: Option<usize>,
    pub touched_boundary_violations: Option<usize>,
    pub touched_deprecated: Option<usize>,
    pub hot_path_overlap: Option<usize>,
}

// ── Response types ─────────────────────────────────────────────────────────────

/// Consolidated PR-review context. All list fields are capped per
/// `PrReviewCaps` before serialisation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PrReviewContextResponse {
    pub base_commit: String,
    pub changed_ranges_count: usize,
    pub touched_symbols: Vec<TouchedSymbol>,
    pub blast_radius: Vec<SymbolImpact>,
    pub hotspot_overlap: Vec<HotspotEntry>,
    pub touched_cycles: Vec<CycleGroup>,
    pub touched_deprecated: Vec<DeprecatedHit>,
    pub removed_public_symbols: Vec<SymbolRef>,
    pub hot_path_overlap: Vec<HotPathHit>,
    pub touched_boundary_violations: Vec<BoundaryViolation>,
    pub findings: Vec<Finding>,
    pub limitations_note: String,
}

/// A compact symbol reference used by `removed_public_symbols`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolRef {
    pub key: String,
    pub display_name: String,
    pub file: Option<String>,
}

/// Per-symbol blast-radius rollup. `top_dependents` is capped at 10
/// entries (depth asc).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SymbolImpact {
    pub key: String,
    pub display_name: String,
    pub total_dependents: usize,
    pub top_dependents: Vec<ImpactEntry>,
}

/// A single reviewer-facing finding. Kept compact to keep the overall
/// response under the serialised-size budget.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Finding {
    pub kind: String,
    pub severity: String,
    pub subject_key: Option<String>,
    pub subject_file: Option<String>,
    pub message: String,
    pub details_md: String,
    pub suggested_action: Option<String>,
    pub confidence: f32,
}

// ── Internal helpers ───────────────────────────────────────────────────────────

/// Fully-resolved list caps after merging caller overrides with
/// [`DEFAULT_CAPS`].
#[derive(Debug, Clone, Copy)]
struct ResolvedCaps {
    touched_symbols: usize,
    blast_radius: usize,
    hotspot_overlap: usize,
    touched_cycles: usize,
    touched_boundary_violations: usize,
    touched_deprecated: usize,
    hot_path_overlap: usize,
}

const DEFAULT_TOUCHED_SYMBOLS_CAP: usize = 100;
const DEFAULT_BLAST_RADIUS_CAP: usize = 50;
const DEFAULT_HOTSPOT_OVERLAP_CAP: usize = 20;
const DEFAULT_TOUCHED_CYCLES_CAP: usize = 20;
const DEFAULT_TOUCHED_BOUNDARY_VIOLATIONS_CAP: usize = 50;
const DEFAULT_TOUCHED_DEPRECATED_CAP: usize = 20;
const DEFAULT_HOT_PATH_OVERLAP_CAP: usize = 20;

/// Resolve caller-supplied overrides against the built-in defaults.
fn default_caps(caps: &Option<PrReviewCaps>) -> ResolvedCaps {
    let c = caps.as_ref();
    ResolvedCaps {
        touched_symbols: c
            .and_then(|c| c.touched_symbols)
            .unwrap_or(DEFAULT_TOUCHED_SYMBOLS_CAP),
        blast_radius: c
            .and_then(|c| c.blast_radius)
            .unwrap_or(DEFAULT_BLAST_RADIUS_CAP),
        hotspot_overlap: c
            .and_then(|c| c.hotspot_overlap)
            .unwrap_or(DEFAULT_HOTSPOT_OVERLAP_CAP),
        touched_cycles: c
            .and_then(|c| c.touched_cycles)
            .unwrap_or(DEFAULT_TOUCHED_CYCLES_CAP),
        touched_boundary_violations: c
            .and_then(|c| c.touched_boundary_violations)
            .unwrap_or(DEFAULT_TOUCHED_BOUNDARY_VIOLATIONS_CAP),
        touched_deprecated: c
            .and_then(|c| c.touched_deprecated)
            .unwrap_or(DEFAULT_TOUCHED_DEPRECATED_CAP),
        hot_path_overlap: c
            .and_then(|c| c.hot_path_overlap)
            .unwrap_or(DEFAULT_HOT_PATH_OVERLAP_CAP),
    }
}

const LIMITATIONS_NOTE: &str = "Base-graph-only analysis — this tool cannot detect cycles introduced by this PR, added public symbols, or visibility widening. The base commit is the current `main` HEAD; head-commit structure is not built. Removed-public-API detection requires diff text (deletion detection), which this tool does not parse.";

/// Resolve the base commit SHA via `git rev-parse HEAD` in
/// `project_path`. On any failure returns `"unknown"` so the response
/// remains well-formed.
fn resolve_base_commit(project_path: &str) -> String {
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "HEAD"]).current_dir(project_path);
    match cmd.output() {
        Ok(out) if out.status.success() => {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if sha.is_empty() {
                tracing::warn!(
                    project = %project_path,
                    "git rev-parse HEAD returned empty output; reporting 'unknown'"
                );
                "unknown".to_string()
            } else {
                sha
            }
        }
        Ok(out) => {
            tracing::warn!(
                project = %project_path,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "git rev-parse HEAD failed; reporting 'unknown'"
            );
            "unknown".to_string()
        }
        Err(e) => {
            tracing::warn!(
                project = %project_path,
                error = %e,
                "git rev-parse HEAD could not spawn; reporting 'unknown'"
            );
            "unknown".to_string()
        }
    }
}

/// Cap a finding's message to ≤ 200 chars — longer descriptions belong
/// in `details_md`. Truncation preserves the first 197 chars plus `...`.
fn cap_message(msg: String) -> String {
    if msg.chars().count() <= 200 {
        msg
    } else {
        let truncated: String = msg.chars().take(197).collect();
        format!("{truncated}...")
    }
}

/// Cap `details_md` to ≤ 1 KB (1024 bytes) while staying valid UTF-8.
fn cap_details(details: String) -> String {
    if details.len() <= 1024 {
        details
    } else {
        let mut end = 1024;
        while !details.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        let mut out = details[..end].to_string();
        out.push_str("...");
        out
    }
}

// ── Handler ────────────────────────────────────────────────────────────────────

#[tool_router(router = pr_review_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Consolidated PR-review signal bundle built from the base-branch
    /// canonical graph.
    #[tool(
        description = "Given a PR's changed line ranges (parsed from `git diff --unified=0 base..head`), assemble the base-graph signals that matter for review in one call: touched symbols with fan-in/fan-out, blast radius, hotspot overlap, touched cycles, deprecated-caller hits, hot-path overlap, and architecture-boundary violations. Base-graph-only — does NOT build a head graph, detect newly-introduced cycles, or parse the diff text for removed-API detection. Every list is capped per `caps` (defaults: touched_symbols=100, blast_radius=50, hotspot_overlap=20, touched_cycles=20, touched_boundary_violations=50, touched_deprecated=20, hot_path_overlap=20)."
    )]
    pub async fn pr_review_context(
        &self,
        Parameters(params): Parameters<PrReviewContextParams>,
    ) -> Json<ErrorOr<PrReviewContextResponse>> {
        match self.pr_review_context_inner(params).await {
            Ok(r) => Json(ErrorOr::Ok(r)),
            Err(error) => Json(ErrorOr::Error(ErrorResponse { error })),
        }
    }
}

impl DjinnMcpServer {
    async fn pr_review_context_inner(
        &self,
        params: PrReviewContextParams,
    ) -> Result<PrReviewContextResponse, String> {
        let caps = default_caps(&params.caps);
        let changed_ranges_count = params.changed_ranges.len();
        let base_commit = resolve_base_commit(&params.project_path);

        // Step 2: diff_touches on base graph.
        let diff = self
            .state
            .repo_graph()
            .diff_touches(&params.project_path, &params.changed_ranges)
            .await?;
        let mut touched_symbols = diff.touched_symbols;
        touched_symbols.truncate(caps.touched_symbols);
        let affected_files = diff.affected_files;
        let touched_symbol_keys: Vec<String> =
            touched_symbols.iter().map(|s| s.key.clone()).collect();

        // Step 5: blast radius — cap the per-symbol impact probing at 20
        // to keep the tool cheap; aggregate and cap the final list to
        // `caps.blast_radius`.
        let mut blast_radius: Vec<SymbolImpact> = Vec::new();
        for ts in touched_symbols.iter().take(20) {
            let impact = self
                .state
                .repo_graph()
                .impact(&params.project_path, &ts.key, 3, None)
                .await?;
            if let ImpactResult::Detailed(mut entries) = impact {
                let total = entries.len();
                entries.sort_by_key(|e| e.depth);
                entries.truncate(10);
                blast_radius.push(SymbolImpact {
                    key: ts.key.clone(),
                    display_name: ts.display_name.clone(),
                    total_dependents: total,
                    top_dependents: entries,
                });
            }
        }
        // Sort by total_dependents desc so the highest-impact symbols win.
        blast_radius.sort_by(|a, b| b.total_dependents.cmp(&a.total_dependents));
        blast_radius.truncate(caps.blast_radius);

        // Step 6: hotspot overlap — retain entries whose file is in
        // `affected_files`.
        let window_days = params.hotspots_window_days.unwrap_or(90);
        let hotspots = self
            .state
            .repo_graph()
            .hotspots(&params.project_path, window_days, None, 200)
            .await?;
        let affected_set: std::collections::HashSet<&str> =
            affected_files.iter().map(|s| s.as_str()).collect();
        let mut hotspot_overlap: Vec<HotspotEntry> = hotspots
            .into_iter()
            .filter(|h| affected_set.contains(h.file.as_str()))
            .collect();
        hotspot_overlap.truncate(caps.hotspot_overlap);

        // Step 7: touched cycles — retain any SCC whose member-set
        // intersects `touched_symbol_keys`.
        let cycles = self
            .state
            .repo_graph()
            .cycles(&params.project_path, Some("symbol"), 2)
            .await?;
        let touched_set: std::collections::HashSet<&str> =
            touched_symbol_keys.iter().map(|s| s.as_str()).collect();
        let mut touched_cycles: Vec<CycleGroup> = cycles
            .into_iter()
            .filter(|c| c.members.iter().any(|m| touched_set.contains(m.key.as_str())))
            .collect();
        touched_cycles.truncate(caps.touched_cycles);

        // Step 8: deprecated callers — retain hits where either the
        // deprecated symbol itself or any caller matches a touched key.
        let deprecated = self
            .state
            .repo_graph()
            .deprecated_callers(&params.project_path, 500)
            .await?;
        let mut touched_deprecated: Vec<DeprecatedHit> = deprecated
            .into_iter()
            .filter(|d| {
                touched_set.contains(d.deprecated_symbol.as_str())
                    || d.callers.iter().any(|c| touched_set.contains(c.key.as_str()))
            })
            .collect();
        touched_deprecated.truncate(caps.touched_deprecated);

        // Step 9: removed public symbols — v1 placeholder. See the
        // finding emitted below.
        let removed_public_symbols: Vec<SymbolRef> = Vec::new();

        // Step 10: hot-path overlap — requires both seed lists; when
        // either is empty the overlap set is empty by construction.
        let mut hot_path_overlap: Vec<HotPathHit> =
            if !params.seed_entries.is_empty() && !params.seed_sinks.is_empty() {
                self.state
                    .repo_graph()
                    .touches_hot_path(
                        &params.project_path,
                        &params.seed_entries,
                        &params.seed_sinks,
                        &touched_symbol_keys,
                    )
                    .await?
            } else {
                Vec::new()
            };
        hot_path_overlap.truncate(caps.hot_path_overlap);

        // Step 11: boundary violations — retain ones whose endpoints
        // touch the PR.
        let mut touched_boundary_violations: Vec<BoundaryViolation> =
            if !params.boundary_rules.is_empty() {
                let all = self
                    .state
                    .repo_graph()
                    .boundary_check(&params.project_path, &params.boundary_rules)
                    .await?;
                all.into_iter()
                    .filter(|v| {
                        touched_set.contains(v.from_key.as_str())
                            || touched_set.contains(v.to_key.as_str())
                    })
                    .collect()
            } else {
                Vec::new()
            };
        touched_boundary_violations.truncate(caps.touched_boundary_violations);

        // Step 12: findings — one per cycle (warn), deprecated hit
        // (warn), hot-path hit (warn), boundary violation (blocker),
        // hotspot overlap (info), plus the removed-public-API skipped
        // info item.
        let mut findings: Vec<Finding> = Vec::new();

        for c in &touched_cycles {
            let sample: Vec<String> = c
                .members
                .iter()
                .take(5)
                .map(|m| m.display_name.clone())
                .collect();
            findings.push(Finding {
                kind: "touched_cycle".into(),
                severity: "warn".into(),
                subject_key: c.members.first().map(|m| m.key.clone()),
                subject_file: None,
                message: cap_message(format!(
                    "PR touches a dependency cycle of size {}",
                    c.size
                )),
                details_md: cap_details(format!(
                    "Members (up to 5): {}",
                    sample.join(", ")
                )),
                suggested_action: Some("Verify the cycle was present on base and not widened by this PR.".into()),
                confidence: 0.85,
            });
        }

        for d in &touched_deprecated {
            findings.push(Finding {
                kind: "touched_deprecated".into(),
                severity: "warn".into(),
                subject_key: Some(d.deprecated_symbol.clone()),
                subject_file: d.deprecated_file.clone(),
                message: cap_message(format!(
                    "PR touches deprecated symbol `{}` (callers: {})",
                    d.deprecated_display_name,
                    d.callers.len()
                )),
                details_md: cap_details(format!(
                    "Callers (up to 5): {}",
                    d.callers
                        .iter()
                        .take(5)
                        .map(|c| c.display_name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                suggested_action: Some("Migrate remaining callers off the deprecated API.".into()),
                confidence: 0.9,
            });
        }

        for h in &hot_path_overlap {
            findings.push(Finding {
                kind: "hot_path_overlap".into(),
                severity: "warn".into(),
                subject_key: Some(h.symbol.clone()),
                subject_file: None,
                message: cap_message(format!(
                    "`{}` sits on {} entry→sink shortest path(s)",
                    h.symbol, h.on_path_count
                )),
                details_md: cap_details(match &h.example_path {
                    Some(p) => format!("Example path: {}", p.join(" → ")),
                    None => "No example path available.".into(),
                }),
                suggested_action: Some("Audit the change for latency/availability regressions on the critical path.".into()),
                confidence: 0.8,
            });
        }

        for v in &touched_boundary_violations {
            findings.push(Finding {
                kind: "touched_boundary_violation".into(),
                severity: "blocker".into(),
                subject_key: Some(v.from_key.clone()),
                subject_file: v.from_file.clone(),
                message: cap_message(format!(
                    "Forbidden edge: `{}` → `{}` ({})",
                    v.from_key, v.to_key, v.edge_kind
                )),
                details_md: cap_details(format!("Rule index: {}", v.rule_index)),
                suggested_action: Some("Remove the forbidden import or update the boundary rule.".into()),
                confidence: 0.95,
            });
        }

        for h in &hotspot_overlap {
            findings.push(Finding {
                kind: "hotspot_overlap".into(),
                severity: "info".into(),
                subject_key: None,
                subject_file: Some(h.file.clone()),
                message: cap_message(format!(
                    "PR touches hotspot `{}` (churn {}, composite {:.3})",
                    h.file, h.churn, h.composite_score
                )),
                details_md: cap_details(format!(
                    "Top symbols: {}",
                    h.top_symbols.join(", ")
                )),
                suggested_action: Some("Expect elevated regression risk — request extra review coverage.".into()),
                confidence: 0.6,
            });
        }

        findings.push(Finding {
            kind: "removed_public_api_skipped".into(),
            severity: "info".into(),
            subject_key: None,
            subject_file: None,
            message: "Removed-public-API detection skipped (v1)".into(),
            details_md: cap_details(
                "This tool analyses the base graph only and does not parse diff text for deletions. Use `git diff --stat` + manual review to check for removed public symbols.".into(),
            ),
            suggested_action: None,
            confidence: 1.0,
        });

        Ok(PrReviewContextResponse {
            base_commit,
            changed_ranges_count,
            touched_symbols,
            blast_radius,
            hotspot_overlap,
            touched_cycles,
            touched_deprecated,
            removed_public_symbols,
            hot_path_overlap,
            touched_boundary_violations,
            findings,
            limitations_note: LIMITATIONS_NOTE.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_caps_honours_overrides() {
        let caps = PrReviewCaps {
            touched_symbols: Some(7),
            ..Default::default()
        };
        let r = default_caps(&Some(caps));
        assert_eq!(r.touched_symbols, 7);
        assert_eq!(r.blast_radius, DEFAULT_BLAST_RADIUS_CAP);
    }

    #[test]
    fn default_caps_uses_defaults_when_missing() {
        let r = default_caps(&None);
        assert_eq!(r.touched_symbols, DEFAULT_TOUCHED_SYMBOLS_CAP);
        assert_eq!(r.blast_radius, DEFAULT_BLAST_RADIUS_CAP);
        assert_eq!(r.hotspot_overlap, DEFAULT_HOTSPOT_OVERLAP_CAP);
        assert_eq!(r.touched_cycles, DEFAULT_TOUCHED_CYCLES_CAP);
        assert_eq!(
            r.touched_boundary_violations,
            DEFAULT_TOUCHED_BOUNDARY_VIOLATIONS_CAP
        );
        assert_eq!(r.touched_deprecated, DEFAULT_TOUCHED_DEPRECATED_CAP);
        assert_eq!(r.hot_path_overlap, DEFAULT_HOT_PATH_OVERLAP_CAP);
    }

    #[test]
    fn cap_message_truncates_long_strings() {
        let long = "x".repeat(250);
        let capped = cap_message(long);
        assert_eq!(capped.chars().count(), 200);
        assert!(capped.ends_with("..."));
    }

    #[test]
    fn cap_message_passes_short_strings_through() {
        let s = "short".to_string();
        assert_eq!(cap_message(s.clone()), s);
    }

    #[test]
    fn cap_details_truncates_over_1kb() {
        let big = "y".repeat(2048);
        let capped = cap_details(big);
        assert!(capped.len() <= 1024 + 3);
        assert!(capped.ends_with("..."));
    }

    #[test]
    fn parses_params_from_json() {
        let json = serde_json::json!({
            "project_path": "/tmp/repo",
            "changed_ranges": [
                {"file": "a.rs", "start_line": 1, "end_line": 10}
            ],
        });
        let p: PrReviewContextParams = serde_json::from_value(json).unwrap();
        assert_eq!(p.project_path, "/tmp/repo");
        assert_eq!(p.changed_ranges.len(), 1);
        assert!(p.seed_entries.is_empty());
        assert!(p.caps.is_none());
    }
}
