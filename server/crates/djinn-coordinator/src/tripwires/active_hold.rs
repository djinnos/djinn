//! Durable active-hold interpretation layer for tripwire gate activity.
//!
//! This module computes the *active* tripwire hold state for a given
//! `(task_id, pr_number, head_sha)` triple from the typed activity events
//! emitted by the PR-poller gate path (sibling task `vonj`):
//!
//! - [`TRIPWIRE_EVENT_GATE_HELD`] (`tripwire.gate.held`) — marks one or more
//!   enforcement-on findings as held for a specific head SHA.
//! - [`TRIPWIRE_EVENT_HOLD_RELEASED`] (`tripwire.hold.released`) — releases a
//!   previously-held set of findings for a head SHA.
//!
//! ## Sticky head semantics
//!
//! Findings are **active for the same head SHA** until a matching
//! `tripwire.hold.released` event covers the same finding idempotency keys.
//! A **newer** head SHA **supersedes** all older held/released state — when
//! the PR head advances, every prior hold and release becomes irrelevant and
//! fresh evaluation against the new head is required.
//!
//! ## Label tamper reconciliation
//!
//! When an active current-head hold exists but the task no longer carries the
//! [`HUMAN_REVIEW_HOLD_LABEL`] label, the coordinator must reapply the label
//! and emit a [`TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED`] event. This module
//! provides the pure computation that detects this condition and builds the
//! tamper payload; the actual label write + activity-log write are performed
//! by the caller (the PR poller reconciliation tick), which must **fail
//! closed** — activity-log failures or label-update failures must never let
//! the PR proceed as if the hold passed.
//!
//! This module is **pure** — no DB, GitHub, or LLM calls. It deserializes
//! [`ActivityEntry`] payload JSON into the typed payload structs from
//! [`crate::tripwires::activity_payloads`] and computes state. All side
//! effects are the caller's responsibility.

// Contract types are consumed by the enforcement layer incrementally.
#![allow(dead_code)]

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use crate::tripwires::activity_payloads::{
    TRIPWIRE_EVENT_GATE_HELD, TRIPWIRE_EVENT_HOLD_RELEASED, TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED,
    TripwireFindingSummary, TripwireGateDecisionPayload, TripwireHoldReleasedPayload,
    TripwireTamperPayload,
};

// ─── Active hold state ─────────────────────────────────────────────────────

/// Computed active-hold state for a `(task_id, pr_number, head_sha)` triple.
///
/// Produced by [`compute_active_hold_state`]. The state is derived purely
/// from typed activity events for the task; it does not consult the live
/// task row or GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveHoldState {
    /// The head SHA this state was computed for.
    pub head_sha: String,
    /// Whether there is at least one active (unreleased) enforcement hold
    /// for this head SHA.
    pub held: bool,
    /// The enforcement-on findings that are still active (unreleased) for
    /// this head SHA, in deterministic engine-sorted order. Empty when
    /// `held == false`.
    pub active_findings: Vec<TripwireFindingSummary>,
    /// Policy revision from the most recent gate-held event for this head.
    pub policy_revision: Option<String>,
    /// Allowlist revision from the most recent gate-held event for this
    /// head, when boundary findings were present.
    pub allowlist_revision: Option<String>,
    /// Gate-level idempotency key from the most recent gate-held event for
    /// this head.
    pub gate_idempotency_key: Option<String>,
    /// Finding idempotency keys that were released for this head SHA.
    /// Useful for audit / debugging.
    pub released_finding_keys: HashSet<String>,
}

impl ActiveHoldState {
    /// Returns `true` when there are no active holds for this head SHA.
    pub fn is_clear(&self) -> bool {
        !self.held
    }
}

/// Compute the active-hold state for a given head SHA from a set of activity
/// entries.
///
/// `entries` should be the tripwire-related activity rows for the task
/// (typically obtained via `TaskRepository::list_activity` or
/// `TaskRepository::query_activity`). The function filters for
/// `tripwire.gate.held` and `tripwire.hold.released` events, keeps only
/// those whose `head_sha` **matches** `current_head_sha` (newer/older heads
/// are irrelevant and superseded), and computes which enforcement findings
/// remain active.
///
/// # Algorithm
///
/// 1. Partition entries into gate-held and hold-released events.
/// 2. Drop any event whose `head_sha` does not equal `current_head_sha`
///    (head supersession).
/// 3. Collect the set of released finding idempotency keys from release
///    events for this head.
/// 4. From the most recent gate-held event for this head, take the
///    enforcement findings and subtract any that have been released.
/// 5. If any enforcement findings remain, `held == true`.
///
/// # Arguments
///
/// * `entries` — activity entries (any event types; non-tripwire entries
///   are ignored).
/// * `current_head_sha` — the current PR head SHA to evaluate against.
pub fn compute_active_hold_state<'a, I>(entries: I, current_head_sha: &str) -> ActiveHoldState
where
    I: IntoIterator<Item = &'a ActivityEntryRef>,
{
    let mut latest_held: Option<TripwireGateDecisionPayload> = None;
    let mut latest_held_created_at: Option<String> = None;
    let mut released_keys: HashSet<String> = HashSet::new();

    for entry in entries {
        match entry.event_type.as_str() {
            TRIPWIRE_EVENT_GATE_HELD => {
                if let Ok(payload) =
                    serde_json::from_str::<TripwireGateDecisionPayload>(&entry.payload)
                {
                    // Only consider held events for the current head.
                    if payload.head_sha == current_head_sha {
                        // Keep the most recent held event (entries are
                        // assumed chronologically ordered, but we compare
                        // timestamps defensively).
                        let should_replace = match &latest_held_created_at {
                            None => true,
                            Some(prev) => entry.created_at.as_str() > prev.as_str(),
                        };
                        if should_replace {
                            latest_held = Some(payload);
                            latest_held_created_at = Some(entry.created_at.clone());
                        }
                    }
                }
            }
            TRIPWIRE_EVENT_HOLD_RELEASED => {
                if let Ok(payload) =
                    serde_json::from_str::<TripwireHoldReleasedPayload>(&entry.payload)
                {
                    // Only consider releases for the current head.
                    if payload.head_sha == current_head_sha {
                        for finding in &payload.released_findings {
                            released_keys.insert(finding.idempotency_key.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let active_findings: Vec<TripwireFindingSummary> = match &latest_held {
        Some(held) => held
            .findings
            .iter()
            .filter(|f| {
                // Only enforcement-on findings represent holds.
                f.severity
                    == crate::tripwires::activity_payloads::TripwireSeverity::HumanReviewRequired
            })
            .filter(|f| !released_keys.contains(&f.idempotency_key))
            .cloned()
            .collect(),
        None => Vec::new(),
    };

    let held = !active_findings.is_empty();

    let (policy_revision, allowlist_revision, gate_idempotency_key) = match &latest_held {
        Some(held) => (
            Some(held.policy_revision.clone()),
            held.allowlist_revision.clone(),
            Some(held.idempotency_key.clone()),
        ),
        None => (None, None, None),
    };

    ActiveHoldState {
        head_sha: current_head_sha.to_owned(),
        held,
        active_findings,
        policy_revision,
        allowlist_revision,
        gate_idempotency_key,
        released_finding_keys: released_keys,
    }
}

/// Distinct head SHAs that carry at least one `tripwire.gate.held` event in
/// `entries`, in first-seen order.
///
/// The hold-release producer iterates these to release **every** head that
/// may still be held, not just the task row's current `github_head_sha`. The
/// merge-boundary gate and the release emitter must not disagree on "current
/// head": a hold pinned to an earlier CI-snapshot head must still be released
/// when the hold is resolved, even though the task row's head has since
/// advanced (incident: task `7tmi` — gate saw head `fd3d3670`, emitter used
/// row head `e4e0ccb8`, release was a silent no-op).
pub fn gate_held_head_shas<'a, I>(entries: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a ActivityEntryRef>,
{
    let mut seen: Vec<String> = Vec::new();
    let mut set: HashSet<String> = HashSet::new();
    for entry in entries {
        if entry.event_type == TRIPWIRE_EVENT_GATE_HELD
            && let Ok(payload) = serde_json::from_str::<TripwireGateDecisionPayload>(&entry.payload)
            && set.insert(payload.head_sha.clone())
        {
            seen.push(payload.head_sha);
        }
    }
    seen
}

// ─── Label tamper reconciliation ───────────────────────────────────────────

/// Result of a label-tamper reconciliation check.
///
/// Returned by [`check_label_tamper`]. When an active hold exists but the
/// `human-review-hold` label is missing from the task, this carries the
/// tamper payload to log and signals that the label must be reapplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TamperReconciliation {
    /// `true` when tamper was detected (active hold exists, label missing).
    pub tamper_detected: bool,
    /// The tamper payload to persist as a `tripwire.tamper.label_removed`
    /// activity event. `None` when no tamper was detected.
    pub payload: Option<TripwireTamperPayload>,
}

/// Check whether an active hold exists but the task no longer carries the
/// `human-review-hold` label.
///
/// This is the pure detection half of reconciliation. The caller is
/// responsible for:
///
/// 1. Reapplying the label (idempotent `update_labels`).
/// 2. Logging the returned tamper payload via `log_activity`.
/// 3. **Failing closed** — if either the label reapplication or the
///    activity-log write fails, the PR must NOT advance.
///
/// # Arguments
///
/// * `state` — the computed active-hold state for the current head.
/// * `task_id` — task UUID.
/// * `project_id` — project UUID.
/// * `pr_number` — PR number (when known).
/// * `task_has_hold_label` — whether the task currently carries the
///   `human-review-hold` label.
/// * `detection_source` — source of the detection observation (e.g.
///   `"pr_poller"`, `"final_pre_merge_check"`).
/// * `detected_at` — wall-clock timestamp (RFC 3339) for the tamper payload.
pub fn check_label_tamper(
    state: &ActiveHoldState,
    task_id: &str,
    project_id: &str,
    pr_number: Option<u64>,
    task_has_hold_label: bool,
    detection_source: &str,
    detected_at: &str,
) -> TamperReconciliation {
    if !state.held || task_has_hold_label {
        return TamperReconciliation {
            tamper_detected: false,
            payload: None,
        };
    }

    let policy_revision = state.policy_revision.clone().unwrap_or_default();

    let payload = TripwireTamperPayload {
        event_type: TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED.to_owned(),
        task_id: task_id.to_owned(),
        project_id: project_id.to_owned(),
        pr_number,
        head_sha: state.head_sha.clone(),
        policy_revision,
        // Best-effort actor: the removal was observed outside the release
        // path, so the actor is unknown at detection time.
        removed_by: "unknown".to_owned(),
        detection_source: detection_source.to_owned(),
        tampered_findings: state.active_findings.clone(),
        reapplied: true,
        idempotency_key: build_tamper_reconciliation_key(
            task_id,
            pr_number,
            &state.head_sha,
            state
                .gate_idempotency_key
                .as_deref()
                .unwrap_or(&state.head_sha),
        ),
        detected_at: Some(detected_at.to_owned()),
    };

    TamperReconciliation {
        tamper_detected: true,
        payload: Some(payload),
    }
}

/// Build a deterministic idempotency key for a label-tamper reconciliation
/// event.
///
/// The key is a SHA-256 hex digest of:
///
/// ```text
/// tamper:{task_id}:{pr_number}:{head_sha}:{gate_idempotency_key}
/// ```
///
/// This ensures that the same logical tamper (same task, PR, head, and
/// originating gate decision) collapses to one activity row, while a
/// different head SHA or gate decision produces a distinct key.
pub fn build_tamper_reconciliation_key(
    task_id: &str,
    pr_number: Option<u64>,
    head_sha: &str,
    gate_idempotency_key: &str,
) -> String {
    let pr = pr_number.map(|n| n.to_string()).unwrap_or_default();
    let payload = format!(
        "tamper:{}:{}:{}:{}",
        task_id, pr, head_sha, gate_idempotency_key,
    );
    let hash = Sha256::digest(payload.as_bytes());
    format!("sha256:{}", hex::encode(hash))
}

// ─── Activity entry reference ──────────────────────────────────────────────

/// A borrowed view of an activity-log entry sufficient for hold-state
/// computation.
///
/// This is deliberately decoupled from [`djinn_core::models::ActivityEntry`]
/// so the pure computation can be unit-tested with lightweight fixtures and
/// so the caller can pass either owned entries or references. In production,
/// the caller maps `&[ActivityEntry]` to `&[ActivityEntryRef]` via
/// [`ActivityEntryRef::from_entry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntryRef {
    pub event_type: String,
    pub payload: String,
    pub created_at: String,
}

impl ActivityEntryRef {
    /// Convert a borrowed `ActivityEntry` (from `djinn-core`) into the
    /// lightweight reference used by this module.
    pub fn from_entry(entry: &djinn_core::models::ActivityEntry) -> Self {
        Self {
            event_type: entry.event_type.clone(),
            payload: entry.payload.clone(),
            created_at: entry.created_at.clone(),
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tripwires::activity_payloads::{
        TRIPWIRE_EVENT_GATE_HELD, TRIPWIRE_EVENT_GATE_PASSED, TRIPWIRE_EVENT_HOLD_RELEASED,
        TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED, TripwireEvidenceSpan, TripwireFindingSummary,
        TripwireGateDecisionPayload, TripwireHoldReleasedPayload, TripwireSeverity,
    };

    // ── Fixture helpers ───────────────────────────────────────────────────

    const TASK_ID: &str = "task-001";
    const PROJECT_ID: &str = "proj-001";
    const PR_NUMBER: u64 = 42;
    const POLICY_REV: &str = "org-policy:17";

    fn enforcement_finding(rule_id: &str, path: &str, key: &str) -> TripwireFindingSummary {
        TripwireFindingSummary {
            rule_id: rule_id.to_owned(),
            reason_code: format!("tripwire.{rule_id}.changed"),
            severity: TripwireSeverity::HumanReviewRequired,
            evidence: TripwireEvidenceSpan::file(path),
            idempotency_key: key.to_owned(),
            content_fingerprint: format!("fp:{key}"),
            downgrade_reason: None,
        }
    }

    fn report_only_finding(rule_id: &str, path: &str, key: &str) -> TripwireFindingSummary {
        TripwireFindingSummary {
            rule_id: rule_id.to_owned(),
            reason_code: format!("tripwire.{rule_id}.changed"),
            severity: TripwireSeverity::ReportOnly,
            evidence: TripwireEvidenceSpan::file(path),
            idempotency_key: key.to_owned(),
            content_fingerprint: format!("fp:{key}"),
            downgrade_reason: None,
        }
    }

    fn gate_held_entry(
        head_sha: &str,
        findings: Vec<TripwireFindingSummary>,
        created_at: &str,
    ) -> ActivityEntryRef {
        let enforcement_count = findings
            .iter()
            .filter(|f| f.severity == TripwireSeverity::HumanReviewRequired)
            .count() as u32;
        let report_only_count = findings
            .iter()
            .filter(|f| f.severity == TripwireSeverity::ReportOnly)
            .count() as u32;

        let payload = TripwireGateDecisionPayload {
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            task_id: TASK_ID.to_owned(),
            project_id: PROJECT_ID.to_owned(),
            pr_number: Some(PR_NUMBER),
            head_sha: head_sha.to_owned(),
            base_sha: None,
            policy_revision: POLICY_REV.to_owned(),
            allowlist_revision: None,
            findings,
            enforcement_finding_count: enforcement_count,
            report_only_finding_count: report_only_count,
            idempotency_key: format!("sha256:gate:{head_sha}"),
            decided_at: Some(created_at.to_owned()),
        };

        ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            payload: serde_json::to_string(&payload).unwrap_or_default(),
            created_at: created_at.to_owned(),
        }
    }

    fn hold_released_entry(
        head_sha: &str,
        released_findings: Vec<TripwireFindingSummary>,
        created_at: &str,
    ) -> ActivityEntryRef {
        let payload = TripwireHoldReleasedPayload {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            task_id: TASK_ID.to_owned(),
            project_id: PROJECT_ID.to_owned(),
            pr_number: Some(PR_NUMBER),
            head_sha: head_sha.to_owned(),
            policy_revision: POLICY_REV.to_owned(),
            released_by: "user-lead".to_owned(),
            released_by_role: "lead".to_owned(),
            rationale: "approved after review".to_owned(),
            released_findings,
            carried_forward: false,
            idempotency_key: format!("sha256:release:{head_sha}"),
            released_at: Some(created_at.to_owned()),
        };

        ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            payload: serde_json::to_string(&payload).unwrap_or_default(),
            created_at: created_at.to_owned(),
        }
    }

    // ── AC 1: Active hold ─────────────────────────────────────────────────

    /// A gate-held event with enforcement findings for the current head SHA
    /// must produce an active hold state with `held == true` and the
    /// findings listed.
    #[test]
    fn active_hold_with_enforcement_findings() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let f2 = enforcement_finding("unsafe_code_change", "src/native.rs", "key-unsafe");
        let entries = vec![gate_held_entry(
            "sha-aaa",
            vec![f1.clone(), f2.clone()],
            "2026-01-01T00:00:00Z",
        )];

        let state = compute_active_hold_state(&entries, "sha-aaa");

        assert!(state.held, "must be held when enforcement findings exist");
        assert!(!state.is_clear());
        assert_eq!(state.active_findings.len(), 2);
        assert_eq!(state.active_findings[0].idempotency_key, "key-mig");
        assert_eq!(state.active_findings[1].idempotency_key, "key-unsafe");
        assert_eq!(state.policy_revision.as_deref(), Some(POLICY_REV));
        assert_eq!(
            state.gate_idempotency_key.as_deref(),
            Some("sha256:gate:sha-aaa")
        );
        assert!(state.released_finding_keys.is_empty());
    }

    /// Report-only findings must NOT produce an active hold.
    #[test]
    fn report_only_findings_do_not_hold() {
        let f1 = report_only_finding("ci_workflow_change", ".github/workflows/ci.yml", "key-ci");
        let entries = vec![gate_held_entry("sha-bbb", vec![f1], "2026-01-01T00:00:00Z")];

        let state = compute_active_hold_state(&entries, "sha-bbb");

        assert!(!state.held, "report-only findings must not hold");
        assert!(state.is_clear());
        assert!(state.active_findings.is_empty());
    }

    // ── AC 1: Released hold ───────────────────────────────────────────────

    /// A release event covering the same finding keys for the same head SHA
    /// must clear the hold.
    #[test]
    fn released_hold_clears_findings() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let entries = vec![
            gate_held_entry("sha-aaa", vec![f1.clone()], "2026-01-01T00:00:00Z"),
            hold_released_entry("sha-aaa", vec![f1], "2026-01-02T00:00:00Z"),
        ];

        let state = compute_active_hold_state(&entries, "sha-aaa");

        assert!(!state.held, "released hold must not be active");
        assert!(state.is_clear());
        assert!(state.active_findings.is_empty());
        assert!(state.released_finding_keys.contains("key-mig"));
    }

    /// A release covering only some findings leaves the rest active.
    #[test]
    fn partial_release_leaves_remaining_findings_active() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let f2 = enforcement_finding("unsafe_code_change", "src/native.rs", "key-unsafe");
        let entries = vec![
            gate_held_entry(
                "sha-aaa",
                vec![f1.clone(), f2.clone()],
                "2026-01-01T00:00:00Z",
            ),
            hold_released_entry("sha-aaa", vec![f1], "2026-01-02T00:00:00Z"),
        ];

        let state = compute_active_hold_state(&entries, "sha-aaa");

        assert!(state.held, "unreleased finding must keep the hold active");
        assert_eq!(state.active_findings.len(), 1);
        assert_eq!(state.active_findings[0].idempotency_key, "key-unsafe");
    }

    // ── AC 2: Stale release after new head SHA ────────────────────────────

    /// A release recorded for an older head SHA must be treated as stale
    /// after a push — the new head must require fresh evaluation.
    #[test]
    fn stale_release_after_new_head_sha() {
        let old_finding =
            enforcement_finding("migration_change", "migrations/001.sql", "key-mig-old");
        let new_finding =
            enforcement_finding("migration_change", "migrations/001.sql", "key-mig-new");

        // Old head was held and then released.
        // New head is held (fresh evaluation).
        let entries = vec![
            gate_held_entry("sha-old", vec![old_finding], "2026-01-01T00:00:00Z"),
            hold_released_entry("sha-old", vec![], "2026-01-02T00:00:00Z"),
            gate_held_entry("sha-new", vec![new_finding.clone()], "2026-01-03T00:00:00Z"),
        ];

        let state = compute_active_hold_state(&entries, "sha-new");

        assert!(
            state.held,
            "new head hold must be active despite old head release"
        );
        assert_eq!(state.active_findings.len(), 1);
        assert_eq!(state.active_findings[0].idempotency_key, "key-mig-new");
        // The old release must not leak into the new head's released set.
        assert!(
            state.released_finding_keys.is_empty(),
            "old-head release must not affect new-head state"
        );
    }

    // ── AC 2: Superseded old head ─────────────────────────────────────────

    /// When the current head SHA has no gate-held event, the state is clear
    /// even if an older head was held — the old hold is superseded.
    #[test]
    fn superseded_old_head_is_ignored() {
        let old_finding =
            enforcement_finding("migration_change", "migrations/001.sql", "key-mig-old");

        let entries = vec![gate_held_entry(
            "sha-old",
            vec![old_finding],
            "2026-01-01T00:00:00Z",
        )];

        // Query for a newer head that has no held event yet.
        let state = compute_active_hold_state(&entries, "sha-new");

        assert!(!state.held, "old-head hold must not apply to a newer head");
        assert!(state.is_clear());
        assert!(state.active_findings.is_empty());
    }

    /// A newer head with a gate-passed event is clear (no enforcement
    /// findings), superseding the old held head entirely.
    #[test]
    fn newer_head_passed_supersedes_old_held_head() {
        let old_finding =
            enforcement_finding("migration_change", "migrations/001.sql", "key-mig-old");

        // gate-passed payload for the new head.
        let passed_payload = TripwireGateDecisionPayload {
            event_type: TRIPWIRE_EVENT_GATE_PASSED.to_owned(),
            task_id: TASK_ID.to_owned(),
            project_id: PROJECT_ID.to_owned(),
            pr_number: Some(PR_NUMBER),
            head_sha: "sha-new".to_owned(),
            base_sha: None,
            policy_revision: POLICY_REV.to_owned(),
            allowlist_revision: None,
            findings: Vec::new(),
            enforcement_finding_count: 0,
            report_only_finding_count: 0,
            idempotency_key: "sha256:passed:sha-new".to_owned(),
            decided_at: Some("2026-01-03T00:00:00Z".to_owned()),
        };
        let passed_entry = ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_GATE_PASSED.to_owned(),
            payload: serde_json::to_string(&passed_payload).unwrap_or_default(),
            created_at: "2026-01-03T00:00:00Z".to_owned(),
        };

        let entries = vec![
            gate_held_entry("sha-old", vec![old_finding], "2026-01-01T00:00:00Z"),
            passed_entry,
        ];

        let state = compute_active_hold_state(&entries, "sha-new");

        assert!(!state.held, "new head with passed gate is clear");
        assert!(state.active_findings.is_empty());
    }

    // ── AC 3: Direct label-removal tamper reapplication ───────────────────

    /// When an active hold exists and the task does NOT carry the
    /// human-review-hold label, the reconciliation must detect tamper and
    /// build a `tripwire.tamper.label_removed` payload.
    #[test]
    fn direct_label_removal_triggers_tamper_reconciliation() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let entries = vec![gate_held_entry(
            "sha-aaa",
            vec![f1.clone()],
            "2026-01-01T00:00:00Z",
        )];

        let state = compute_active_hold_state(&entries, "sha-aaa");
        assert!(state.held);

        // Task does NOT have the hold label → tamper.
        let recon = check_label_tamper(
            &state,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            false, // label missing
            "pr_poller",
            "2026-01-02T00:00:00Z",
        );

        assert!(recon.tamper_detected);
        let payload = recon.payload.expect("tamper payload must be present");
        assert_eq!(payload.event_type, TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED);
        assert_eq!(payload.task_id, TASK_ID);
        assert_eq!(payload.project_id, PROJECT_ID);
        assert_eq!(payload.pr_number, Some(PR_NUMBER));
        assert_eq!(payload.head_sha, "sha-aaa");
        assert_eq!(payload.policy_revision, POLICY_REV);
        assert_eq!(payload.detection_source, "pr_poller");
        assert!(
            payload.reapplied,
            "reconciliation must mark label as reapplied"
        );
        assert_eq!(payload.tampered_findings.len(), 1);
        assert_eq!(payload.tampered_findings[0].idempotency_key, "key-mig");
        assert!(
            payload.idempotency_key.starts_with("sha256:"),
            "idempotency key must be sha256-prefixed"
        );
        payload.validate().expect("tamper payload must validate");
    }

    /// When an active hold exists and the task DOES carry the label, no
    /// tamper is detected.
    #[test]
    fn label_present_no_tamper() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let entries = vec![gate_held_entry("sha-aaa", vec![f1], "2026-01-01T00:00:00Z")];

        let state = compute_active_hold_state(&entries, "sha-aaa");
        assert!(state.held);

        let recon = check_label_tamper(
            &state,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            true, // label present
            "pr_poller",
            "2026-01-02T00:00:00Z",
        );

        assert!(!recon.tamper_detected);
        assert!(recon.payload.is_none());
    }

    /// When there is no active hold (hold was released), no tamper is
    /// detected even if the label is missing.
    #[test]
    fn no_tamper_when_hold_released() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let entries = vec![
            gate_held_entry("sha-aaa", vec![f1.clone()], "2026-01-01T00:00:00Z"),
            hold_released_entry("sha-aaa", vec![f1], "2026-01-02T00:00:00Z"),
        ];

        let state = compute_active_hold_state(&entries, "sha-aaa");
        assert!(!state.held);

        let recon = check_label_tamper(
            &state,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            false, // label missing, but no active hold
            "pr_poller",
            "2026-01-03T00:00:00Z",
        );

        assert!(!recon.tamper_detected);
        assert!(recon.payload.is_none());
    }

    // ── Idempotency key determinism ───────────────────────────────────────

    /// The tamper reconciliation idempotency key must be deterministic for
    /// the same inputs.
    #[test]
    fn tamper_key_is_deterministic() {
        let k1 = build_tamper_reconciliation_key(
            TASK_ID,
            Some(PR_NUMBER),
            "sha-aaa",
            "sha256:gate:sha-aaa",
        );
        let k2 = build_tamper_reconciliation_key(
            TASK_ID,
            Some(PR_NUMBER),
            "sha-aaa",
            "sha256:gate:sha-aaa",
        );
        assert_eq!(k1, k2);
    }

    /// The tamper key must change with the head SHA.
    #[test]
    fn tamper_key_changes_with_head_sha() {
        let k1 =
            build_tamper_reconciliation_key(TASK_ID, Some(PR_NUMBER), "sha-a", "sha256:gate:a");
        let k2 =
            build_tamper_reconciliation_key(TASK_ID, Some(PR_NUMBER), "sha-b", "sha256:gate:b");
        assert_ne!(k1, k2);
    }

    // ── Non-tripwire entries are ignored ──────────────────────────────────

    /// Activity entries with unrelated event types must be ignored.
    #[test]
    fn non_tripwire_entries_are_ignored() {
        let comment_entry = ActivityEntryRef {
            event_type: "comment".to_owned(),
            payload: r#"{"body":"hello"}"#.to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        };
        let entries = vec![comment_entry];

        let state = compute_active_hold_state(&entries, "sha-aaa");

        assert!(!state.held);
        assert!(state.active_findings.is_empty());
    }

    /// Malformed payload JSON must be gracefully skipped (fail-closed: no
    /// spurious hold, no panic).
    #[test]
    fn malformed_payload_is_skipped() {
        let malformed = ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            payload: "{not valid json".to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        };
        let entries = vec![malformed];

        let state = compute_active_hold_state(&entries, "sha-aaa");

        // Malformed entry is skipped → no hold derived from it.
        assert!(!state.held);
        assert!(state.active_findings.is_empty());
    }

    // ── Multiple held events: most recent wins ────────────────────────────

    /// When multiple gate-held events exist for the same head, the most
    /// recent one is used.
    #[test]
    fn most_recent_held_event_wins() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig-1");
        let f2 = enforcement_finding("unsafe_code_change", "src/native.rs", "key-unsafe-2");

        let entries = vec![
            gate_held_entry("sha-aaa", vec![f1], "2026-01-01T00:00:00Z"),
            gate_held_entry("sha-aaa", vec![f2.clone()], "2026-01-02T00:00:00Z"),
        ];

        let state = compute_active_hold_state(&entries, "sha-aaa");

        assert!(state.held);
        // The most recent held event's findings are active.
        assert_eq!(state.active_findings.len(), 1);
        assert_eq!(state.active_findings[0].idempotency_key, "key-unsafe-2");
    }

    // ── ActivityEntryRef::from_entry round-trip ───────────────────────────

    /// `from_entry` must faithfully map `ActivityEntry` fields.
    #[test]
    fn from_entry_maps_fields() {
        let entry = djinn_core::models::ActivityEntry {
            id: "act-1".to_owned(),
            task_id: Some(TASK_ID.to_owned()),
            actor_id: "coordinator".to_owned(),
            actor_role: "system".to_owned(),
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            payload: r#"{"event_type":"tripwire.gate.held"}"#.to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        };

        let refs = ActivityEntryRef::from_entry(&entry);
        assert_eq!(refs.event_type, TRIPWIRE_EVENT_GATE_HELD);
        assert_eq!(refs.created_at, "2026-01-01T00:00:00Z");
        assert!(refs.payload.contains("tripwire.gate.held"));
    }

    // ── gate_held_head_shas ───────────────────────────────────────────────

    /// Distinct held heads are returned in first-seen order (the release
    /// producer iterates these to release every held head, not just the row's
    /// current head — incident 7tmi).
    #[test]
    fn gate_held_head_shas_returns_distinct_heads() {
        let f = enforcement_finding("migration_change", "migrations/001.sql", "k");
        let entries = vec![
            gate_held_entry("sha-a", vec![f.clone()], "2026-01-01T00:00:00Z"),
            gate_held_entry("sha-b", vec![f.clone()], "2026-01-02T00:00:00Z"),
            // Duplicate head must not repeat.
            gate_held_entry("sha-a", vec![f.clone()], "2026-01-03T00:00:00Z"),
            // Releases are not gate.held events and must be ignored here.
            hold_released_entry("sha-a", vec![f], "2026-01-04T00:00:00Z"),
        ];
        let heads = gate_held_head_shas(&entries);
        assert_eq!(heads, vec!["sha-a".to_owned(), "sha-b".to_owned()]);
    }

    /// No gate.held events → empty head list.
    #[test]
    fn gate_held_head_shas_empty_without_held() {
        let entries: Vec<ActivityEntryRef> = Vec::new();
        assert!(gate_held_head_shas(&entries).is_empty());
    }

    // ── Empty entries ─────────────────────────────────────────────────────

    /// With no activity entries, the state is clear.
    #[test]
    fn empty_entries_produce_clear_state() {
        let entries: Vec<ActivityEntryRef> = Vec::new();
        let state = compute_active_hold_state(&entries, "sha-aaa");

        assert!(!state.held);
        assert!(state.is_clear());
        assert!(state.active_findings.is_empty());
    }
}
