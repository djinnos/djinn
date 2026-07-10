//! Audit item scheduler: materializes selected audit records into ordinary
//! review tasks at a configurable cadence with max-open and SLO controls.
//!
//! The scheduler runs on the coordinator's leader-tick cadence and:
//!
//! 1. Queries for unmaterialized selections (from non-superseded frames
//!    where `audit_task_id IS NULL`).
//! 2. Enforces `max_open_audits` — if the count of open audit tasks meets or
//!    exceeds the cap, selection pauses and a typed warning event is emitted.
//! 3. Enforces SLO age — if any unmaterialized selection has been pending
//!    longer than `slo_age_hours`, selection pauses with an SLO warning.
//! 4. For each materializable selection (up to `per_tick_budget`), creates an
//!    ordinary review task with a provenance-rich description and links it
//!    back to the selection.
//!
//! Idempotency is guaranteed by the `audit_task_id IS NULL` filter: repeated
//! ticks never re-materialize an already-linked selection.
//!
//! ## Configuration
//!
//! [`AuditSchedulerConfig`] is injectable in tests. Production defaults:
//! - `max_open_audits`: 5
//! - `slo_age_hours`: 336 (14 days)
//! - `per_tick_budget`: 1
//! - `min_materialization_interval_hours`: 84 (roughly 2/week)
//! - `enabled`: true
//!
//! ## Events
//!
//! - `audit.task.materialized` — emitted for each successfully created audit
//!   task.
//! - `audit.backlog.pause` — emitted when the max-open or SLO cap would be
//!   exceeded, preserving unsampled state.

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use djinn_db::{AuditSamplerRepository, EpicRepository, TaskRepository};

// ── Configuration ────────────────────────────────────────────────────────────

/// Configuration for the audit scheduler. Injectable for testing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditSchedulerConfig {
    /// Whether the scheduler is enabled. When false, `run_audit_scheduler`
    /// returns immediately with no side effects.
    pub enabled: bool,
    /// Maximum number of open audit tasks across all projects. When reached,
    /// selection pauses with a typed warning event.
    pub max_open_audits: i64,
    /// Maximum age (in hours) for an unmaterialized selection before SLO
    /// overflow is declared. Set to 0 to disable the SLO check.
    pub slo_age_hours: i64,
    /// Maximum number of audit tasks to create per scheduler tick once the
    /// cadence gate has elapsed.
    pub per_tick_budget: usize,
    /// Minimum time between materializing audit tasks. Production defaults to
    /// 84 hours (roughly two audit tasks per week); tests may set this to 0 for
    /// immediate materialization or to a shorter interval.
    pub min_materialization_interval_hours: i64,
}

impl Default for AuditSchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_open_audits: 5,
            slo_age_hours: 14 * 24, // 14 days
            per_tick_budget: 1,
            min_materialization_interval_hours: 84,
        }
    }
}

// ── Event types ──────────────────────────────────────────────────────────────

/// Event type for a successfully materialized audit task.
pub const EVENT_AUDIT_MATERIALIZED: &str = "audit.task.materialized";

/// Event type for a backlog pause (max-open or SLO overflow).
pub const EVENT_BACKLOG_PAUSE: &str = "audit.backlog.pause";

// ── Pause reasons ────────────────────────────────────────────────────────────

/// Reason why the scheduler paused selection/materialization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BacklogPauseReason {
    /// The number of open audit tasks has reached or exceeded
    /// [`AuditSchedulerConfig::max_open_audits`].
    MaxOpenAudits,
    /// An unmaterialized selection has been pending longer than
    /// [`AuditSchedulerConfig::slo_age_hours`].
    SLOAgeExceeded,
}

impl std::fmt::Display for BacklogPauseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MaxOpenAudits => write!(f, "max_open_audits"),
            Self::SLOAgeExceeded => write!(f, "slo_age_exceeded"),
        }
    }
}

// ── Result types ─────────────────────────────────────────────────────────────

/// Result of running the audit scheduler.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuditSchedulerResult {
    /// Whether the scheduler was enabled and ran.
    pub ran: bool,
    /// Items successfully materialized into audit tasks.
    pub materialized_items: Vec<MaterializedAuditItem>,
    /// Whether materialization was paused due to backlog controls.
    pub paused: bool,
    /// Reason for the pause (if `paused` is true).
    pub pause_reason: Option<BacklogPauseReason>,
    /// Total number of unmaterialized selections seen (before cap filtering).
    pub total_unmaterialized: usize,
}

/// Details of a single materialized audit item.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaterializedAuditItem {
    /// The selection id from `audit_selections`.
    pub selection_id: String,
    /// The created audit task id.
    pub audit_task_id: String,
    /// The linked merged-change id.
    pub merged_change_id: String,
    /// The frame id the selection belongs to.
    pub frame_id: String,
    /// The stratum (unflagged_merged or autonomous_release).
    pub stratum: String,
}

// ── Scheduler ────────────────────────────────────────────────────────────────

/// Run the audit scheduler once (intended to be called from the coordinator's
/// leader-tick).
///
/// Materializes selected audit records into ordinary review tasks at a
/// configurable rate, enforcing max-open and SLO backlog controls. Overflow
/// emits a typed warning event and preserves unsampled state.
///
/// Idempotent across repeated ticks: only selections with
/// `audit_task_id IS NULL` from non-superseded frames are considered.
pub async fn run_audit_scheduler(
    config: &AuditSchedulerConfig,
    audit_repo: &AuditSamplerRepository,
    task_repo: &TaskRepository,
    epic_repo: &EpicRepository,
) -> AuditSchedulerResult {
    let mut result = AuditSchedulerResult::default();

    if !config.enabled {
        info!("audit scheduler: disabled, skipping");
        return result;
    }

    result.ran = true;

    // 1. Query unmaterialized selections.
    let unmaterialized = match audit_repo.list_unmaterialized_selections().await {
        Ok(rows) => rows,
        Err(e) => {
            warn!(error = %e, "audit scheduler: failed to list unmaterialized selections");
            return result;
        }
    };

    result.total_unmaterialized = unmaterialized.len();

    if unmaterialized.is_empty() {
        info!("audit scheduler: no unmaterialized selections");
        return result;
    }

    // 2. Enforce max-open-audits cap.
    if config.max_open_audits > 0 {
        match audit_repo.count_open_audit_tasks().await {
            Ok(open_count) => {
                if open_count >= config.max_open_audits {
                    warn!(
                        open_count,
                        max = config.max_open_audits,
                        "audit scheduler: max-open-audits cap reached; pausing materialization"
                    );
                    result.paused = true;
                    result.pause_reason = Some(BacklogPauseReason::MaxOpenAudits);
                    emit_backlog_pause_event(
                        task_repo,
                        &BacklogPauseReason::MaxOpenAudits,
                        open_count,
                        config.max_open_audits,
                        result.total_unmaterialized,
                    )
                    .await;
                    return result;
                }
            }
            Err(e) => {
                warn!(error = %e, "audit scheduler: failed to count open audit tasks");
                return result;
            }
        }
    }

    // 3. Enforce SLO age for the oldest unmaterialized selection.
    if config.slo_age_hours > 0
        && let Some(oldest) = unmaterialized.first()
        && let Some(pause_reason) =
            check_slo_age(&oldest.selection_created_at, config.slo_age_hours)
    {
        warn!(
            selection_id = %oldest.selection_id,
            created_at = %oldest.selection_created_at,
            slo_hours = config.slo_age_hours,
            "audit scheduler: SLO age exceeded; pausing materialization"
        );
        result.paused = true;
        result.pause_reason = Some(pause_reason);
        emit_backlog_pause_event(
            task_repo,
            &BacklogPauseReason::SLOAgeExceeded,
            0,
            config.max_open_audits,
            result.total_unmaterialized,
        )
        .await;
        return result;
    }

    // 4. Enforce persisted cadence/rate gate after backlog checks. SLO and
    // max-open warnings intentionally still fire even when cadence has not
    // elapsed.
    if cadence_not_elapsed(config, audit_repo).await {
        info!(
            min_interval_hours = config.min_materialization_interval_hours,
            "audit scheduler: cadence has not elapsed; preserving unmaterialized selections"
        );
        return result;
    }

    // 5. Materialize up to per_tick_budget selections.
    let budget = config.per_tick_budget.max(1);
    for item in unmaterialized.iter().take(budget) {
        match materialize_one(audit_repo, task_repo, epic_repo, item).await {
            Ok(mat_item) => {
                info!(
                    selection_id = %mat_item.selection_id,
                    audit_task_id = %mat_item.audit_task_id,
                    stratum = %mat_item.stratum,
                    "audit scheduler: materialized audit task"
                );
                result.materialized_items.push(mat_item);
            }
            Err(e) => {
                warn!(
                    selection_id = %item.selection_id,
                    error = %e,
                    "audit scheduler: failed to materialize selection; continuing"
                );
            }
        }
    }

    result
}

// ── Cadence check ────────────────────────────────────────────────────────────

/// Return true when the persisted materialization cadence has not elapsed.
async fn cadence_not_elapsed(
    config: &AuditSchedulerConfig,
    audit_repo: &AuditSamplerRepository,
) -> bool {
    if config.min_materialization_interval_hours <= 0 {
        return false;
    }

    let latest = match audit_repo.latest_audit_materialized_at().await {
        Ok(latest) => latest,
        Err(e) => {
            warn!(error = %e, "audit scheduler: failed to read latest materialization timestamp");
            return true;
        }
    };

    let Some(latest) = latest else {
        return false;
    };
    let Some(latest_secs) = parse_iso_timestamp(&latest) else {
        warn!(latest_materialized_at = %latest, "audit scheduler: could not parse latest materialization timestamp; allowing materialization");
        return false;
    };

    let elapsed_hours = chrono_now_utc().saturating_sub(latest_secs) / 3600;
    elapsed_hours < config.min_materialization_interval_hours as u64
}

// ── SLO check ────────────────────────────────────────────────────────────────

/// Check if a selection's age exceeds the SLO limit.
///
/// Returns `Some(BacklogPauseReason::SLOAgeExceeded)` if the selection has
/// been pending longer than `slo_age_hours`, `None` otherwise.
fn check_slo_age(created_at: &str, slo_age_hours: i64) -> Option<BacklogPauseReason> {
    // Parse ISO 8601 timestamp. We compare against a naive UTC timestamp.
    let created = parse_iso_timestamp(created_at)?;
    let now = chrono_now_utc();
    let age_hours = now.saturating_sub(created) / 3600;
    if age_hours >= slo_age_hours as u64 {
        Some(BacklogPauseReason::SLOAgeExceeded)
    } else {
        None
    }
}

/// Parse an ISO 8601 timestamp string into seconds since epoch.
///
/// Handles both `2026-07-01T00:00:00Z` and `2026-07-01T00:00:00.000Z` formats.
fn parse_iso_timestamp(ts: &str) -> Option<u64> {
    // Strip trailing Z if present.
    let ts = ts.trim_end_matches('Z');
    // Try parsing with and without fractional seconds.
    let formats = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ];
    for fmt in &formats {
        if let Some(dt) = chrono_parse(ts, fmt) {
            return Some(dt);
        }
    }
    None
}

/// Simple chrono-like parse using standard library only.
///
/// Returns seconds since Unix epoch for the given timestamp.
fn chrono_parse(ts: &str, _fmt: &str) -> Option<u64> {
    // Use a simple manual parser for the common ISO 8601 format.
    // This avoids adding chrono as a dependency.
    let parts: Vec<&str> = ts.split('T').collect();
    if parts.len() != 2 {
        return None;
    }
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let time_parts: Vec<&str> = parts[1].split(':').collect();
    if time_parts.len() < 3 {
        return None;
    }

    let year: u64 = date_parts[0].parse().ok()?;
    let month: u64 = date_parts[1].parse().ok()?;
    let day: u64 = date_parts[2].parse().ok()?;
    let hour: u64 = time_parts[0].parse().ok()?;
    let minute: u64 = time_parts[1].parse().ok()?;
    // Seconds may have fractional part.
    let sec_str = time_parts[2].split('.').next().unwrap_or(time_parts[2]);
    let second: u64 = sec_str.parse().ok()?;

    // Compute days since epoch using a simple algorithm.
    let days = days_since_epoch(year, month, day)?;
    let seconds = days * 86400 + hour * 3600 + minute * 60 + second;
    Some(seconds)
}

/// Compute days since Unix epoch (1970-01-01) for a given date.
fn days_since_epoch(year: u64, month: u64, day: u64) -> Option<u64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days in each month (non-leap year).
    let days_in_month = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = |y: u64| (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);

    let mut total_days = 0u64;
    for y in 1970..year {
        total_days += if is_leap(y) { 366 } else { 365 };
    }
    for m in 1..month {
        let mut d = days_in_month[m as usize];
        if m == 2 && is_leap(year) {
            d = 29;
        }
        total_days += d;
    }
    Some(total_days + day - 1)
}

/// Get current UTC time as seconds since epoch.
///
/// Uses the `djinn-core` clock abstraction so tests can inject a fixed clock.
fn chrono_now_utc() -> u64 {
    use djinn_core::clock::{Clock, SystemClock};
    SystemClock::new()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Task materialization ─────────────────────────────────────────────────────

/// Materialize a single unmaterialized selection into an audit review task.
///
/// Creates an ordinary review task with provenance data, links the task id
/// back to the selection, and emits an activity event.
async fn materialize_one(
    audit_repo: &AuditSamplerRepository,
    task_repo: &TaskRepository,
    epic_repo: &EpicRepository,
    item: &djinn_db::UnmaterializedSelection,
) -> Result<MaterializedAuditItem, String> {
    // Resolve or create the audit epic for this project.
    let epic_id = ensure_audit_epic(epic_repo, &item.project_id)
        .await
        .map_err(|e| format!("failed to ensure audit epic: {e}"))?;

    // Build the provenance-rich task description.
    let description = build_audit_task_description(item, audit_repo).await;

    // Create the review task and link the selection atomically in a single
    // database transaction. This guarantees restart-idempotency: if the
    // coordinator crashes after the task INSERT but before the selection
    // UPDATE, the transaction rolls back and the next tick retries cleanly.
    let title = format!(
        "Audit review: {} ({})",
        &item.merge_commit_sha[..8.min(item.merge_commit_sha.len())],
        item.stratum
    );
    let audit_task_id = audit_repo
        .materialize_audit_task_atomic(
            &item.selection_id,
            &item.project_id,
            Some(&epic_id),
            &title,
            &description,
        )
        .await
        .map_err(|e| format!("failed to materialize audit task atomically: {e}"))?;

    // Emit activity event.
    let payload = serde_json::json!({
        "event_type": EVENT_AUDIT_MATERIALIZED,
        "selection_id": item.selection_id,
        "audit_task_id": audit_task_id,
        "merged_change_id": item.merged_change_id,
        "frame_id": item.frame_id,
        "stratum": item.stratum,
        "merge_commit_sha": item.merge_commit_sha,
        "project_id": item.project_id,
    });
    let _ = task_repo
        .log_activity(
            Some(&audit_task_id),
            "coordinator",
            "system",
            EVENT_AUDIT_MATERIALIZED,
            &payload.to_string(),
        )
        .await;

    Ok(MaterializedAuditItem {
        selection_id: item.selection_id.clone(),
        audit_task_id,
        merged_change_id: item.merged_change_id.clone(),
        frame_id: item.frame_id.clone(),
        stratum: item.stratum.clone(),
    })
}

// ── Epic resolution ──────────────────────────────────────────────────────────

/// Ensure an audit epic exists for the given project. If no epic titled
/// "Audit reviews" exists for the project, one is created.
async fn ensure_audit_epic(
    epic_repo: &EpicRepository,
    project_id: &str,
) -> Result<String, djinn_db::Error> {
    // Check for existing audit epic.
    let epics = epic_repo.list_for_project(project_id).await?;
    if let Some(existing) = epics.iter().find(|e| e.title == AUDIT_EPIC_TITLE) {
        return Ok(existing.id.clone());
    }

    // Create a new audit epic for this project.
    let epic = epic_repo
        .create_for_project(
            project_id,
            djinn_db::EpicCreateInput {
                title: AUDIT_EPIC_TITLE,
                description: "Auto-created epic for audit sampler review tasks. \
                    Each task represents an independently selected merged change \
                    for operator review.",
                emoji: "🔍",
                color: "#FF6B35",
                owner: "",
                memory_refs: None,
                status: None,
                auto_breakdown: Some(false),
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await?;
    Ok(epic.id)
}

/// Title used for auto-created audit epics.
const AUDIT_EPIC_TITLE: &str = "Audit reviews";

// ── Task description builder ─────────────────────────────────────────────────

/// Build a provenance-rich description for an audit review task.
///
/// The description includes all data needed by an operator reviewer to
/// independently verify the selection and review the merged change.
async fn build_audit_task_description(
    item: &djinn_db::UnmaterializedSelection,
    audit_repo: &AuditSamplerRepository,
) -> String {
    let seed_status = if item.seed_reveal.is_some() {
        "revealed"
    } else {
        "committed (not yet revealed)"
    };

    // Fetch the policy revision from the frame's policy.
    let policy_revision = match audit_repo
        .get_sample_policy_by_id(&item.frame_policy_id)
        .await
    {
        Ok(Some(policy)) => policy.revision,
        _ => 0,
    };

    // Format gate and release provenance.
    let gate_provenance_str = item
        .gate_provenance
        .as_ref()
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()))
        .unwrap_or_else(|| "none".to_string());
    let release_provenance_str = item
        .release_provenance
        .as_ref()
        .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()))
        .unwrap_or_else(|| "none".to_string());

    format!(
        r#"## Audit Review — Independent spot-check

This task was auto-created by the audit sampler scheduler. Review the
merged change below and record your findings.

### Source

| Field | Value |
|---|---|
| **Project ID** | `{project_id}` |
| **Source task** | `{task_id}` |
| **PR** | `{pr_number}` |
| **Merge commit SHA** | `{merge_sha}` |
| **Head SHA** | `{head_sha}` |

### Sampling metadata

| Field | Value |
|---|---|
| **Stratum** | `{stratum}` |
| **Frame ID** | `{frame_id}` |
| **Frame revision** | {frame_rev} |
| **Policy revision** | {policy_rev} |
| **Window** | `{window_start}` → `{window_end}` |

### Replay / randomness

| Field | Value |
|---|---|
| **Algorithm** | `{algorithm}` |
| **Seed commitment** | `{seed_commitment}` |
| **Seed status** | {seed_status} |
| **Selected position** | {position} |
| **Replay data** | `{replay_data}` |

### Tripwire / gate provenance

**Gate outcome:** `{gate_outcome}`

<details><summary>Gate provenance</summary>

```json
{gate_provenance}
```

</details>

<details><summary>Release provenance</summary>

```json
{release_provenance}
```

</details>

### Instructions

1. Check out the merge commit `{merge_sha}` and review the changes.
2. Verify the tripwire gate/release decision was correct.
3. Record your audit outcome using the audit-outcome tool.
"#,
        project_id = item.project_id,
        task_id = item.task_id.as_deref().unwrap_or("n/a"),
        pr_number = item
            .pr_number
            .map(|n| n.to_string())
            .unwrap_or_else(|| "n/a".to_string()),
        merge_sha = item.merge_commit_sha,
        head_sha = item.head_sha.as_deref().unwrap_or("n/a"),
        stratum = item.stratum,
        frame_id = item.frame_id,
        frame_rev = item.frame_revision,
        policy_rev = policy_revision,
        window_start = item.window_start,
        window_end = item.window_end,
        algorithm = item.algorithm,
        seed_commitment = item.seed_commitment,
        seed_status = seed_status,
        position = item.selected_position,
        replay_data = serde_json::to_string(&item.replay_data).unwrap_or_else(|_| "{}".to_string()),
        gate_outcome = item.gate_outcome,
        gate_provenance = gate_provenance_str,
        release_provenance = release_provenance_str,
    )
}

// ── Backlog pause event ──────────────────────────────────────────────────────

/// Emit a typed backlog-pause/warning activity event.
async fn emit_backlog_pause_event(
    task_repo: &TaskRepository,
    reason: &BacklogPauseReason,
    open_count: i64,
    max_open: i64,
    pending_count: usize,
) {
    let payload = serde_json::json!({
        "event_type": EVENT_BACKLOG_PAUSE,
        "reason": reason.to_string(),
        "open_audit_tasks": open_count,
        "max_open_audits": max_open,
        "pending_unmaterialized": pending_count,
    });
    if let Err(e) = task_repo
        .log_activity(
            None, // board-wide observation, not tied to a specific task
            "coordinator",
            "system",
            EVENT_BACKLOG_PAUSE,
            &payload.to_string(),
        )
        .await
    {
        warn!(error = %e, "audit scheduler: failed to emit backlog-pause event");
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;
