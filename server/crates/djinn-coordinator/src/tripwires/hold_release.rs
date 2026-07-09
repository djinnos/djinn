//! Producer helpers for `tripwire.hold.released` activity events.
//!
//! The active-hold interpreter ([`crate::tripwires::active_hold`]) clears a
//! hold only when it observes a [`TRIPWIRE_EVENT_HOLD_RELEASED`] event whose
//! `head_sha` matches the current head and whose released finding
//! idempotency keys cover the held findings. Until this module landed there
//! was **no producer** of that event anywhere in the coordinator — a human
//! closing a `human-review-hold` remediation task unparked the source (via
//! `mark_human_review_resolved`) but never wrote the release event, so the
//! merge-boundary tripwire gate could never clear for the current head short
//! of pushing a new commit (incident: hold task `yynd`).
//!
//! This module is **pure** — no DB, GitHub, or LLM calls. It builds and
//! validates a [`TripwireHoldReleasedPayload`] from a computed
//! [`ActiveHoldState`]; the actual activity-log write is the caller's
//! responsibility (see `CoordinatorActor::emit_tripwire_release_on_hold_close`).

use sha2::{Digest, Sha256};

use crate::tripwires::active_hold::ActiveHoldState;
use crate::tripwires::activity_payloads::{
    TRIPWIRE_EVENT_HOLD_RELEASED, TripwireHoldReleasedPayload,
};

/// Fallback rationale stamped on a release when the closing hold task carries
/// no close reason. The payload validator rejects an empty rationale, so a
/// non-empty fallback is required for the fail-closed close path.
pub const DEFAULT_HOLD_RELEASE_RATIONALE: &str = "human review hold closed";

/// Actor id stamped on a release emitted by the human hold-task-close path.
/// The close is driven by a human resolving the `human-review-hold`
/// remediation task; the specific GitHub login is not resolvable at the
/// coordinator event boundary, so a stable `"human"` sentinel is used.
pub const HUMAN_RELEASE_ACTOR: &str = "human";

/// Role stamped on a release emitted by the human hold-task-close path.
pub const HUMAN_RELEASE_ROLE: &str = "human_reviewer";

/// Build a deterministic idempotency key for a hold-release event.
///
/// The key is a SHA-256 hex digest of:
///
/// ```text
/// release:{task_id}:{pr_number}:{head_sha}:{released_by}:{policy_revision}
/// ```
///
/// This mirrors the release-key derivation documented on
/// [`TripwireHoldReleasedPayload`] (`(task_id, pr_number, head_sha,
/// released_by, policy_revision)`) so the same logical release recorded twice
/// collapses to one stable key, while a different head SHA produces a
/// distinct key (stale-release protection).
pub fn build_hold_release_key(
    task_id: &str,
    pr_number: Option<u64>,
    head_sha: &str,
    released_by: &str,
    policy_revision: &str,
) -> String {
    let pr = pr_number.map(|n| n.to_string()).unwrap_or_default();
    let payload = format!("release:{task_id}:{pr}:{head_sha}:{released_by}:{policy_revision}");
    let hash = Sha256::digest(payload.as_bytes());
    format!("sha256:{}", hex::encode(hash))
}

/// Build a validated [`TripwireHoldReleasedPayload`] that releases **every**
/// finding currently active in `state` for the state's head SHA.
///
/// Returns:
///
/// - `Ok(None)` when the state carries no active hold — nothing to release,
///   the caller should skip emission (no spurious event).
/// - `Ok(Some(payload))` with a validated payload covering all active
///   findings when a hold exists.
/// - `Err(msg)` when the constructed payload fails validation (empty
///   actor / role after the rationale fallback is applied).
///
/// The `head_sha` of the release is taken from `state.head_sha`, so a release
/// built from a hold computed against head A can never clear a hold on head B
/// (the active-hold interpreter keys releases on `head_sha`).
#[allow(clippy::too_many_arguments)]
pub fn build_hold_released_payload(
    state: &ActiveHoldState,
    task_id: &str,
    project_id: &str,
    pr_number: Option<u64>,
    released_by: &str,
    released_by_role: &str,
    rationale: &str,
    released_at: &str,
) -> Result<Option<TripwireHoldReleasedPayload>, String> {
    if !state.held {
        return Ok(None);
    }

    // The payload validator rejects an empty rationale; fall back to a fixed
    // string when the closing hold task carried no close reason.
    let rationale = if rationale.trim().is_empty() {
        DEFAULT_HOLD_RELEASE_RATIONALE
    } else {
        rationale
    };

    let policy_revision = state.policy_revision.clone().unwrap_or_default();
    let idempotency_key = build_hold_release_key(
        task_id,
        pr_number,
        &state.head_sha,
        released_by,
        &policy_revision,
    );

    let payload = TripwireHoldReleasedPayload {
        event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
        task_id: task_id.to_owned(),
        project_id: project_id.to_owned(),
        pr_number,
        head_sha: state.head_sha.clone(),
        policy_revision,
        released_by: released_by.to_owned(),
        released_by_role: released_by_role.to_owned(),
        rationale: rationale.to_owned(),
        released_findings: state.active_findings.clone(),
        idempotency_key,
        released_at: Some(released_at.to_owned()),
    };

    payload.validate()?;
    Ok(Some(payload))
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tripwires::active_hold::{ActivityEntryRef, compute_active_hold_state};
    use crate::tripwires::activity_payloads::{
        TRIPWIRE_EVENT_GATE_HELD, TripwireEvidenceSpan, TripwireFindingSummary,
        TripwireGateDecisionPayload, TripwireSeverity,
    };

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
            report_only_finding_count: 0,
            idempotency_key: format!("sha256:gate:{head_sha}"),
            decided_at: Some(created_at.to_owned()),
        };
        ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            payload: serde_json::to_string(&payload).unwrap_or_default(),
            created_at: created_at.to_owned(),
        }
    }

    /// A release built from an active single-finding hold must validate and
    /// carry the held finding, and — fed back into the interpreter as an
    /// activity entry — must clear the hold for the same head (end-to-end
    /// producer→interpreter round-trip).
    #[test]
    fn release_from_active_hold_clears_same_head() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let held = vec![gate_held_entry("sha-aaa", vec![f1], "2026-01-01T00:00:00Z")];
        let state = compute_active_hold_state(&held, "sha-aaa");
        assert!(state.held);

        let payload = build_hold_released_payload(
            &state,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            HUMAN_RELEASE_ACTOR,
            HUMAN_RELEASE_ROLE,
            "resolved after manual review",
            "2026-01-02T00:00:00Z",
        )
        .expect("payload builds")
        .expect("hold is active so a payload is produced");

        payload.validate().expect("payload validates");
        assert_eq!(payload.head_sha, "sha-aaa");
        assert_eq!(payload.released_findings.len(), 1);
        assert_eq!(payload.released_findings[0].idempotency_key, "key-mig");
        assert_eq!(payload.released_by, HUMAN_RELEASE_ACTOR);

        // Feed the release back into the interpreter alongside the held event.
        let release_entry = ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            payload: serde_json::to_string(&payload).unwrap_or_default(),
            created_at: "2026-01-02T00:00:00Z".to_owned(),
        };
        let mut entries = held.clone();
        entries.push(release_entry);
        let cleared = compute_active_hold_state(&entries, "sha-aaa");
        assert!(
            !cleared.held,
            "release must clear the hold for the same head"
        );
        assert!(cleared.released_finding_keys.contains("key-mig"));
    }

    /// A multi-finding hold must be fully covered by the human-close release
    /// (partial coverage regression — all held findings released at once).
    #[test]
    fn release_covers_all_multi_findings() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let f2 = enforcement_finding("unsafe_code_change", "src/native.rs", "key-unsafe");
        let held = vec![gate_held_entry(
            "sha-aaa",
            vec![f1, f2],
            "2026-01-01T00:00:00Z",
        )];
        let state = compute_active_hold_state(&held, "sha-aaa");
        assert_eq!(state.active_findings.len(), 2);

        let payload = build_hold_released_payload(
            &state,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            HUMAN_RELEASE_ACTOR,
            HUMAN_RELEASE_ROLE,
            "reviewed",
            "2026-01-02T00:00:00Z",
        )
        .expect("payload builds")
        .expect("hold active");
        assert_eq!(payload.released_findings.len(), 2);

        let release_entry = ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            payload: serde_json::to_string(&payload).unwrap_or_default(),
            created_at: "2026-01-02T00:00:00Z".to_owned(),
        };
        let mut entries = held.clone();
        entries.push(release_entry);
        let cleared = compute_active_hold_state(&entries, "sha-aaa");
        assert!(!cleared.held, "multi-finding hold fully cleared");
        assert_eq!(cleared.active_findings.len(), 0);
    }

    /// A release built for head A must NOT clear a hold on head B
    /// (stale-release protection preserved through the producer).
    #[test]
    fn release_for_old_head_does_not_clear_new_head() {
        let old_f = enforcement_finding("migration_change", "migrations/001.sql", "key-old");
        let held_old = vec![gate_held_entry(
            "sha-old",
            vec![old_f],
            "2026-01-01T00:00:00Z",
        )];
        let state_old = compute_active_hold_state(&held_old, "sha-old");

        let payload = build_hold_released_payload(
            &state_old,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            HUMAN_RELEASE_ACTOR,
            HUMAN_RELEASE_ROLE,
            "reviewed",
            "2026-01-02T00:00:00Z",
        )
        .expect("payload builds")
        .expect("hold active");
        assert_eq!(payload.head_sha, "sha-old");

        // A new head is held (fresh push). The old-head release must not leak.
        let new_f = enforcement_finding("migration_change", "migrations/001.sql", "key-new");
        let release_entry = ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            payload: serde_json::to_string(&payload).unwrap_or_default(),
            created_at: "2026-01-02T00:00:00Z".to_owned(),
        };
        let entries = vec![
            gate_held_entry("sha-new", vec![new_f], "2026-01-03T00:00:00Z"),
            release_entry,
        ];
        let state_new = compute_active_hold_state(&entries, "sha-new");
        assert!(
            state_new.held,
            "old-head release must not clear the new-head hold"
        );
        assert!(state_new.released_finding_keys.is_empty());
    }

    /// A clear state (no active hold) must produce no payload — the close
    /// path skips emission (no spurious event).
    #[test]
    fn no_payload_when_not_held() {
        let state = compute_active_hold_state::<&[ActivityEntryRef]>(&[], "sha-none");
        assert!(!state.held);
        let out = build_hold_released_payload(
            &state,
            TASK_ID,
            PROJECT_ID,
            None,
            HUMAN_RELEASE_ACTOR,
            HUMAN_RELEASE_ROLE,
            "irrelevant",
            "2026-01-02T00:00:00Z",
        )
        .expect("build ok");
        assert!(out.is_none(), "no hold → no release payload");
    }

    /// An empty close reason must fall back to the fixed rationale so the
    /// payload validator (which rejects empty rationale) accepts it.
    #[test]
    fn empty_rationale_falls_back() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let held = vec![gate_held_entry("sha-aaa", vec![f1], "2026-01-01T00:00:00Z")];
        let state = compute_active_hold_state(&held, "sha-aaa");

        let payload = build_hold_released_payload(
            &state,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            HUMAN_RELEASE_ACTOR,
            HUMAN_RELEASE_ROLE,
            "   ", // whitespace-only close reason
            "2026-01-02T00:00:00Z",
        )
        .expect("build ok")
        .expect("hold active");
        assert_eq!(payload.rationale, DEFAULT_HOLD_RELEASE_RATIONALE);
        payload.validate().expect("fallback rationale validates");
    }

    /// The release idempotency key is stable for identical inputs and
    /// changes with the head SHA (double-close does not corrupt state; a new
    /// head produces a distinct key).
    #[test]
    fn release_key_stable_and_head_sensitive() {
        let k1 = build_hold_release_key(
            TASK_ID,
            Some(PR_NUMBER),
            "sha-a",
            HUMAN_RELEASE_ACTOR,
            POLICY_REV,
        );
        let k2 = build_hold_release_key(
            TASK_ID,
            Some(PR_NUMBER),
            "sha-a",
            HUMAN_RELEASE_ACTOR,
            POLICY_REV,
        );
        assert_eq!(k1, k2, "same inputs → same key");
        let k3 = build_hold_release_key(
            TASK_ID,
            Some(PR_NUMBER),
            "sha-b",
            HUMAN_RELEASE_ACTOR,
            POLICY_REV,
        );
        assert_ne!(k1, k3, "different head → different key");
        assert!(k1.starts_with("sha256:"));
    }

    /// Idempotency: building the release payload twice from the same state
    /// yields byte-identical payloads (stable key + deterministic fields), so
    /// a double/re-close cannot produce a diverging release row.
    #[test]
    fn double_build_is_idempotent() {
        let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
        let held = vec![gate_held_entry("sha-aaa", vec![f1], "2026-01-01T00:00:00Z")];
        let state = compute_active_hold_state(&held, "sha-aaa");

        let p1 = build_hold_released_payload(
            &state,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            HUMAN_RELEASE_ACTOR,
            HUMAN_RELEASE_ROLE,
            "reviewed",
            "2026-01-02T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        let p2 = build_hold_released_payload(
            &state,
            TASK_ID,
            PROJECT_ID,
            Some(PR_NUMBER),
            HUMAN_RELEASE_ACTOR,
            HUMAN_RELEASE_ROLE,
            "reviewed",
            "2026-01-02T00:00:00Z",
        )
        .unwrap()
        .unwrap();
        assert_eq!(p1.idempotency_key, p2.idempotency_key);
        assert_eq!(
            serde_json::to_string(&p1).unwrap(),
            serde_json::to_string(&p2).unwrap()
        );
    }
}
