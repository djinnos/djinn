//! Release carry-forward for unchanged tripwire findings across PR heads.
//!
//! When a PR head advances, the sticky-hold state machine
//! ([`crate::tripwires::active_hold`]) deliberately supersedes every prior
//! hold and release — a new head requires fresh adjudication. That is correct
//! when the flagged *content* changed, but it re-holds benign changes that
//! were already adjudicated and released on an earlier head yet are
//! byte-identical on the new head (a rebase, a merge of `main`, or an
//! unrelated commit that leaves the flagged file untouched).
//!
//! This module closes that gap **without** weakening the per-head stale-release
//! protection. Every finding carries a head-independent
//! [content fingerprint](crate::tripwires::engine::build_finding_content_fingerprint)
//! derived from `(rule_id, file, patch hunk)`. When a NEW head produces an
//! enforcement finding whose fingerprint matches a finding that was already
//! *released* on a PRIOR head of the same PR, [`build_carry_forward_releases`]
//! emits a fresh `tripwire.hold.released` payload **for the new head**,
//! referencing the prior rationale/releaser and marked `carried_forward:
//! true`. A genuinely changed finding has a different fingerprint, so it is
//! never carried forward — it re-holds and is adjudicated afresh.
//!
//! This module is **pure** — no DB, GitHub, or LLM calls. The caller
//! (`CoordinatorActor`, the gate path) logs the returned payloads as activity
//! events and re-computes the active-hold state.

// Consumed by the gate path (`pr_watcher`) once wired; keep the module warm
// for incremental landing without churn.
#![allow(dead_code)]

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use crate::tripwires::active_hold::ActivityEntryRef;
use crate::tripwires::activity_payloads::{
    TRIPWIRE_EVENT_HOLD_RELEASED, TripwireFindingSummary, TripwireHoldReleasedPayload,
};

/// Prior-head release context recovered for a single content fingerprint.
#[derive(Debug, Clone)]
struct PriorRelease {
    head_sha: String,
    released_by: String,
    released_by_role: String,
    rationale: String,
    policy_revision: String,
}

/// A carry-forward release built for the new head plus the prior head it was
/// carried from (for logging / audit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarryForwardRelease {
    /// The prior head SHA the release rationale/releaser was carried from.
    pub from_head_sha: String,
    /// The validated release payload for the new head.
    pub payload: TripwireHoldReleasedPayload,
}

/// Build a deterministic idempotency key for a carry-forward release.
fn build_carry_forward_key(
    task_id: &str,
    pr_number: Option<u64>,
    new_head_sha: &str,
    policy_revision: &str,
) -> String {
    let pr = pr_number.map(|n| n.to_string()).unwrap_or_default();
    let payload = format!("carry_forward:{task_id}:{pr}:{new_head_sha}:{policy_revision}");
    let hash = Sha256::digest(payload.as_bytes());
    format!("sha256:{}", hex::encode(hash))
}

/// Compute carry-forward releases for a new head's enforcement findings.
///
/// # Arguments
/// * `entries` — the source task's tripwire activity entries (gate.held +
///   hold.released across all heads).
/// * `new_head_sha` — the head just evaluated by the gate.
/// * `new_enforcement_findings` — the enforcement-on findings the gate
///   produced for `new_head_sha` (each carrying a `content_fingerprint`).
/// * `task_id`, `project_id`, `pr_number` — identity for the release payload.
/// * `released_at` — RFC 3339 timestamp for the emitted release.
///
/// Returns at most one [`CarryForwardRelease`] (all matched findings collapse
/// into a single release for the new head). Returns an empty vector when no
/// enforcement finding's fingerprint matches a prior-head release, or when
/// `new_head_sha` itself already carries a release for those fingerprints.
///
/// The returned payload is validated; a build that would fail validation is
/// dropped rather than returned.
pub fn build_carry_forward_releases(
    entries: &[ActivityEntryRef],
    new_head_sha: &str,
    new_enforcement_findings: &[TripwireFindingSummary],
    task_id: &str,
    project_id: &str,
    pr_number: Option<u64>,
    released_at: &str,
) -> Vec<CarryForwardRelease> {
    if new_enforcement_findings.is_empty() {
        return Vec::new();
    }

    // Map content fingerprint → prior release context, from releases on any
    // head OTHER than the new head. Ignore blank fingerprints (pre-fingerprint
    // rows) so we never match on the empty string.
    let mut prior_by_fp: HashMap<String, PriorRelease> = HashMap::new();
    // Fingerprints already released ON the new head — never re-emit those.
    let mut released_on_new_head: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for entry in entries {
        if entry.event_type != TRIPWIRE_EVENT_HOLD_RELEASED {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<TripwireHoldReleasedPayload>(&entry.payload)
        else {
            continue;
        };
        for finding in &payload.released_findings {
            let fp = finding.content_fingerprint.trim();
            if fp.is_empty() {
                continue;
            }
            if payload.head_sha == new_head_sha {
                released_on_new_head.insert(fp.to_owned());
                continue;
            }
            // Keep the most-recent prior release per fingerprint. Entries are
            // chronological, so a later insert wins.
            prior_by_fp.insert(
                fp.to_owned(),
                PriorRelease {
                    head_sha: payload.head_sha.clone(),
                    released_by: payload.released_by.clone(),
                    released_by_role: payload.released_by_role.clone(),
                    rationale: payload.rationale.clone(),
                    policy_revision: payload.policy_revision.clone(),
                },
            );
        }
    }

    if prior_by_fp.is_empty() {
        return Vec::new();
    }

    // Collect new-head enforcement findings whose fingerprint was released on a
    // prior head and NOT already re-released on the new head.
    let mut matched: Vec<TripwireFindingSummary> = Vec::new();
    let mut prior: Option<PriorRelease> = None;
    for finding in new_enforcement_findings {
        let fp = finding.content_fingerprint.trim();
        if fp.is_empty() || released_on_new_head.contains(fp) {
            continue;
        }
        if let Some(p) = prior_by_fp.get(fp) {
            matched.push(finding.clone());
            // Reference the first matched prior release for rationale/releaser.
            if prior.is_none() {
                prior = Some(p.clone());
            }
        }
    }

    let (Some(prior), false) = (prior, matched.is_empty()) else {
        return Vec::new();
    };

    let policy_revision = if prior.policy_revision.trim().is_empty() {
        // Fall back to the new findings' policy revision is not tracked here;
        // an empty revision still produces a valid, distinct release key.
        String::new()
    } else {
        prior.policy_revision.clone()
    };

    let rationale = format!(
        "carried forward from prior head {}: {}",
        &prior.head_sha[..12.min(prior.head_sha.len())],
        prior.rationale,
    );

    let payload = TripwireHoldReleasedPayload {
        event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
        task_id: task_id.to_owned(),
        project_id: project_id.to_owned(),
        pr_number,
        head_sha: new_head_sha.to_owned(),
        policy_revision: policy_revision.clone(),
        released_by: prior.released_by.clone(),
        released_by_role: prior.released_by_role.clone(),
        rationale,
        released_findings: matched,
        carried_forward: true,
        idempotency_key: build_carry_forward_key(
            task_id,
            pr_number,
            new_head_sha,
            &policy_revision,
        ),
        released_at: Some(released_at.to_owned()),
    };

    if payload.validate().is_err() {
        return Vec::new();
    }

    vec![CarryForwardRelease {
        from_head_sha: prior.head_sha,
        payload,
    }]
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tripwires::activity_payloads::{
        TRIPWIRE_EVENT_GATE_HELD, TripwireEvidenceSpan, TripwireGateDecisionPayload,
        TripwireSeverity,
    };

    const TASK_ID: &str = "task-cf";
    const PROJECT_ID: &str = "proj-cf";
    const PR: u64 = 7;
    const POLICY_REV: &str = "org-policy:1";

    fn finding(rule_id: &str, path: &str, fp: &str) -> TripwireFindingSummary {
        TripwireFindingSummary {
            rule_id: rule_id.to_owned(),
            reason_code: format!("tripwire.{rule_id}.changed"),
            severity: TripwireSeverity::HumanReviewRequired,
            evidence: TripwireEvidenceSpan::file(path),
            idempotency_key: format!("sha256:{path}"),
            content_fingerprint: fp.to_owned(),
            downgrade_reason: None,
        }
    }

    fn gate_held_entry(
        head: &str,
        findings: Vec<TripwireFindingSummary>,
        at: &str,
    ) -> ActivityEntryRef {
        let payload = TripwireGateDecisionPayload {
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            task_id: TASK_ID.to_owned(),
            project_id: PROJECT_ID.to_owned(),
            pr_number: Some(PR),
            head_sha: head.to_owned(),
            base_sha: None,
            policy_revision: POLICY_REV.to_owned(),
            allowlist_revision: None,
            enforcement_finding_count: findings.len() as u32,
            report_only_finding_count: 0,
            findings,
            idempotency_key: format!("sha256:gate:{head}"),
            decided_at: Some(at.to_owned()),
        };
        ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            payload: serde_json::to_string(&payload).unwrap(),
            created_at: at.to_owned(),
        }
    }

    fn release_entry(
        head: &str,
        findings: Vec<TripwireFindingSummary>,
        rationale: &str,
        at: &str,
    ) -> ActivityEntryRef {
        let payload = TripwireHoldReleasedPayload {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            task_id: TASK_ID.to_owned(),
            project_id: PROJECT_ID.to_owned(),
            pr_number: Some(PR),
            head_sha: head.to_owned(),
            policy_revision: POLICY_REV.to_owned(),
            released_by: "lead".to_owned(),
            released_by_role: "lead".to_owned(),
            rationale: rationale.to_owned(),
            released_findings: findings,
            carried_forward: false,
            idempotency_key: format!("sha256:release:{head}"),
            released_at: Some(at.to_owned()),
        };
        ActivityEntryRef {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            payload: serde_json::to_string(&payload).unwrap(),
            created_at: at.to_owned(),
        }
    }

    /// Same content fingerprint released on a prior head → carry-forward
    /// release emitted for the new head, marked `carried_forward`.
    #[test]
    fn unchanged_finding_is_carried_forward() {
        let f_old = finding("unsafe_code_change", "tests/fixture.rs", "fp:same");
        let entries = vec![
            gate_held_entry("head-a", vec![f_old.clone()], "2026-01-01T00:00:00Z"),
            release_entry(
                "head-a",
                vec![f_old],
                "benign test fixture",
                "2026-01-02T00:00:00Z",
            ),
        ];
        // New head produces the same finding (identical fingerprint).
        let new = finding("unsafe_code_change", "tests/fixture.rs", "fp:same");
        let out = build_carry_forward_releases(
            &entries,
            "head-b",
            &[new],
            TASK_ID,
            PROJECT_ID,
            Some(PR),
            "2026-01-03T00:00:00Z",
        );
        assert_eq!(out.len(), 1, "one carry-forward release expected");
        let cf = &out[0];
        assert_eq!(cf.from_head_sha, "head-a");
        assert_eq!(cf.payload.head_sha, "head-b");
        assert!(cf.payload.carried_forward);
        assert_eq!(cf.payload.released_findings.len(), 1);
        assert!(cf.payload.rationale.contains("benign test fixture"));
        cf.payload
            .validate()
            .expect("carry-forward release validates");
    }

    /// Changed content (different fingerprint) is NOT carried forward — it
    /// re-holds and must be adjudicated afresh.
    #[test]
    fn changed_finding_is_not_carried_forward() {
        let f_old = finding("unsafe_code_change", "src/native.rs", "fp:old");
        let entries = vec![
            gate_held_entry("head-a", vec![f_old.clone()], "2026-01-01T00:00:00Z"),
            release_entry("head-a", vec![f_old], "reviewed", "2026-01-02T00:00:00Z"),
        ];
        // New head: same rule + file, but the patch content changed → new fp.
        let new = finding("unsafe_code_change", "src/native.rs", "fp:new");
        let out = build_carry_forward_releases(
            &entries,
            "head-b",
            &[new],
            TASK_ID,
            PROJECT_ID,
            Some(PR),
            "2026-01-03T00:00:00Z",
        );
        assert!(
            out.is_empty(),
            "changed content must not be carried forward"
        );
    }

    /// A finding never released on any prior head is not carried forward.
    #[test]
    fn never_released_finding_is_not_carried_forward() {
        let f_old = finding("migration_change", "migrations/1.sql", "fp:mig");
        let entries = vec![gate_held_entry(
            "head-a",
            vec![f_old],
            "2026-01-01T00:00:00Z",
        )];
        let new = finding("migration_change", "migrations/1.sql", "fp:mig");
        let out = build_carry_forward_releases(
            &entries,
            "head-b",
            &[new],
            TASK_ID,
            PROJECT_ID,
            Some(PR),
            "2026-01-03T00:00:00Z",
        );
        assert!(
            out.is_empty(),
            "unreleased finding must re-hold, not carry forward"
        );
    }

    /// Blank fingerprints (pre-fingerprint rows) never match.
    #[test]
    fn blank_fingerprint_never_matches() {
        let f_old = finding("unsafe_code_change", "tests/x.rs", "");
        let entries = vec![release_entry(
            "head-a",
            vec![f_old],
            "reviewed",
            "2026-01-02T00:00:00Z",
        )];
        let new = finding("unsafe_code_change", "tests/x.rs", "");
        let out = build_carry_forward_releases(
            &entries,
            "head-b",
            &[new],
            TASK_ID,
            PROJECT_ID,
            Some(PR),
            "2026-01-03T00:00:00Z",
        );
        assert!(out.is_empty(), "blank fingerprints must not match");
    }

    /// Already re-released on the new head → no duplicate carry-forward.
    #[test]
    fn already_released_on_new_head_is_skipped() {
        let f = finding("unsafe_code_change", "tests/y.rs", "fp:dup");
        let entries = vec![
            release_entry("head-a", vec![f.clone()], "ok", "2026-01-02T00:00:00Z"),
            release_entry("head-b", vec![f.clone()], "ok", "2026-01-03T00:00:00Z"),
        ];
        let out = build_carry_forward_releases(
            &entries,
            "head-b",
            &[f],
            TASK_ID,
            PROJECT_ID,
            Some(PR),
            "2026-01-04T00:00:00Z",
        );
        assert!(
            out.is_empty(),
            "must not duplicate an existing new-head release"
        );
    }
}
