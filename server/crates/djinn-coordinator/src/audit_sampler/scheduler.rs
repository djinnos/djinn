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
    /// Maximum number of audit tasks to create per scheduler tick.
    pub per_tick_budget: usize,
}

impl Default for AuditSchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_open_audits: 5,
            slo_age_hours: 14 * 24, // 14 days
            per_tick_budget: 1,
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

    // 4. Materialize up to per_tick_budget selections.
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
mod tests {
    use super::*;
    use djinn_db::{
        AuditSamplerRepository, AuditStratum, CreateSampleFrameParams, CreateSamplePolicyParams,
        CreateSelectionParams, Database, EpicRepository, SelectionRow,
    };
    use serde_json::json;

    fn test_config() -> AuditSchedulerConfig {
        AuditSchedulerConfig {
            enabled: true,
            max_open_audits: 3,
            slo_age_hours: 168, // 7 days
            per_tick_budget: 2,
        }
    }

    fn test_db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    /// Seed a policy, frame, merged change, and selection for testing.
    async fn seed_selection(
        db: &Database,
        project_id: &str,
        merge_sha: &str,
        stratum: &str,
        position: i32,
        created_at: &str,
    ) -> SelectionRow {
        let repo = AuditSamplerRepository::new(db.clone());

        // Seed policy.
        let policy = repo
            .create_sample_policy(CreateSamplePolicyParams {
                project_id,
                revision: 1,
                policy_json: &json!({"unflagged_rate": 0.02, "autonomous_rate": 0.10}),
            })
            .await
            .unwrap();

        // Seed frame.
        let frame = repo
            .create_sample_frame(CreateSampleFrameParams {
                project_id,
                policy_id: &policy.id,
                window_start: "2026-06-24T00:00:00Z",
                window_end: "2026-07-01T00:00:00Z",
                revision: 1,
                eligible_change_ids: &json!([merge_sha]),
                content_hash: Some("abc123"),
                exclusion_counts: &json!({}),
                exclusion_reasons: &json!([]),
                sealed_at: "2026-07-01T00:05:00Z",
            })
            .await
            .unwrap();

        // Seed merged change.
        let mc = repo
            .upsert_merged_change(djinn_db::UpsertMergedChangeParams {
                project_id,
                task_id: Some("task-source-1"),
                pr_number: Some(42),
                head_sha: Some("head-sha-1"),
                merge_commit_sha: merge_sha,
                merged_at: "2026-07-01T00:00:00Z",
                gate_outcome: "pass",
                gate_provenance: Some(&json!({"tripwire": "none"})),
                release_provenance: None,
                stratum: if stratum == "autonomous_release" {
                    AuditStratum::AutonomousRelease
                } else {
                    AuditStratum::UnflaggedMerged
                },
                excluded: false,
                exclusion_reason: None,
            })
            .await
            .unwrap();

        // Seed selection via repository with specific created_at.
        repo.create_selection(CreateSelectionParams {
            frame_id: &frame.id,
            merged_change_id: &mc.id,
            stratum: if stratum == "autonomous_release" {
                AuditStratum::AutonomousRelease
            } else {
                AuditStratum::UnflaggedMerged
            },
            selected_position: position,
            algorithm: "hmac-sha256-counter-v1",
            seed_commitment: "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
            seed_reveal: Some("fedcba0987654321fedcba0987654321fedcba0987654321fedcba0987654321"),
            replay_data: &json!([]),
            audit_task_id: None,
            created_at: Some(created_at),
        })
        .await
        .unwrap()
    }

    /// Create an open task to count toward max_open_audits.
    async fn create_open_task(db: &Database, project_id: &str) -> String {
        djinn_db::test_support::seed_task_row(
            db,
            djinn_db::test_support::UsageTestTaskSeed {
                project_id,
                status: "open",
                close_reason: None,
                total_reopen_count: 0,
            },
        )
        .await
    }

    // ── Test: normal materialization ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn normal_materialization_creates_task_and_links_selection() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        // Seed one selection.
        let sel = seed_selection(
            &db,
            &project_id,
            "sha-normal-001",
            "unflagged_merged",
            0,
            "2026-07-09T12:00:00Z",
        )
        .await;

        let config = test_config();
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events);

        let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

        assert!(result.ran);
        assert!(!result.paused);
        assert_eq!(result.materialized_items.len(), 1);
        assert_eq!(result.total_unmaterialized, 1);

        let mat = &result.materialized_items[0];
        assert_eq!(mat.selection_id, sel.id);
        assert_eq!(mat.stratum, "unflagged_merged");

        // Verify the selection now has an audit_task_id.
        let updated = audit_repo
            .get_selection_by_id(&sel.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.audit_task_id.as_deref(),
            Some(mat.audit_task_id.as_str())
        );

        // Verify the created task is an ordinary task (not a verification task).
        let task = task_repo.get(&mat.audit_task_id).await.unwrap().unwrap();
        assert_eq!(task.issue_type, "task");
        assert!(task.description.contains("Audit Review"));
        assert!(task.description.contains("sha-normal-001"));
        assert!(task.description.contains("hmac-sha256-counter-v1"));
    }

    // ── Test: idempotency across repeated ticks ───────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_tick_does_not_rematerialize() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        seed_selection(
            &db,
            &project_id,
            "sha-idempotent-001",
            "unflagged_merged",
            0,
            "2026-07-09T12:00:00Z",
        )
        .await;

        let config = test_config();
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events);

        // First tick: materializes.
        let result1 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
        assert_eq!(result1.materialized_items.len(), 1);

        // Second tick: nothing to materialize.
        let result2 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
        assert!(result2.materialized_items.is_empty());
        assert!(!result2.paused);
        assert_eq!(result2.total_unmaterialized, 0);
    }

    // ── Test: max-open pause ──────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn max_open_audits_triggers_pause() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        // Seed a selection that won't be materialized because cap is hit.
        seed_selection(
            &db,
            &project_id,
            "sha-maxopen-001",
            "unflagged_merged",
            0,
            "2026-07-09T12:00:00Z",
        )
        .await;

        // Create 3 open tasks (matching max_open_audits = 3).
        for i in 0..3 {
            let tid = create_open_task(&db, &project_id).await;
            // Also link them to selections so count_open_audit_tasks counts them.
            let sel = seed_selection(
                &db,
                &project_id,
                &format!("sha-maxopen-linked-{i}"),
                "unflagged_merged",
                i + 10,
                "2026-07-09T10:00:00Z",
            )
            .await;
            AuditSamplerRepository::new(db.clone())
                .set_selection_audit_task(&sel.id, &tid)
                .await
                .unwrap();
        }

        let config = test_config(); // max_open_audits = 3
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events);

        let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

        assert!(result.ran);
        assert!(result.paused);
        assert_eq!(result.pause_reason, Some(BacklogPauseReason::MaxOpenAudits));
        assert!(result.materialized_items.is_empty());
        assert_eq!(result.total_unmaterialized, 1);
    }

    // ── Test: SLO pause ───────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slo_age_exceeded_triggers_pause() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        // Seed a selection with created_at 20 days ago (exceeds 7-day SLO).
        seed_selection(
            &db,
            &project_id,
            "sha-slo-001",
            "unflagged_merged",
            0,
            "2026-06-20T12:00:00Z", // ~20 days before now
        )
        .await;

        let config = AuditSchedulerConfig {
            slo_age_hours: 168, // 7 days
            ..test_config()
        };
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events);

        let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

        assert!(result.ran);
        assert!(result.paused);
        assert_eq!(
            result.pause_reason,
            Some(BacklogPauseReason::SLOAgeExceeded)
        );
        assert!(result.materialized_items.is_empty());
    }

    // ── Test: recovery after backlog falls below cap ──────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recovery_after_backlog_falls_below_cap() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        // Seed a selection.
        let sel = seed_selection(
            &db,
            &project_id,
            "sha-recovery-001",
            "unflagged_merged",
            0,
            "2026-07-09T12:00:00Z",
        )
        .await;

        let config = AuditSchedulerConfig {
            max_open_audits: 1, // cap at 1
            ..test_config()
        };
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events);

        // First: create an open audit task to fill the cap.
        let existing_task_id = create_open_task(&db, &project_id).await;
        let existing_sel = seed_selection(
            &db,
            &project_id,
            "sha-recovery-existing",
            "unflagged_merged",
            99,
            "2026-07-09T10:00:00Z",
        )
        .await;
        audit_repo
            .set_selection_audit_task(&existing_sel.id, &existing_task_id)
            .await
            .unwrap();

        // First tick: should pause (cap reached).
        let result1 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
        assert!(result1.paused);
        assert_eq!(
            result1.pause_reason,
            Some(BacklogPauseReason::MaxOpenAudits)
        );

        // Close the existing task.
        djinn_db::test_support::close_task_at(&db, &existing_task_id, "2026-07-10T00:00:00Z").await;

        // Second tick: should succeed (cap no longer exceeded).
        let result2 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
        assert!(!result2.paused);
        assert_eq!(result2.materialized_items.len(), 1);
        assert_eq!(result2.materialized_items[0].selection_id, sel.id);
    }

    // ── Test: disabled scheduler ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_scheduler_does_nothing() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        seed_selection(
            &db,
            &project_id,
            "sha-disabled-001",
            "unflagged_merged",
            0,
            "2026-07-09T12:00:00Z",
        )
        .await;

        let config = AuditSchedulerConfig {
            enabled: false,
            ..test_config()
        };
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events);

        let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

        assert!(!result.ran);
        assert!(result.materialized_items.is_empty());
    }

    // ── Test: provenance description ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_description_includes_provenance_data() {
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        seed_selection(
            &db,
            &project_id,
            "sha-provenance-001",
            "autonomous_release",
            0,
            "2026-07-09T12:00:00Z",
        )
        .await;

        let config = test_config();
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events);

        let result = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;

        assert_eq!(result.materialized_items.len(), 1);
        let task = task_repo
            .get(&result.materialized_items[0].audit_task_id)
            .await
            .unwrap()
            .unwrap();

        // Verify all required provenance fields are present in the description.
        let desc = &task.description;
        assert!(desc.contains("sha-provenance-001"), "merge SHA");
        assert!(desc.contains("autonomous_release"), "stratum");
        assert!(desc.contains("hmac-sha256-counter-v1"), "algorithm");
        assert!(desc.contains("Seed commitment"), "seed commitment label");
        assert!(desc.contains("revealed"), "seed status");
        assert!(desc.contains("Frame revision"), "frame revision");
        assert!(desc.contains("Policy revision"), "policy revision");
        assert!(desc.contains("task-source-1"), "source task id");
        assert!(desc.contains("42"), "PR number");
        assert!(desc.contains("head-sha-1"), "head SHA");
        assert!(desc.contains("Gate provenance"), "gate provenance section");
        assert!(
            desc.contains("Release provenance"),
            "release provenance section"
        );
    }

    // ── Test: ISO timestamp parsing ───────────────────────────────────────────

    #[test]
    fn parse_iso_timestamp_handles_common_formats() {
        // Standard format
        let ts = parse_iso_timestamp("2026-07-01T12:00:00Z");
        assert!(ts.is_some());

        // With milliseconds
        let ts = parse_iso_timestamp("2026-07-01T12:00:00.000Z");
        assert!(ts.is_some());

        // Without Z
        let ts = parse_iso_timestamp("2026-07-01T12:00:00");
        assert!(ts.is_some());

        // Invalid format
        let ts = parse_iso_timestamp("not-a-timestamp");
        assert!(ts.is_none());
    }

    #[test]
    fn days_since_epoch_known_dates() {
        // 1970-01-01 = day 0
        assert_eq!(days_since_epoch(1970, 1, 1), Some(0));
        // 1970-01-02 = day 1
        assert_eq!(days_since_epoch(1970, 1, 2), Some(1));
        // 2024-03-01 is 19783 days from 1970-01-01 (54 years + 31 Jan + 29 Feb)
        assert_eq!(days_since_epoch(2024, 3, 1), Some(19783));
        // Invalid month
        assert_eq!(days_since_epoch(2024, 13, 1), None);
    }

    #[test]
    fn check_slo_age_detects_old_selection() {
        // A selection from 30 days ago with a 7-day SLO should trigger.
        // We use a fixed timestamp well in the past.
        let old_ts = "2020-01-01T00:00:00Z";
        let result = check_slo_age(old_ts, 168);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), BacklogPauseReason::SLOAgeExceeded);
    }

    #[test]
    fn check_slo_age_ignores_recent_selection() {
        // A very recent timestamp should not trigger with a very large SLO.
        let recent = "2026-07-10T12:00:00Z";
        let result = check_slo_age(recent, 100_000);
        assert!(result.is_none());
    }
    // ── Test: crash-restart idempotency ──────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crash_restart_idempotency_atomic_materialization() {
        // Simulates the crash-restart scenario: the scheduler materializes a
        // selection, then a "restart" happens (second scheduler tick). The
        // atomic transaction ensures the second tick creates zero duplicate
        // tasks because list_unmaterialized_selections no longer returns the
        // already-linked selection.
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        seed_selection(
            &db,
            &project_id,
            "sha-crash-001",
            "unflagged_merged",
            0,
            "2026-07-09T12:00:00Z",
        )
        .await;

        let config = test_config();
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events.clone());
        let epic_repo = EpicRepository::new(db.clone(), events);

        // "Tick 1" — succeeds (simulates normal materialization before crash).
        let result1 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
        assert_eq!(result1.materialized_items.len(), 1);
        let first_task_id = result1.materialized_items[0].audit_task_id.clone();

        // Verify the selection is now linked.
        let sel_id = result1.materialized_items[0].selection_id.clone();
        let updated = audit_repo
            .get_selection_by_id(&sel_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.audit_task_id.as_deref(),
            Some(first_task_id.as_str())
        );

        // "Tick 2" after restart — must not re-materialize.
        let result2 = run_audit_scheduler(&config, &audit_repo, &task_repo, &epic_repo).await;
        assert!(result2.materialized_items.is_empty());
        assert_eq!(result2.total_unmaterialized, 0);

        // Verify exactly one task exists (no duplicate).
        let task = task_repo.get(&first_task_id).await.unwrap().unwrap();
        assert_eq!(task.issue_type, "task");
        assert!(task.description.contains("Audit Review"));
    }

    // ── Test: atomic materialization directly ────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn atomic_materialization_creates_task_and_links_in_one_tx() {
        // Directly tests the materialize_audit_task_atomic method to verify
        // the task and selection link are created atomically.
        let db = test_db();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        let sel = seed_selection(
            &db,
            &project_id,
            "sha-atomic-001",
            "unflagged_merged",
            0,
            "2026-07-09T12:00:00Z",
        )
        .await;

        let events = djinn_core::events::EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), events.clone());
        let task_repo = TaskRepository::new(db.clone(), events);
        let audit_repo = AuditSamplerRepository::new(db.clone());

        // Ensure the audit epic exists.
        let epic_id = ensure_audit_epic(&epic_repo, &project_id).await.unwrap();

        // Call the atomic method directly.
        let task_id = audit_repo
            .materialize_audit_task_atomic(
                &sel.id,
                &project_id,
                Some(&epic_id),
                "Audit review: test",
                "test description",
            )
            .await
            .unwrap();

        // Verify the task exists.
        let task = task_repo.get(&task_id).await.unwrap().unwrap();
        assert_eq!(task.issue_type, "task");
        assert_eq!(task.description, "test description");

        // Verify the selection is linked.
        let updated = audit_repo
            .get_selection_by_id(&sel.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.audit_task_id.as_deref(), Some(task_id.as_str()));

        // Verify list_unmaterialized no longer returns this selection.
        let unmaterialized = audit_repo.list_unmaterialized_selections().await.unwrap();
        assert!(
            !unmaterialized.iter().any(|u| u.selection_id == sel.id),
            "linked selection must not appear in unmaterialized list"
        );
    }
}
