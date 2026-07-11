//! Audit outcome recording with typed activity event emission.
//!
//! This module provides the coordinator-level function to record an audit
//! outcome for a selected audit item. It wraps [`AuditSamplerRepository`]
//! with:
//!
//! - Typed [`EVENT_OUTCOME_RECORDED`] activity events following ADR-020
//!   payload conventions.
//! - Links to the full provenance chain: merged-change id, frame revision,
//!   selection id, and audit task id.
//!
//! ## Design
//!
//! The recording function is a thin coordinator-layer wrapper: it delegates
//! storage to [`AuditSamplerRepository::record_outcome`] and event emission
//! to [`TaskRepository::log_activity`]. No additional state machine or
//! retry logic is needed because:
//!
//! - The `audit_outcomes` table has a UNIQUE constraint on `selection_id`,
//!   preventing duplicate outcomes at the DB layer.
//! - Activity logging is fire-and-forget (best-effort) — a failed event
//!   emission does not roll back the persisted outcome.
//!
//! ## Typed payload: `audit.outcome.recorded`
//!
//! ```json
//! {
//!   "event_type": "audit.outcome.recorded",
//!   "outcome_id": "<uuid>",
//!   "selection_id": "<uuid>",
//!   "audit_task_id": "<uuid or null>",
//!   "merged_change_id": "<uuid>",
//!   "frame_id": "<uuid>",
//!   "stratum": "unflagged_merged | autonomous_release",
//!   "outcome": "clean | miss",
//!   "miss_category": "<string or null>",
//!   "miss_severity": "<string or null>",
//!   "requires_rule_update": false,
//!   "actor": "<actor id>",
//!   "notes": "<string or null>",
//!   "recorded_at": "<iso8601>",
//!   "project_id": "<uuid>"
//! }
//! ```

use serde::{Deserialize, Serialize};
use tracing::warn;

use djinn_db::{
    AuditOutcomeKind, AuditOutcomeRow, AuditSamplerRepository, RecordOutcomeParams, SelectionRow,
    TaskRepository,
};

// ── Event type ───────────────────────────────────────────────────────────────

/// Event type for a recorded audit outcome.
pub const EVENT_OUTCOME_RECORDED: &str = "audit.outcome.recorded";

// ── Input type ───────────────────────────────────────────────────────────────

/// Parameters for recording an audit outcome at the coordinator level.
///
/// This is the public-facing input; it includes the actor field that
/// [`RecordOutcomeParams`] (the DB-layer input) does not carry.
pub struct RecordAuditOutcomeParams<'a> {
    /// The selection id this outcome pertains to.
    pub selection_id: &'a str,
    /// Clean or miss classification.
    pub outcome: AuditOutcomeKind,
    /// Category of the miss (e.g. `"missed_security_finding"`).
    /// Required when `outcome == Miss`, ignored when `outcome == Clean`.
    pub miss_category: Option<&'a str>,
    /// Severity of the miss (e.g. `"high"`, `"medium"`, `"low"`).
    /// Required when `outcome == Miss`, ignored when `outcome == Clean`.
    pub miss_severity: Option<&'a str>,
    /// Whether the miss indicates that a tripwire rule needs updating.
    pub requires_rule_update: bool,
    /// The actor recording the outcome (e.g. operator handle or system id).
    pub actor: &'a str,
    /// Optional free-text rationale or notes.
    pub notes: Option<&'a str>,
}

// ── Result type ──────────────────────────────────────────────────────────────

/// Result of recording an audit outcome, including the full provenance chain.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditOutcomeRecordResult {
    /// The persisted outcome row.
    pub outcome: AuditOutcomeRow,
    /// The selection this outcome is attached to.
    pub selection: SelectionRow,
    /// Whether the `audit.outcome.recorded` activity event was emitted
    /// successfully. A `false` value means event emission failed but the
    /// outcome was persisted — the caller should log the failure but not
    /// retry the DB write.
    pub event_emitted: bool,
}

// ── Record function ──────────────────────────────────────────────────────────

/// Record an audit outcome for a selected audit item.
///
/// This function:
///
/// 1. Looks up the selection to resolve provenance links (frame_id,
///    merged_change_id, stratum).
/// 2. Persists the outcome via [`AuditSamplerRepository::record_outcome`].
/// 3. Emits a typed [`EVENT_OUTCOME_RECORDED`] activity event with the full
///    provenance chain.
///
/// # Errors
///
/// Returns an error if the selection does not exist or if the outcome
/// INSERT fails (e.g. duplicate `selection_id`). Activity event emission
/// failures are logged but do not cause the function to return an error —
/// the outcome is still persisted.
pub async fn record_audit_outcome(
    audit_repo: &AuditSamplerRepository,
    task_repo: &TaskRepository,
    params: RecordAuditOutcomeParams<'_>,
) -> Result<AuditOutcomeRecordResult, String> {
    // 1. Look up the selection for provenance.
    let selection = audit_repo
        .get_selection_by_id(params.selection_id)
        .await
        .map_err(|e| format!("failed to look up selection: {e}"))?
        .ok_or_else(|| format!("selection not found: {}", params.selection_id))?;

    // 2. Look up the merged change for project_id.
    let merged_change = audit_repo
        .get_merged_change_by_id(&selection.merged_change_id)
        .await
        .map_err(|e| format!("failed to look up merged change: {e}"))?
        .ok_or_else(|| format!("merged change not found: {}", selection.merged_change_id))?;

    // 3. Persist the outcome.
    let outcome = audit_repo
        .record_outcome(RecordOutcomeParams {
            selection_id: params.selection_id,
            outcome: params.outcome.clone(),
            miss_category: params.miss_category,
            miss_severity: params.miss_severity,
            requires_rule_update: params.requires_rule_update,
            notes: params.notes,
        })
        .await
        .map_err(|e| format!("failed to record outcome: {e}"))?;

    // 4. Build and emit the typed activity event.
    let payload = serde_json::json!({
        "event_type": EVENT_OUTCOME_RECORDED,
        "outcome_id": outcome.id,
        "selection_id": outcome.selection_id,
        "audit_task_id": selection.audit_task_id,
        "merged_change_id": selection.merged_change_id,
        "frame_id": selection.frame_id,
        "stratum": selection.stratum,
        "outcome": outcome.outcome,
        "miss_category": outcome.miss_category,
        "miss_severity": outcome.miss_severity,
        "requires_rule_update": outcome.requires_rule_update,
        "actor": params.actor,
        "notes": outcome.notes,
        "recorded_at": outcome.recorded_at,
        "project_id": merged_change.project_id,
    });

    let event_emitted = match task_repo
        .log_activity(
            selection.audit_task_id.as_deref(),
            params.actor,
            "coordinator",
            EVENT_OUTCOME_RECORDED,
            &payload.to_string(),
        )
        .await
    {
        Ok(_) => true,
        Err(e) => {
            warn!(
                error = %e,
                outcome_id = %outcome.id,
                "audit outcome: failed to emit outcome-recorded event"
            );
            false
        }
    };

    Ok(AuditOutcomeRecordResult {
        outcome,
        selection,
        event_emitted,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use djinn_db::{
        AuditStratum, CreateSampleFrameParams, CreateSamplePolicyParams, CreateSelectionParams,
        UpsertMergedChangeParams,
    };

    /// Create a full test fixture: project + policy + merged change + frame +
    /// selection + repos. Returns (db, audit_repo, task_repo, selection_id, project_id).
    async fn setup_test_fixture(
        stratum: AuditStratum,
    ) -> (
        djinn_db::Database,
        AuditSamplerRepository,
        TaskRepository,
        String, // selection_id
        String, // project_id
    ) {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let project_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_project(&db, &project_id, &format!("proj-{project_id}")).await;

        let audit_repo = AuditSamplerRepository::new(db.clone());

        // Create policy
        let policy = audit_repo
            .create_sample_policy(CreateSamplePolicyParams {
                project_id: &project_id,
                revision: 1,
                policy_json: &json!({"unflagged_rate": 0.1, "autonomous_rate": 0.5}),
            })
            .await
            .unwrap();

        // Create merged change
        let change = audit_repo
            .upsert_merged_change(UpsertMergedChangeParams {
                project_id: &project_id,
                task_id: Some("task-test"),
                pr_number: Some(42),
                head_sha: Some("head-sha"),
                merge_commit_sha: &format!("merge-sha-{}", &uuid::Uuid::now_v7().to_string()[..8]),
                merged_at: "2026-06-28T00:00:00Z",
                gate_outcome: "pass",
                gate_provenance: Some(&json!({"tripwire": "none"})),
                release_provenance: None,
                stratum: stratum.clone(),
                excluded: false,
                exclusion_reason: None,
            })
            .await
            .unwrap();

        // Create frame
        let frame = audit_repo
            .create_sample_frame(CreateSampleFrameParams {
                project_id: &project_id,
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

        // Create selection
        let sel = audit_repo
            .create_selection(CreateSelectionParams {
                frame_id: &frame.id,
                merged_change_id: &change.id,
                stratum: stratum.clone(),
                selected_position: 0,
                algorithm: "hmac-sha256-counter-v1",
                seed_commitment: &"aa".repeat(32),
                seed_reveal: None,
                replay_data: &json!({"counter_seq": [0]}),
                audit_task_id: Some("audit-task-test"),
                created_at: None,
            })
            .await
            .unwrap();

        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db.clone(), events);

        (db, audit_repo, task_repo, sel.id, project_id)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_clean_outcome() {
        let (_db, audit_repo, task_repo, selection_id, _project_id) =
            setup_test_fixture(AuditStratum::UnflaggedMerged).await;

        let result = record_audit_outcome(
            &audit_repo,
            &task_repo,
            RecordAuditOutcomeParams {
                selection_id: &selection_id,
                outcome: AuditOutcomeKind::Clean,
                miss_category: None,
                miss_severity: None,
                requires_rule_update: false,
                actor: "test-operator",
                notes: Some("Reviewed and looks good"),
            },
        )
        .await
        .unwrap();

        assert_eq!(result.outcome.outcome, "clean");
        assert!(result.outcome.miss_category.is_none());
        assert!(result.outcome.miss_severity.is_none());
        assert!(!result.outcome.requires_rule_update);
        assert_eq!(
            result.outcome.notes.as_deref(),
            Some("Reviewed and looks good")
        );
        assert_eq!(result.selection.id, selection_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_miss_outcome() {
        let (_db, audit_repo, task_repo, selection_id, _project_id) =
            setup_test_fixture(AuditStratum::AutonomousRelease).await;

        let result = record_audit_outcome(
            &audit_repo,
            &task_repo,
            RecordAuditOutcomeParams {
                selection_id: &selection_id,
                outcome: AuditOutcomeKind::Miss,
                miss_category: Some("missed_security_finding"),
                miss_severity: Some("high"),
                requires_rule_update: true,
                actor: "test-operator",
                notes: Some("Should have been caught by tripwire"),
            },
        )
        .await
        .unwrap();

        assert_eq!(result.outcome.outcome, "miss");
        assert_eq!(
            result.outcome.miss_category.as_deref(),
            Some("missed_security_finding")
        );
        assert_eq!(result.outcome.miss_severity.as_deref(), Some("high"));
        assert!(result.outcome.requires_rule_update);
        assert_eq!(
            result.outcome.notes.as_deref(),
            Some("Should have been caught by tripwire")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_outcome_fails() {
        let (_db, audit_repo, task_repo, selection_id, _project_id) =
            setup_test_fixture(AuditStratum::UnflaggedMerged).await;

        // First outcome succeeds.
        record_audit_outcome(
            &audit_repo,
            &task_repo,
            RecordAuditOutcomeParams {
                selection_id: &selection_id,
                outcome: AuditOutcomeKind::Clean,
                miss_category: None,
                miss_severity: None,
                requires_rule_update: false,
                actor: "test-operator",
                notes: None,
            },
        )
        .await
        .unwrap();

        // Second outcome for same selection fails (unique constraint).
        let err = record_audit_outcome(
            &audit_repo,
            &task_repo,
            RecordAuditOutcomeParams {
                selection_id: &selection_id,
                outcome: AuditOutcomeKind::Miss,
                miss_category: Some("test"),
                miss_severity: Some("low"),
                requires_rule_update: false,
                actor: "test-operator",
                notes: None,
            },
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("failed to record outcome"),
            "expected unique constraint violation, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonexistent_selection_fails() {
        let db = djinn_db::Database::open_in_memory().unwrap();
        let audit_repo = AuditSamplerRepository::new(db.clone());
        let events = djinn_core::events::EventBus::noop();
        let task_repo = TaskRepository::new(db, events);

        let err = record_audit_outcome(
            &audit_repo,
            &task_repo,
            RecordAuditOutcomeParams {
                selection_id: "nonexistent-id",
                outcome: AuditOutcomeKind::Clean,
                miss_category: None,
                miss_severity: None,
                requires_rule_update: false,
                actor: "test-operator",
                notes: None,
            },
        )
        .await
        .unwrap_err();

        assert!(
            err.contains("selection not found"),
            "expected selection not found, got: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn outcome_emits_activity_event() {
        let (_db, audit_repo, task_repo, selection_id, project_id) =
            setup_test_fixture(AuditStratum::UnflaggedMerged).await;

        let result = record_audit_outcome(
            &audit_repo,
            &task_repo,
            RecordAuditOutcomeParams {
                selection_id: &selection_id,
                outcome: AuditOutcomeKind::Miss,
                miss_category: Some("logic_error"),
                miss_severity: Some("medium"),
                requires_rule_update: false,
                actor: "reviewer-1",
                notes: Some("Found missing edge case"),
            },
        )
        .await
        .unwrap();

        // The event emission flag should be true (log_activity succeeds on
        // in-memory DB).
        assert!(result.event_emitted, "event should be emitted");

        // Verify the activity event was actually persisted using TaskRepository.
        let entries = task_repo
            .query_activity(djinn_db::ActivityQuery {
                event_type: Some(EVENT_OUTCOME_RECORDED.to_string()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(entries.len(), 1, "should have exactly one outcome event");
        let entry = &entries[0];
        assert_eq!(entry.actor_id, "reviewer-1");
        assert_eq!(entry.actor_role, "coordinator");

        // Parse and verify payload fields.
        let payload: serde_json::Value = serde_json::from_str(&entry.payload).unwrap();
        assert_eq!(payload["event_type"], EVENT_OUTCOME_RECORDED);
        assert_eq!(payload["outcome_id"], result.outcome.id);
        assert_eq!(payload["selection_id"], selection_id);
        assert_eq!(
            payload["merged_change_id"],
            result.selection.merged_change_id
        );
        assert_eq!(payload["frame_id"], result.selection.frame_id);
        assert_eq!(payload["outcome"], "miss");
        assert_eq!(payload["miss_category"], "logic_error");
        assert_eq!(payload["miss_severity"], "medium");
        assert_eq!(payload["requires_rule_update"], false);
        assert_eq!(payload["actor"], "reviewer-1");
        assert_eq!(payload["project_id"], project_id);
        assert_eq!(payload["notes"], "Found missing edge case");
        assert!(payload["recorded_at"].is_string());
    }
}
