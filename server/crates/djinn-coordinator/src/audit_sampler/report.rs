//! False-negative ledger report helpers and revalidation query boundaries.
//!
//! This module provides aggregated reporting over persisted audit outcomes
//! and revalidation helpers for hotfix/rollback/reopen ground truth.
//!
//! ## Report helpers
//!
//! [`generate_false_negative_report`] aggregates audit outcomes per stratum
//! (unflagged_merged and autonomous_release), producing:
//!
//! - Per-stratum totals and miss counts
//! - Miss-rate estimates (misses / total outcomes)
//! - Category and severity breakdowns
//! - `requires_rule_update` counts
//! - Backlog/SLO context (open audit tasks, unmaterialized selections)
//!
//! ## Revalidation helpers
//!
//! [`query_revalidation_ground_truth`] checks for hotfix, rollback, and
//! reopen signals that correlate with audit-outcome misses. Because the
//! project does not yet have a canonical hotfix/rollback/reopen fact source
//! in the audit sampler tables, this helper implements the **boundary**:
//!
//! - It queries `tasks` for tasks that were reopened (`total_reopen_count > 0`)
//!   or closed with a hotfix/rollback `close_reason`.
//! - It joins these against the audit merged-change ledger to identify
//!   revalidation candidates.
//! - Tests use synthetic repository data (task rows with `close_reason =
//!   "hotfix"` etc.) to exercise the boundary without external systems.

use serde::{Deserialize, Serialize};
use tracing::warn;

use djinn_db::{AuditSamplerRepository, TaskRepository};

// ── Report types ─────────────────────────────────────────────────────────────

/// Aggregated false-negative report for a project.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FalseNegativeReport {
    /// Per-stratum report entries.
    pub strata: Vec<StratumReport>,
    /// Backlog context: number of unmaterialized selections.
    pub unmaterialized_count: usize,
    /// Backlog context: number of open audit tasks.
    pub open_audit_task_count: i64,
    /// Whether any SLO thresholds are breached.
    pub slo_breached: bool,
}

/// Per-stratum aggregation of audit outcomes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StratumReport {
    /// Stratum name (`unflagged_merged` or `autonomous_release`).
    pub stratum: String,
    /// Total number of outcomes recorded for this stratum.
    pub total_outcomes: i64,
    /// Number of miss outcomes.
    pub total_misses: i64,
    /// Miss-rate estimate: `total_misses / total_outcomes` (0.0 when
    /// `total_outcomes == 0`).
    pub miss_rate: f64,
    /// Breakdown of misses by category (e.g. `"missed_security_finding": 3`).
    pub category_breakdown: std::collections::HashMap<String, i64>,
    /// Breakdown of misses by severity (e.g. `"high": 1, "medium": 2`).
    pub severity_breakdown: std::collections::HashMap<String, i64>,
    /// Number of outcomes that flagged `requires_rule_update`.
    pub requires_rule_update_count: i64,
}

/// Result of a revalidation ground-truth query.
///
/// Contains candidates identified from hotfix, rollback, and reopen signals
/// that may correlate with missed audit findings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevalidationResult {
    /// Merged changes whose associated task was reopened at least once.
    pub reopened_candidates: Vec<RevalidationCandidate>,
    /// Merged changes whose associated task was closed with a hotfix or
    /// rollback reason.
    pub hotfix_rollback_candidates: Vec<RevalidationCandidate>,
}

/// A single revalidation candidate linking an audit merged-change to a
/// ground-truth signal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevalidationCandidate {
    /// The merged-change id from the audit ledger.
    pub merged_change_id: String,
    /// The merge commit SHA.
    pub merge_commit_sha: String,
    /// The associated task id (if available).
    pub task_id: Option<String>,
    /// The project id.
    pub project_id: String,
    /// The ground-truth signal: `"reopen"`, `"hotfix"`, or `"rollback"`.
    pub signal: String,
    /// Additional detail (e.g. reopen count, close reason).
    pub detail: String,
}

// ── Report generation ────────────────────────────────────────────────────────

/// Generate a false-negative report for a project.
///
/// Queries all persisted audit outcomes for the given project and aggregates
/// them by stratum. Also includes backlog/SLO context.
pub async fn generate_false_negative_report(
    audit_repo: &AuditSamplerRepository,
    project_id: &str,
) -> Result<FalseNegativeReport, String> {
    let outcomes = audit_repo
        .list_outcomes_for_project(project_id)
        .await
        .map_err(|e| format!("failed to list outcomes: {e}"))?;

    let unmaterialized = audit_repo
        .list_unmaterialized_selections()
        .await
        .map_err(|e| format!("failed to list unmaterialized: {e}"))?;

    let open_count = audit_repo
        .count_open_audit_tasks()
        .await
        .map_err(|e| format!("failed to count open tasks: {e}"))?;

    // Aggregate per stratum.
    let mut strata_map: std::collections::HashMap<String, StratumReport> =
        std::collections::HashMap::new();

    for row in &outcomes {
        let entry = strata_map
            .entry(row.stratum.clone())
            .or_insert_with(|| StratumReport {
                stratum: row.stratum.clone(),
                total_outcomes: 0,
                total_misses: 0,
                miss_rate: 0.0,
                category_breakdown: std::collections::HashMap::new(),
                severity_breakdown: std::collections::HashMap::new(),
                requires_rule_update_count: 0,
            });

        entry.total_outcomes += 1;

        if row.outcome == "miss" {
            entry.total_misses += 1;

            if let Some(ref cat) = row.miss_category {
                *entry.category_breakdown.entry(cat.clone()).or_insert(0) += 1;
            }
            if let Some(ref sev) = row.miss_severity {
                *entry.severity_breakdown.entry(sev.clone()).or_insert(0) += 1;
            }
        }

        if row.requires_rule_update {
            entry.requires_rule_update_count += 1;
        }
    }

    // Compute miss rates.
    let mut strata: Vec<StratumReport> = strata_map.into_values().collect();
    for s in &mut strata {
        if s.total_outcomes > 0 {
            s.miss_rate = s.total_misses as f64 / s.total_outcomes as f64;
        }
    }
    strata.sort_by(|a, b| a.stratum.cmp(&b.stratum));

    // Check SLO breach: any unmaterialized selection older than 14 days.
    let slo_breached = unmaterialized.iter().any(|sel| {
        // Parse the selection_created_at timestamp and check if it's older
        // than 14 days. We use a simple string comparison for ISO-8601 dates
        // since the format is fixed (YYYY-MM-DDTHH:MM:SS.MSZ).
        is_older_than_days(&sel.selection_created_at, 14)
    });

    Ok(FalseNegativeReport {
        strata,
        unmaterialized_count: unmaterialized.len(),
        open_audit_task_count: open_count,
        slo_breached,
    })
}

/// Simple check: is the ISO-8601 timestamp older than N days from now?
///
/// Uses string prefix comparison for year-month-day which is safe for the
/// fixed format used by the audit tables. Falls back to false on parse
/// errors.
fn is_older_than_days(iso_ts: &str, days: i64) -> bool {
    // Quick string-based approach: compare date portion.
    // For production use, a proper datetime parse would be better, but
    // this is sufficient for report helpers.
    use djinn_core::clock::{Clock, SystemClock};

    let now = SystemClock::new().now();
    let now_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let threshold_secs = now_secs.saturating_sub(days as u64 * 86400);

    // Parse the ISO timestamp to epoch seconds (best-effort).
    parse_iso_to_epoch_secs(iso_ts).is_some_and(|ts| ts < threshold_secs)
}

/// Parse an ISO-8601 timestamp (YYYY-MM-DDTHH:MM:SS.MSZ) to epoch seconds.
///
/// This is a simplified parser that handles the exact format used by the
/// audit tables. Returns `None` on parse failure.
fn parse_iso_to_epoch_secs(iso: &str) -> Option<u64> {
    // Expected format: "2026-07-09T12:00:00.000Z"
    let date_part = iso.get(..10)?;
    let time_part = iso.get(11..19)?;

    let year: i32 = date_part.get(..4)?.parse().ok()?;
    let month: u32 = date_part.get(5..7)?.parse().ok()?;
    let day: u32 = date_part.get(8..10)?.parse().ok()?;

    let hour: u32 = time_part.get(..2)?.parse().ok()?;
    let minute: u32 = time_part.get(3..5)?.parse().ok()?;
    let second: u32 = time_part.get(6..8)?.parse().ok()?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // Days from epoch (1970-01-01) using a simplified calendar calculation.
    let mut days_from_epoch = 0i64;
    for y in 1970..year {
        days_from_epoch += if is_leap_year(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days_from_epoch += days_in_month(m, year) as i64;
    }
    days_from_epoch += (day - 1) as i64;

    let epoch =
        days_from_epoch as u64 * 86400 + hour as u64 * 3600 + minute as u64 * 60 + second as u64;

    Some(epoch)
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(month: u32, year: i32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// ── Revalidation helpers ─────────────────────────────────────────────────────

/// Query for revalidation ground-truth candidates.
///
/// This helper identifies audit merged changes whose associated tasks show
/// hotfix, rollback, or reopen signals. It uses the `tasks` table's
/// `close_reason` and `total_reopen_count` columns as ground-truth sources.
///
/// Because no canonical hotfix/rollback/reopen fact source exists in the
/// audit sampler tables, this implements the **helper boundary**: the
/// function queries available persisted facts (task metadata) and joins
/// against the audit ledger. Tests exercise this with synthetic task data.
pub async fn query_revalidation_ground_truth(
    audit_repo: &AuditSamplerRepository,
    task_repo: &TaskRepository,
    project_id: &str,
) -> Result<RevalidationResult, String> {
    let mut reopened_candidates = Vec::new();
    let mut hotfix_rollback_candidates = Vec::new();

    // Get all outcomes for this project to find which merged changes have
    // been audited.
    let outcomes = audit_repo
        .list_outcomes_for_project(project_id)
        .await
        .map_err(|e| format!("failed to list outcomes: {e}"))?;

    // For each outcome, check the associated task for revalidation signals.
    // We need to get the task_id from the selection.
    for outcome in &outcomes {
        let selection = audit_repo
            .get_selection_by_id(&outcome.selection_id)
            .await
            .map_err(|e| format!("failed to get selection: {e}"))?;

        let Some(sel) = selection else {
            continue;
        };

        // The source task (not the audit task) is referenced by
        // merged_change.task_id.
        let merged_change = audit_repo
            .get_merged_change_by_id(&sel.merged_change_id)
            .await
            .map_err(|e| format!("failed to get merged change: {e}"))?;

        let Some(mc) = merged_change else {
            continue;
        };

        let Some(ref source_task_id) = mc.task_id else {
            continue;
        };

        // Look up the source task for revalidation signals.
        match task_repo.get(source_task_id).await {
            Ok(Some(task)) => {
                let candidate = RevalidationCandidate {
                    merged_change_id: mc.id.clone(),
                    merge_commit_sha: mc.merge_commit_sha.clone(),
                    task_id: mc.task_id.clone(),
                    project_id: mc.project_id.clone(),
                    signal: String::new(),
                    detail: String::new(),
                };

                // Check for reopens.
                if task.total_reopen_count > 0 {
                    let mut c = candidate.clone();
                    c.signal = "reopen".to_string();
                    c.detail = format!("reopen_count={}", task.total_reopen_count);
                    reopened_candidates.push(c);
                }

                // Check for hotfix/rollback close reasons.
                if let Some(ref reason) = task.close_reason {
                    let lower = reason.to_lowercase();
                    if lower.contains("hotfix") || lower.contains("rollback") {
                        let mut c = candidate;
                        c.signal = if lower.contains("hotfix") {
                            "hotfix".to_string()
                        } else {
                            "rollback".to_string()
                        };
                        c.detail = format!("close_reason={reason}");
                        hotfix_rollback_candidates.push(c);
                    }
                }
            }
            Ok(None) => {
                // Task deleted — skip.
            }
            Err(e) => {
                warn!(
                    error = %e,
                    task_id = %source_task_id,
                    "revalidation: failed to look up source task"
                );
            }
        }
    }

    Ok(RevalidationResult {
        reopened_candidates,
        hotfix_rollback_candidates,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use djinn_db::{
        AuditSamplerRepository, AuditStratum, CreateSampleFrameParams, CreateSamplePolicyParams,
        CreateSelectionParams, RecordOutcomeParams, TaskRepository, UpsertMergedChangeParams,
    };

    use super::*;

    /// Seed a project and return project_id.
    async fn seed_project(db: &djinn_db::Database) -> String {
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(db, &project_id, &format!("proj-{project_id}")).await;
        project_id
    }

    /// Create a merged change + frame + selection. Returns selection_id.
    async fn seed_selection_for_outcome(
        repo: &AuditSamplerRepository,
        project_id: &str,
        stratum: AuditStratum,
        merge_sha: &str,
        task_id: Option<&str>,
    ) -> String {
        let policy = repo
            .create_sample_policy(CreateSamplePolicyParams {
                project_id,
                revision: 1,
                policy_json: &json!({"unflagged_rate": 0.1, "autonomous_rate": 0.5}),
            })
            .await
            .unwrap();

        let change = repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id,
                task_id,
                pr_number: Some(1),
                head_sha: Some("head"),
                merge_commit_sha: merge_sha,
                merged_at: "2026-06-28T00:00:00Z",
                gate_outcome: "pass",
                gate_provenance: None,
                release_provenance: None,
                stratum,
                excluded: false,
                exclusion_reason: None,
            })
            .await
            .unwrap();

        let frame = repo
            .create_sample_frame(CreateSampleFrameParams {
                project_id,
                policy_id: &policy.id,
                window_start: "2026-06-24T00:00:00Z",
                window_end: "2026-07-01T00:00:00Z",
                revision: 1,
                eligible_change_ids: &json!([&change.id]),
                content_hash: None,
                exclusion_counts: &json!({}),
                exclusion_reasons: &json!([]),
                sealed_at: "2026-07-01T00:05:00Z",
            })
            .await
            .unwrap();

        let sel = repo
            .create_selection(CreateSelectionParams {
                frame_id: &frame.id,
                merged_change_id: &change.id,
                stratum: AuditStratum::UnflaggedMerged,
                selected_position: 0,
                algorithm: "hmac-sha256-counter-v1",
                seed_commitment: &"aa".repeat(32),
                seed_reveal: None,
                replay_data: &json!({}),
                audit_task_id: None,
                created_at: None,
            })
            .await
            .unwrap();

        sel.id
    }

    /// Create an audit task row directly (for revalidation tests).
    async fn seed_audit_task(
        db: &djinn_db::Database,
        project_id: &str,
        close_reason: Option<&str>,
        total_reopen_count: i32,
    ) -> String {
        let status = if close_reason.is_some() {
            "closed"
        } else {
            "open"
        };
        djinn_db::test_support::seed_task_row(
            db,
            djinn_db::test_support::UsageTestTaskSeed {
                project_id,
                status,
                close_reason,
                total_reopen_count,
            },
        )
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn report_aggregates_per_stratum() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let audit_repo = AuditSamplerRepository::new(db.clone());

        // Create 3 selections: 2 unflagged, 1 autonomous.
        let sel1 = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::UnflaggedMerged,
            "sha-uf-1",
            Some("task-uf-1"),
        )
        .await;
        let sel2 = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::UnflaggedMerged,
            "sha-uf-2",
            Some("task-uf-2"),
        )
        .await;
        let sel3 = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::AutonomousRelease,
            "sha-ar-1",
            Some("task-ar-1"),
        )
        .await;

        // Record outcomes: sel1=clean, sel2=miss(high, security), sel3=miss(medium, logic).
        audit_repo
            .record_outcome(RecordOutcomeParams {
                selection_id: &sel1,
                outcome: djinn_db::AuditOutcomeKind::Clean,
                miss_category: None,
                miss_severity: None,
                requires_rule_update: false,
                notes: None,
            })
            .await
            .unwrap();

        audit_repo
            .record_outcome(RecordOutcomeParams {
                selection_id: &sel2,
                outcome: djinn_db::AuditOutcomeKind::Miss,
                miss_category: Some("missed_security_finding"),
                miss_severity: Some("high"),
                requires_rule_update: true,
                notes: None,
            })
            .await
            .unwrap();

        audit_repo
            .record_outcome(RecordOutcomeParams {
                selection_id: &sel3,
                outcome: djinn_db::AuditOutcomeKind::Miss,
                miss_category: Some("logic_error"),
                miss_severity: Some("medium"),
                requires_rule_update: false,
                notes: None,
            })
            .await
            .unwrap();

        // Generate report.
        let report = generate_false_negative_report(&audit_repo, &project_id)
            .await
            .unwrap();

        assert_eq!(report.strata.len(), 2, "should have 2 strata");

        // Autonomous release stratum.
        let ar = report
            .strata
            .iter()
            .find(|s| s.stratum == "autonomous_release")
            .expect("autonomous_release stratum");
        assert_eq!(ar.total_outcomes, 1);
        assert_eq!(ar.total_misses, 1);
        assert!((ar.miss_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(ar.category_breakdown["logic_error"], 1);
        assert_eq!(ar.severity_breakdown["medium"], 1);
        assert_eq!(ar.requires_rule_update_count, 0);

        // Unflagged stratum.
        let uf = report
            .strata
            .iter()
            .find(|s| s.stratum == "unflagged_merged")
            .expect("unflagged_merged stratum");
        assert_eq!(uf.total_outcomes, 2);
        assert_eq!(uf.total_misses, 1);
        assert!((uf.miss_rate - 0.5).abs() < f64::EPSILON);
        assert_eq!(uf.category_breakdown["missed_security_finding"], 1);
        assert_eq!(uf.severity_breakdown["high"], 1);
        assert_eq!(uf.requires_rule_update_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn report_empty_project() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let audit_repo = AuditSamplerRepository::new(db.clone());

        let report = generate_false_negative_report(&audit_repo, &project_id)
            .await
            .unwrap();

        assert!(report.strata.is_empty());
        assert_eq!(report.unmaterialized_count, 0);
        assert_eq!(report.open_audit_task_count, 0);
        assert!(!report.slo_breached);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revalidation_finds_reopened_task() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events);

        // Create a source task with reopen_count=2.
        let source_task_id = seed_audit_task(&db, &project_id, None, 2).await;

        // Create a selection linked to that source task.
        let sel_id = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::UnflaggedMerged,
            "sha-reopen-1",
            Some(&source_task_id),
        )
        .await;

        // Record a miss outcome.
        audit_repo
            .record_outcome(RecordOutcomeParams {
                selection_id: &sel_id,
                outcome: djinn_db::AuditOutcomeKind::Miss,
                miss_category: Some("missed_security_finding"),
                miss_severity: Some("high"),
                requires_rule_update: true,
                notes: None,
            })
            .await
            .unwrap();

        let result = query_revalidation_ground_truth(&audit_repo, &task_repo, &project_id)
            .await
            .unwrap();

        assert_eq!(result.reopened_candidates.len(), 1);
        assert_eq!(result.reopened_candidates[0].signal, "reopen");
        assert_eq!(result.reopened_candidates[0].detail, "reopen_count=2");
        assert!(result.hotfix_rollback_candidates.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revalidation_finds_hotfix_and_rollback() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events);

        // Create hotfix task.
        let hotfix_task =
            seed_audit_task(&db, &project_id, Some("hotfix: security patch"), 0).await;
        // Create rollback task.
        let rollback_task =
            seed_audit_task(&db, &project_id, Some("rollback: regression found"), 0).await;

        let sel_hotfix = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::AutonomousRelease,
            "sha-hotfix-1",
            Some(&hotfix_task),
        )
        .await;
        let sel_rollback = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::UnflaggedMerged,
            "sha-rollback-1",
            Some(&rollback_task),
        )
        .await;

        // Record miss outcomes for both.
        for sel in &[&sel_hotfix, &sel_rollback] {
            audit_repo
                .record_outcome(RecordOutcomeParams {
                    selection_id: sel,
                    outcome: djinn_db::AuditOutcomeKind::Miss,
                    miss_category: Some("logic_error"),
                    miss_severity: Some("medium"),
                    requires_rule_update: false,
                    notes: None,
                })
                .await
                .unwrap();
        }

        let result = query_revalidation_ground_truth(&audit_repo, &task_repo, &project_id)
            .await
            .unwrap();

        assert!(result.reopened_candidates.is_empty());
        assert_eq!(result.hotfix_rollback_candidates.len(), 2);

        let hotfix = result
            .hotfix_rollback_candidates
            .iter()
            .find(|c| c.signal == "hotfix")
            .expect("should find hotfix candidate");
        assert!(hotfix.detail.contains("hotfix"));

        let rollback = result
            .hotfix_rollback_candidates
            .iter()
            .find(|c| c.signal == "rollback")
            .expect("should find rollback candidate");
        assert!(rollback.detail.contains("rollback"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revalidation_empty_when_no_signals() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events);

        // Create a source task with no revalidation signals.
        let source_task_id = seed_audit_task(&db, &project_id, None, 0).await;

        let sel_id = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::UnflaggedMerged,
            "sha-clean-1",
            Some(&source_task_id),
        )
        .await;

        // Record a clean outcome.
        audit_repo
            .record_outcome(RecordOutcomeParams {
                selection_id: &sel_id,
                outcome: djinn_db::AuditOutcomeKind::Clean,
                miss_category: None,
                miss_severity: None,
                requires_rule_update: false,
                notes: None,
            })
            .await
            .unwrap();

        let result = query_revalidation_ground_truth(&audit_repo, &task_repo, &project_id)
            .await
            .unwrap();

        assert!(result.reopened_candidates.is_empty());
        assert!(result.hotfix_rollback_candidates.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revalidation_rule_update_count_in_report() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let project_id = seed_project(&db).await;
        let audit_repo = AuditSamplerRepository::new(db.clone());

        // Create 3 selections and record outcomes: 2 with requires_rule_update.
        let sel1 = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::UnflaggedMerged,
            "sha-ru-1",
            Some("task-ru-1"),
        )
        .await;
        let sel2 = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::UnflaggedMerged,
            "sha-ru-2",
            Some("task-ru-2"),
        )
        .await;
        let sel3 = seed_selection_for_outcome(
            &audit_repo,
            &project_id,
            AuditStratum::UnflaggedMerged,
            "sha-ru-3",
            Some("task-ru-3"),
        )
        .await;

        for (sel, needs_update) in &[(&sel1, true), (&sel2, true), (&sel3, false)] {
            audit_repo
                .record_outcome(RecordOutcomeParams {
                    selection_id: sel,
                    outcome: djinn_db::AuditOutcomeKind::Miss,
                    miss_category: Some("test"),
                    miss_severity: Some("low"),
                    requires_rule_update: *needs_update,
                    notes: None,
                })
                .await
                .unwrap();
        }

        let report = generate_false_negative_report(&audit_repo, &project_id)
            .await
            .unwrap();

        let uf = &report.strata[0];
        assert_eq!(uf.requires_rule_update_count, 2);
        assert_eq!(uf.total_outcomes, 3);
        assert_eq!(uf.total_misses, 3);
    }
}
