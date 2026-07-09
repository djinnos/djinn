//! Typed ADR-020-style activity payload structs for tripwire events.
//!
//! Every gate decision, hold release, policy change, direct-label tamper,
//! and break-glass event MUST be persisted as one of the structs in this
//! module. The structs are deliberately additive: downstream enforcement
//! (sibling epic `nptj`) builds them, downstream UI / audit (`r3qg`,
//! `zuir`, `cdb6`) reads them, and the schema is stable across the
//! coordinator's lifetime.
//!
//! Each payload embeds the fields required for explainable, reproducible
//! behavior:
//!
//! - `policy_revision` — the org policy snapshot evaluated against the diff.
//! - `allowlist_revision` — the boundary allowlist revision, when the
//!   payload's findings touch boundary rules (omitted otherwise).
//! - `task_id`, `project_id`, `pr_number`, `head_sha` — identity triple so
//!   the audit sampler can join activity back to PR / task ledger rows.
//! - `reason_code` and `findings` — explainability surface.
//! - `actor` / `rationale` — only present on operator-driven actions
//!   (release, policy change, break-glass).
//! - `idempotency_key` / `finding_keys` — stable keys derived from
//!   `(task_id, pr_number, head_sha, rule_id, evidence_span, policy_revision)`.
//!
//! The event-type string constants ([`TRIPWIRE_EVENT_GATE_HELD`] etc.) are
//! the literal values that go into the activity row's `action` column.

// Contract types are intentionally additive — sibling tasks (`nptj`, `r3qg`,
// `zuir`, `cdb6`) consume them. Suppress dead_code warnings at the module
// level so consumers can land incrementally without this module churning.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ─── Event-type constants ──────────────────────────────────────────────────

/// Gate held (block / human-review-hold) event. Emitted when the engine
/// produces at least one enforcement-on finding for the current PR head.
pub const TRIPWIRE_EVENT_GATE_HELD: &str = "tripwire.gate.held";

/// Gate passed event. Emitted when the engine produces no enforcement-on
/// findings for the current PR head.
pub const TRIPWIRE_EVENT_GATE_PASSED: &str = "tripwire.gate.passed";

/// Report-only gate decision event. Emitted alongside a passed gate when
/// one or more rules had `report_only=true` and tripped; UI dashboards use
/// this for advisory visibility without consuming a hold slot.
pub const TRIPWIRE_EVENT_GATE_REPORT_ONLY: &str = "tripwire.gate.report_only";

/// Hold released event. Emitted when an authorized actor releases a hold on
/// the current PR head (operator action; carries actor + rationale).
pub const TRIPWIRE_EVENT_HOLD_RELEASED: &str = "tripwire.hold.released";

/// Policy changed event. Emitted whenever the active [`crate::tripwires::TripwirePolicy`]
/// revision transitions (e.g. new `org_policy_set` lands). Carries a diff
/// summary of toggled rules.
pub const TRIPWIRE_EVENT_POLICY_CHANGED: &str = "tripwire.policy.changed";

/// Direct-label tamper event. Emitted when an active `human-review-hold`
/// label is removed outside the official release path and the enforcement
/// re-applies it (or surfaces the tamper to operators).
pub const TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED: &str = "tripwire.tamper.label_removed";

/// Break-glass event. Emitted when an authorized operator overrides an
/// active hold (for hotfix / rollback / disaster response). Carries actor,
/// rationale, and the findings being overridden.
pub const TRIPWIRE_EVENT_BREAK_GLASS: &str = "tripwire.break_glass";

// ─── Severity ──────────────────────────────────────────────────────────────

/// Severity classification attached to each [`TripwireFindingSummary`].
///
/// Currently binary: enforcement (`HumanReviewRequired`) means the gate is
/// held and the existing [`HUMAN_REVIEW_HOLD_LABEL`] applies; advisory
/// (`ReportOnly`) means the finding is logged but does not block. Future
/// severities (e.g. `Warn`, `Notice`) can be added without breaking
/// existing consumers because [`TripwireSeverity`] is non-exhaustive only
/// via `serde(other)`-free matching — adding a variant is a compile-time
/// breaking change for downstream exhaustive `match` blocks, which is
/// desirable for safety-critical code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TripwireSeverity {
    /// Enforcement-on finding: gate held, human-review-hold applied.
    HumanReviewRequired,
    /// Advisory-only finding: gate still passes but the activity row is
    /// recorded for operator visibility.
    ReportOnly,
}

impl TripwireSeverity {
    /// Stable wire literal used in JSON payloads.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::HumanReviewRequired => "human_review_required",
            Self::ReportOnly => "report_only",
        }
    }
}

// ─── Shared finding surface ────────────────────────────────────────────────

/// Evidence span inside a finding. Either line-precise (`start_line` /
/// `end_line`) when the diff is text-level, or file-level (both `None`)
/// when only the path is known (binary rewrites, large deletions, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwireEvidenceSpan {
    /// Repository-relative file path of the evidence.
    pub path: String,
    /// First line of the span, when known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub start_line: Option<u32>,
    /// Last line of the span (inclusive), when known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_line: Option<u32>,
    /// `true` when the evidence body has been redacted (e.g. secret
    /// material in a dependency manifest change). UI MUST render the
    /// finding without the raw snippet when this is `true`.
    #[serde(default)]
    pub evidence_redacted: bool,
}

impl TripwireEvidenceSpan {
    /// Construct a line-precise span.
    pub fn lines(path: impl Into<String>, start: u32, end: u32) -> Self {
        Self {
            path: path.into(),
            start_line: Some(start),
            end_line: Some(end),
            evidence_redacted: false,
        }
    }

    /// Construct a file-level span (no line numbers).
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            start_line: None,
            end_line: None,
            evidence_redacted: false,
        }
    }

    /// Mark the evidence body as redacted in the returned span.
    pub fn with_redacted(mut self) -> Self {
        self.evidence_redacted = true;
        self
    }
}

/// Compact finding summary embedded in every gate decision and most
/// operator-action payloads.
///
/// The shape is intentionally narrow: a stable `rule_id`, the corresponding
/// reason code, severity classification, evidence span, and a stable
/// idempotency key. The full finding body (with snippet, hunk, or diff
/// metadata) is persisted alongside but lives outside this struct to keep
/// the activity payload small.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwireFindingSummary {
    /// Stable rule-family identifier (e.g. `"migration_change"`).
    pub rule_id: String,
    /// Stable, namespaced reason code (e.g. `"tripwire.migration.changed"`).
    pub reason_code: String,
    /// Enforcement or advisory severity classification.
    pub severity: TripwireSeverity,
    /// Evidence span (path + optional line range).
    pub evidence: TripwireEvidenceSpan,
    /// Stable idempotency key for this specific finding. Derived from
    /// `(task_id, pr_number, head_sha, rule_id, evidence_path, evidence_line_range, policy_revision)`.
    /// See engine module (sibling task `gl0p`) for the canonical builder.
    pub idempotency_key: String,
    /// Head-independent content fingerprint. Stable across PR heads while the
    /// underlying flagged content (rule + file + patch hunk) is unchanged —
    /// unlike [`idempotency_key`](TripwireFindingSummary::idempotency_key)
    /// which includes `head_sha`. The gate's release carry-forward keys on
    /// this to recognise a finding already adjudicated on a prior head.
    /// `#[serde(default)]` keeps pre-fingerprint activity rows decoding.
    #[serde(default)]
    pub content_fingerprint: String,
    /// When set, the reason this finding was downgraded from enforcement to
    /// report-only (e.g. the evidence path is a test file). Omitted from the
    /// wire when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub downgrade_reason: Option<String>,
}

// ─── Gate decision payload ─────────────────────────────────────────────────

/// Payload for `tripwire.gate.held`, `tripwire.gate.passed`, and
/// `tripwire.gate.report_only` events.
///
/// A single struct covers all three event variants; the event-type string
/// is selected from [`TRIPWIRE_EVENT_GATE_HELD`] / [`TRIPWIRE_EVENT_GATE_PASSED`]
/// / [`TRIPWIRE_EVENT_GATE_REPORT_ONLY`] based on the findings + per-rule
/// `report_only` flags. `held` implies at least one enforcement-on finding;
/// `report_only` implies findings but every one of them is advisory;
/// `passed` implies zero findings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwireGateDecisionPayload {
    /// Event-type literal — one of [`TRIPWIRE_EVENT_GATE_HELD`],
    /// [`TRIPWIRE_EVENT_GATE_PASSED`], [`TRIPWIRE_EVENT_GATE_REPORT_ONLY`].
    pub event_type: String,
    /// Task id this PR is associated with.
    pub task_id: String,
    /// Project id (multi-tenant key).
    pub project_id: String,
    /// PR number, when the decision is tied to a pull request.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pr_number: Option<u64>,
    /// Head SHA the engine evaluated against. Sticky hold state keys on
    /// this; a new head SHA supersedes the previous decision.
    pub head_sha: String,
    /// Base SHA, when the engine has it (for diff anchoring).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub base_sha: Option<String>,
    /// Org policy revision evaluated. Propagates into idempotency key.
    pub policy_revision: String,
    /// Boundary allowlist revision, when any boundary rule tripped. Omitted
    /// from `passed` payloads that did not consult the boundary allowlist.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub allowlist_revision: Option<String>,
    /// Findings emitted by the engine. Empty for `passed` events.
    pub findings: Vec<TripwireFindingSummary>,
    /// Compact count summary: number of enforcement-on findings.
    pub enforcement_finding_count: u32,
    /// Compact count summary: number of report-only findings.
    pub report_only_finding_count: u32,
    /// Stable idempotency key for this gate decision. Derived from
    /// `(task_id, pr_number, head_sha, policy_revision, allowlist_revision,
    /// sorted_finding_keys)` so the same logical decision recorded twice
    /// collapses to one activity row.
    pub idempotency_key: String,
    /// Wall-clock timestamp (RFC 3339 / ISO 8601) the engine produced the
    /// decision. Set by the caller; the struct itself does not read the
    /// clock (keeps the contract deterministic / side-effect-free).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub decided_at: Option<String>,
}

impl TripwireGateDecisionPayload {
    /// Validate that `event_type` is one of the three gate-event literals
    /// and that the count summary matches the findings vector. Returns
    /// `Ok(())` on success or an `Err` describing the inconsistency so
    /// callers can short-circuit before persistence.
    pub fn validate(&self) -> Result<(), String> {
        match self.event_type.as_str() {
            TRIPWIRE_EVENT_GATE_HELD
            | TRIPWIRE_EVENT_GATE_PASSED
            | TRIPWIRE_EVENT_GATE_REPORT_ONLY => {}
            other => {
                return Err(format!(
                    "gate decision payload must carry one of the gate event types, got {other:?}"
                ));
            }
        }
        let (enforced, report_only) = self.findings.iter().fold((0u32, 0u32), |(e, r), f| match f
            .severity
        {
            TripwireSeverity::HumanReviewRequired => (e + 1, r),
            TripwireSeverity::ReportOnly => (e, r + 1),
        });
        if enforced != self.enforcement_finding_count {
            return Err(format!(
                "enforcement_finding_count mismatch: declared {}, computed {}",
                self.enforcement_finding_count, enforced
            ));
        }
        if report_only != self.report_only_finding_count {
            return Err(format!(
                "report_only_finding_count mismatch: declared {}, computed {}",
                self.report_only_finding_count, report_only
            ));
        }
        Ok(())
    }
}

// ─── Hold-released payload ─────────────────────────────────────────────────

/// Payload for `tripwire.hold.released` events.
///
/// Emitted when an authorized operator releases a hold on the current PR
/// head. The actor + rationale are mandatory so the audit sampler can
/// later reconstruct who overrode what and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwireHoldReleasedPayload {
    pub event_type: String,
    pub task_id: String,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pr_number: Option<u64>,
    pub head_sha: String,
    pub policy_revision: String,
    /// Identifier of the actor releasing the hold (e.g. user id, system
    /// component name).
    pub released_by: String,
    /// Role of the actor at release time (e.g. `"lead"`, `"maintainer"`).
    pub released_by_role: String,
    /// Operator-supplied rationale explaining why the hold is being
    /// released without addressing every finding.
    pub rationale: String,
    /// Findings that were released (still recorded for audit).
    pub released_findings: Vec<TripwireFindingSummary>,
    /// `true` when this release was auto-carried-forward from a prior head:
    /// the same content (by content fingerprint) was already adjudicated and
    /// released on an earlier head of the same PR and is byte-identical on the
    /// new head, so the release is re-emitted for the new head referencing the
    /// prior rationale/releaser without re-running adjudication. `false` for a
    /// first-time (human- or arbiter-driven) release. `#[serde(default)]`
    /// keeps pre-carry-forward rows decoding.
    #[serde(default)]
    pub carried_forward: bool,
    /// Stable idempotency key for this release. Derived from
    /// `(task_id, pr_number, head_sha, released_by, policy_revision)`.
    pub idempotency_key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub released_at: Option<String>,
}

impl TripwireHoldReleasedPayload {
    /// Validate that `event_type` matches [`TRIPWIRE_EVENT_HOLD_RELEASED`]
    /// and that the actor / rationale fields are non-empty.
    pub fn validate(&self) -> Result<(), String> {
        if self.event_type != TRIPWIRE_EVENT_HOLD_RELEASED {
            return Err(format!(
                "hold-released payload must carry event_type = {}, got {:?}",
                TRIPWIRE_EVENT_HOLD_RELEASED, self.event_type
            ));
        }
        if self.released_by.trim().is_empty() {
            return Err("released_by must be non-empty".to_owned());
        }
        if self.released_by_role.trim().is_empty() {
            return Err("released_by_role must be non-empty".to_owned());
        }
        if self.rationale.trim().is_empty() {
            return Err("rationale must be non-empty".to_owned());
        }
        Ok(())
    }
}

// ─── Policy-changed payload ────────────────────────────────────────────────

/// Per-rule toggle recorded in a [`TripwirePolicyChangedPayload`]. Captures
/// the minimum diff operators need to understand what changed between two
/// policy revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwirePolicyRuleChange {
    /// Rule family identifier (e.g. `"migration_change"`).
    pub rule_id: String,
    /// Reason code the rule maps to (e.g. `"tripwire.migration.changed"`).
    pub reason_code: String,
    /// `enabled` flag before the change.
    pub was_enabled: bool,
    /// `enabled` flag after the change.
    pub is_enabled: bool,
    /// `report_only` flag before the change.
    pub was_report_only: bool,
    /// `report_only` flag after the change.
    pub is_report_only: bool,
}

/// Payload for `tripwire.policy.changed` events.
///
/// Emitted whenever the active policy revision transitions. The payload
/// records the previous + new revision labels, the provenance change, and
/// the per-rule diff so dashboards can attribute rule behavior changes to
/// specific policy revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwirePolicyChangedPayload {
    pub event_type: String,
    pub task_id: String,
    pub project_id: String,
    /// Previous policy revision (or `None` if there was no prior policy).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub previous_policy_revision: Option<String>,
    /// New policy revision.
    pub new_policy_revision: String,
    /// Provenance of the new policy (`default` vs `org`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub previous_policy_source: Option<String>,
    /// Provenance of the new policy (`default` vs `org`).
    pub new_policy_source: String,
    /// Previous boundary allowlist revision, when it changed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub previous_allowlist_revision: Option<String>,
    /// New boundary allowlist revision, when applicable.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub new_allowlist_revision: Option<String>,
    /// Per-rule toggle diff.
    pub rule_changes: Vec<TripwirePolicyRuleChange>,
    /// Actor that published the new policy (user id or system component).
    pub changed_by: String,
    /// Role of the actor at change time.
    pub changed_by_role: String,
    /// Optional operator-supplied rationale.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rationale: Option<String>,
    /// Stable idempotency key for this policy change.
    pub idempotency_key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub changed_at: Option<String>,
}

impl TripwirePolicyChangedPayload {
    /// Validate that `event_type` matches [`TRIPWIRE_EVENT_POLICY_CHANGED`]
    /// and that actor fields are non-empty.
    pub fn validate(&self) -> Result<(), String> {
        if self.event_type != TRIPWIRE_EVENT_POLICY_CHANGED {
            return Err(format!(
                "policy-changed payload must carry event_type = {}, got {:?}",
                TRIPWIRE_EVENT_POLICY_CHANGED, self.event_type
            ));
        }
        if self.changed_by.trim().is_empty() {
            return Err("changed_by must be non-empty".to_owned());
        }
        if self.changed_by_role.trim().is_empty() {
            return Err("changed_by_role must be non-empty".to_owned());
        }
        if self.previous_policy_revision.as_deref() == Some(&self.new_policy_revision) {
            return Err("previous_policy_revision must differ from new_policy_revision".to_owned());
        }
        Ok(())
    }
}

// ─── Tamper payload ────────────────────────────────────────────────────────

/// Payload for `tripwire.tamper.label_removed` events.
///
/// Emitted when an active `human-review-hold` label is removed outside the
/// official release path. The payload records the active findings whose
/// hold label was stripped so the audit sampler can correlate tamper
/// events with the gate decision that originally applied the label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwireTamperPayload {
    pub event_type: String,
    pub task_id: String,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pr_number: Option<u64>,
    pub head_sha: String,
    pub policy_revision: String,
    /// Identifier of the actor that performed the label removal (best
    /// effort: GitHub login, system component, or `"unknown"` when the
    /// actor could not be resolved).
    pub removed_by: String,
    /// Source of the removal observation — e.g. `"pr_poller"`,
    /// `"label_webhook"`, `"final_pre_merge_check"`.
    pub detection_source: String,
    /// Findings whose active hold label was stripped.
    pub tampered_findings: Vec<TripwireFindingSummary>,
    /// Whether the enforcement reapplied the label automatically.
    pub reapplied: bool,
    /// Stable idempotency key for this tamper detection.
    pub idempotency_key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detected_at: Option<String>,
}

impl TripwireTamperPayload {
    /// Validate that `event_type` matches [`TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED`]
    /// and that detection-source is non-empty.
    pub fn validate(&self) -> Result<(), String> {
        if self.event_type != TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED {
            return Err(format!(
                "tamper payload must carry event_type = {}, got {:?}",
                TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED, self.event_type
            ));
        }
        if self.detection_source.trim().is_empty() {
            return Err("detection_source must be non-empty".to_owned());
        }
        Ok(())
    }
}

// ─── Break-glass payload ───────────────────────────────────────────────────

/// Payload for `tripwire.break_glass` events.
///
/// Emitted when an authorized operator overrides an active hold for
/// hotfix / rollback / disaster response. The actor + rationale are
/// mandatory and the payload always carries the findings being overridden
/// so the audit sampler can later reconstruct why the hold was bypassed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TripwireBreakGlassPayload {
    pub event_type: String,
    pub task_id: String,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pr_number: Option<u64>,
    pub head_sha: String,
    pub policy_revision: String,
    /// Identifier of the actor performing the break-glass action.
    pub invoked_by: String,
    /// Role of the actor at break-glass time (e.g. `"lead"`,
    /// `"incident_commander"`).
    pub invoked_by_role: String,
    /// Operator-supplied rationale explaining the override (mandatory:
    /// break-glass without rationale is not a valid override).
    pub rationale: String,
    /// Findings whose hold is being bypassed.
    pub overridden_findings: Vec<TripwireFindingSummary>,
    /// Stable idempotency key for this break-glass action.
    pub idempotency_key: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub invoked_at: Option<String>,
}

impl TripwireBreakGlassPayload {
    /// Validate that `event_type` matches [`TRIPWIRE_EVENT_BREAK_GLASS`]
    /// and that actor + rationale fields are non-empty.
    pub fn validate(&self) -> Result<(), String> {
        if self.event_type != TRIPWIRE_EVENT_BREAK_GLASS {
            return Err(format!(
                "break-glass payload must carry event_type = {}, got {:?}",
                TRIPWIRE_EVENT_BREAK_GLASS, self.event_type
            ));
        }
        if self.invoked_by.trim().is_empty() {
            return Err("invoked_by must be non-empty".to_owned());
        }
        if self.invoked_by_role.trim().is_empty() {
            return Err("invoked_by_role must be non-empty".to_owned());
        }
        if self.rationale.trim().is_empty() {
            return Err("rationale must be non-empty".to_owned());
        }
        Ok(())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_finding(rule_id: &str, reason_code: &str, path: &str) -> TripwireFindingSummary {
        TripwireFindingSummary {
            rule_id: rule_id.to_owned(),
            reason_code: reason_code.to_owned(),
            severity: TripwireSeverity::HumanReviewRequired,
            evidence: TripwireEvidenceSpan::file(path.to_owned()),
            idempotency_key: format!("sha256:{rule_id}:{path}"),
            content_fingerprint: format!("fp:sha256:{rule_id}:{path}"),
            downgrade_reason: None,
        }
    }

    // ── Evidence span round-trip ───────────────────────────────────────────

    /// `TripwireEvidenceSpan::lines` and `TripwireEvidenceSpan::file` must
    /// produce distinguishable JSON shapes: line spans emit `start_line` /
    /// `end_line`, file spans omit them.
    #[test]
    fn evidence_span_line_vs_file_serde_round_trip() {
        let line = TripwireEvidenceSpan::lines("Cargo.toml", 17, 21);
        let file = TripwireEvidenceSpan::file("Cargo.lock");

        let line_json = serde_json::to_value(&line).unwrap();
        let file_json = serde_json::to_value(&file).unwrap();

        assert_eq!(line_json["path"], "Cargo.toml");
        assert_eq!(line_json["start_line"], 17);
        assert_eq!(line_json["end_line"], 21);
        assert_eq!(line_json["evidence_redacted"], false);

        assert_eq!(file_json["path"], "Cargo.lock");
        assert!(
            file_json.get("start_line").is_none(),
            "file-level span must omit start_line, got {file_json}"
        );
        assert!(
            file_json.get("end_line").is_none(),
            "file-level span must omit end_line, got {file_json}"
        );

        let line_back: TripwireEvidenceSpan = serde_json::from_value(line_json).unwrap();
        let file_back: TripwireEvidenceSpan = serde_json::from_value(file_json).unwrap();
        assert_eq!(line_back, line);
        assert_eq!(file_back, file);
    }

    /// `evidence_redacted` flag must round-trip and apply to both
    /// line-precise and file-level spans.
    #[test]
    fn evidence_span_redacted_flag_round_trips() {
        let line = TripwireEvidenceSpan::lines("secret.txt", 1, 5).with_redacted();
        let file = TripwireEvidenceSpan::file("secret.bin").with_redacted();
        for span in [line, file] {
            let json = serde_json::to_value(&span).unwrap();
            assert_eq!(json["evidence_redacted"], true);
            let back: TripwireEvidenceSpan = serde_json::from_value(json).unwrap();
            assert!(back.evidence_redacted);
        }
    }

    // ── Gate decision payload ──────────────────────────────────────────────

    /// Held gate payload must round-trip JSON cleanly and validate.
    #[test]
    fn gate_held_payload_serde_round_trip_and_validate() {
        let finding = sample_finding(
            "dependency_identity_change",
            "tripwire.dependency.identity_changed",
            "Cargo.toml",
        );
        let payload = TripwireGateDecisionPayload {
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            task_id: "task_123".to_owned(),
            project_id: "project_456".to_owned(),
            pr_number: Some(42),
            head_sha: "abc123".to_owned(),
            base_sha: Some("def456".to_owned()),
            policy_revision: "org-policy:17".to_owned(),
            allowlist_revision: Some("capability-boundary-allowlist:9".to_owned()),
            findings: vec![finding.clone()],
            enforcement_finding_count: 1,
            report_only_finding_count: 0,
            idempotency_key: "sha256:held:abc123".to_owned(),
            decided_at: Some("2026-01-01T00:00:00Z".to_owned()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: TripwireGateDecisionPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        payload.validate().expect("valid held payload");

        // Verify field shapes survive serialization.
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value["event_type"], TRIPWIRE_EVENT_GATE_HELD);
        assert_eq!(value["task_id"], "task_123");
        assert_eq!(value["pr_number"], 42);
        assert_eq!(value["enforcement_finding_count"], 1);
        assert_eq!(
            value["findings"][0]["rule_id"],
            "dependency_identity_change"
        );
    }

    /// Passed gate payload must validate with zero findings.
    #[test]
    fn gate_passed_payload_round_trip_and_validate() {
        let payload = TripwireGateDecisionPayload {
            event_type: TRIPWIRE_EVENT_GATE_PASSED.to_owned(),
            task_id: "task_pass".to_owned(),
            project_id: "project_p".to_owned(),
            pr_number: None,
            head_sha: "deadbeef".to_owned(),
            base_sha: None,
            policy_revision: "org-policy:17".to_owned(),
            allowlist_revision: None,
            findings: Vec::new(),
            enforcement_finding_count: 0,
            report_only_finding_count: 0,
            idempotency_key: "sha256:passed:deadbeef".to_owned(),
            decided_at: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: TripwireGateDecisionPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        payload.validate().expect("valid passed payload");
    }

    /// Report-only gate payload must mix enforcement-on (impossible) and
    /// advisory findings only — `validate` must reject mixed counts.
    #[test]
    fn gate_report_only_payload_round_trip_and_validate() {
        let advisory = TripwireFindingSummary {
            rule_id: "ci_workflow_change".to_owned(),
            reason_code: "tripwire.ci.workflow_changed".to_owned(),
            severity: TripwireSeverity::ReportOnly,
            evidence: TripwireEvidenceSpan::file(".github/workflows/ci.yml"),
            idempotency_key: "sha256:ci:yml".to_owned(),
            content_fingerprint: "fp:sha256:ci:yml".to_owned(),
            downgrade_reason: None,
        };
        let payload = TripwireGateDecisionPayload {
            event_type: TRIPWIRE_EVENT_GATE_REPORT_ONLY.to_owned(),
            task_id: "task_ro".to_owned(),
            project_id: "project_ro".to_owned(),
            pr_number: Some(7),
            head_sha: "cafe".to_owned(),
            base_sha: None,
            policy_revision: "org-policy:17".to_owned(),
            allowlist_revision: None,
            findings: vec![advisory],
            enforcement_finding_count: 0,
            report_only_finding_count: 1,
            idempotency_key: "sha256:ro:cafe".to_owned(),
            decided_at: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: TripwireGateDecisionPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        payload.validate().expect("valid report-only payload");
    }

    /// `validate` must reject payloads whose declared counts disagree with
    /// the findings vector.
    #[test]
    fn gate_payload_validate_rejects_mismatched_counts() {
        let payload = TripwireGateDecisionPayload {
            event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
            task_id: "t".to_owned(),
            project_id: "p".to_owned(),
            pr_number: None,
            head_sha: "h".to_owned(),
            base_sha: None,
            policy_revision: "org-policy:1".to_owned(),
            allowlist_revision: None,
            findings: vec![sample_finding(
                "migration_change",
                "tripwire.migration.changed",
                "migrations/0001_init.sql",
            )],
            enforcement_finding_count: 0, // wrong — should be 1
            report_only_finding_count: 0,
            idempotency_key: "k".to_owned(),
            decided_at: None,
        };
        let err = payload
            .validate()
            .expect_err("must reject mismatched count");
        assert!(err.contains("enforcement_finding_count"), "got: {err}");
    }

    /// `validate` must reject payloads with an unknown `event_type`.
    #[test]
    fn gate_payload_validate_rejects_unknown_event_type() {
        let payload = TripwireGateDecisionPayload {
            event_type: "tripwire.gate.bogus".to_owned(),
            task_id: "t".to_owned(),
            project_id: "p".to_owned(),
            pr_number: None,
            head_sha: "h".to_owned(),
            base_sha: None,
            policy_revision: "org-policy:1".to_owned(),
            allowlist_revision: None,
            findings: Vec::new(),
            enforcement_finding_count: 0,
            report_only_finding_count: 0,
            idempotency_key: "k".to_owned(),
            decided_at: None,
        };
        let err = payload
            .validate()
            .expect_err("must reject unknown event_type");
        assert!(err.contains("tripwire.gate.bogus"), "got: {err}");
    }

    // ── Hold-released payload ──────────────────────────────────────────────

    /// Hold-released payload must round-trip and validate.
    #[test]
    fn hold_released_payload_serde_round_trip_and_validate() {
        let payload = TripwireHoldReleasedPayload {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            task_id: "task_release".to_owned(),
            project_id: "project_rel".to_owned(),
            pr_number: Some(99),
            head_sha: "feedface".to_owned(),
            policy_revision: "org-policy:17".to_owned(),
            released_by: "user_789".to_owned(),
            released_by_role: "lead".to_owned(),
            rationale: "downstream rollback approved".to_owned(),
            released_findings: vec![sample_finding(
                "large_delete_or_rewrite",
                "tripwire.large_delete_or_rewrite",
                "legacy/big_file.rs",
            )],
            carried_forward: false,
            idempotency_key: "sha256:release:feedface".to_owned(),
            released_at: Some("2026-02-02T00:00:00Z".to_owned()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: TripwireHoldReleasedPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        payload.validate().expect("valid release payload");
    }

    /// Hold-released payload must reject empty actor / rationale.
    #[test]
    fn hold_released_validate_rejects_empty_actor_and_rationale() {
        let base = TripwireHoldReleasedPayload {
            event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
            task_id: "t".to_owned(),
            project_id: "p".to_owned(),
            pr_number: None,
            head_sha: "h".to_owned(),
            policy_revision: "org-policy:1".to_owned(),
            released_by: "user".to_owned(),
            released_by_role: "lead".to_owned(),
            rationale: "rationale".to_owned(),
            released_findings: Vec::new(),
            carried_forward: false,
            idempotency_key: "k".to_owned(),
            released_at: None,
        };
        let mut p = base.clone();
        p.released_by = String::new();
        assert!(p.validate().unwrap_err().contains("released_by"));

        let mut p = base.clone();
        p.released_by_role = "  ".to_owned();
        assert!(p.validate().unwrap_err().contains("released_by_role"));

        let mut p = base.clone();
        p.rationale = String::new();
        assert!(p.validate().unwrap_err().contains("rationale"));

        let mut p = base;
        p.event_type = "tripwire.bogus".to_owned();
        assert!(p.validate().unwrap_err().contains("event_type"));
    }

    // ── Policy-changed payload ──────────────────────────────────────────────

    /// Policy-changed payload must round-trip and validate.
    #[test]
    fn policy_changed_payload_serde_round_trip_and_validate() {
        let payload = TripwirePolicyChangedPayload {
            event_type: TRIPWIRE_EVENT_POLICY_CHANGED.to_owned(),
            task_id: "task_pc".to_owned(),
            project_id: "project_pc".to_owned(),
            previous_policy_revision: Some("org-policy:16".to_owned()),
            new_policy_revision: "org-policy:17".to_owned(),
            previous_policy_source: Some("org".to_owned()),
            new_policy_source: "org".to_owned(),
            previous_allowlist_revision: Some("capability-boundary-allowlist:8".to_owned()),
            new_allowlist_revision: Some("capability-boundary-allowlist:9".to_owned()),
            rule_changes: vec![TripwirePolicyRuleChange {
                rule_id: "network_egress_change".to_owned(),
                reason_code: "tripwire.network.egress_changed".to_owned(),
                was_enabled: true,
                is_enabled: true,
                was_report_only: true,
                is_report_only: false,
            }],
            changed_by: "user_pc".to_owned(),
            changed_by_role: "maintainer".to_owned(),
            rationale: Some("tightened egress rule per security review".to_owned()),
            idempotency_key: "sha256:pc:org-policy:17".to_owned(),
            changed_at: Some("2026-03-03T00:00:00Z".to_owned()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: TripwirePolicyChangedPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        payload.validate().expect("valid policy change payload");
    }

    /// Policy-changed payload must reject revisions that did not change.
    #[test]
    fn policy_changed_validate_rejects_unchanged_revision() {
        let payload = TripwirePolicyChangedPayload {
            event_type: TRIPWIRE_EVENT_POLICY_CHANGED.to_owned(),
            task_id: "t".to_owned(),
            project_id: "p".to_owned(),
            previous_policy_revision: Some("org-policy:17".to_owned()),
            new_policy_revision: "org-policy:17".to_owned(),
            previous_policy_source: None,
            new_policy_source: "org".to_owned(),
            previous_allowlist_revision: None,
            new_allowlist_revision: None,
            rule_changes: Vec::new(),
            changed_by: "user".to_owned(),
            changed_by_role: "maintainer".to_owned(),
            rationale: None,
            idempotency_key: "k".to_owned(),
            changed_at: None,
        };
        let err = payload
            .validate()
            .expect_err("must reject unchanged revision");
        assert!(err.contains("previous_policy_revision"), "got: {err}");
    }

    // ── Tamper payload ─────────────────────────────────────────────────────

    /// Tamper payload must round-trip and validate.
    #[test]
    fn tamper_payload_serde_round_trip_and_validate() {
        let payload = TripwireTamperPayload {
            event_type: TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED.to_owned(),
            task_id: "task_t".to_owned(),
            project_id: "project_t".to_owned(),
            pr_number: Some(3),
            head_sha: "deadbeef".to_owned(),
            policy_revision: "org-policy:17".to_owned(),
            removed_by: "user_t".to_owned(),
            detection_source: "pr_poller".to_owned(),
            tampered_findings: vec![sample_finding(
                "unsafe_code_change",
                "tripwire.unsafe.code_changed",
                "server/crates/foo/src/native.rs",
            )],
            reapplied: true,
            idempotency_key: "sha256:tamper:3:deadbeef".to_owned(),
            detected_at: Some("2026-04-04T00:00:00Z".to_owned()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: TripwireTamperPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        payload.validate().expect("valid tamper payload");
    }

    /// Tamper payload must reject an empty `detection_source`.
    #[test]
    fn tamper_payload_validate_rejects_empty_detection_source() {
        let payload = TripwireTamperPayload {
            event_type: TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED.to_owned(),
            task_id: "t".to_owned(),
            project_id: "p".to_owned(),
            pr_number: None,
            head_sha: "h".to_owned(),
            policy_revision: "org-policy:1".to_owned(),
            removed_by: "u".to_owned(),
            detection_source: String::new(),
            tampered_findings: Vec::new(),
            reapplied: false,
            idempotency_key: "k".to_owned(),
            detected_at: None,
        };
        assert!(payload.validate().unwrap_err().contains("detection_source"));
    }

    // ── Break-glass payload ────────────────────────────────────────────────

    /// Break-glass payload must round-trip and validate.
    #[test]
    fn break_glass_payload_serde_round_trip_and_validate() {
        let payload = TripwireBreakGlassPayload {
            event_type: TRIPWIRE_EVENT_BREAK_GLASS.to_owned(),
            task_id: "task_bg".to_owned(),
            project_id: "project_bg".to_owned(),
            pr_number: Some(1),
            head_sha: "badf00d".to_owned(),
            policy_revision: "org-policy:17".to_owned(),
            invoked_by: "user_bg".to_owned(),
            invoked_by_role: "incident_commander".to_owned(),
            rationale: "SEV-1 hotfix to restore checkout".to_owned(),
            overridden_findings: vec![sample_finding(
                "ci_workflow_change",
                "tripwire.ci.workflow_changed",
                ".github/workflows/deploy.yml",
            )],
            idempotency_key: "sha256:bg:badf00d".to_owned(),
            invoked_at: Some("2026-05-05T00:00:00Z".to_owned()),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: TripwireBreakGlassPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(payload, back);
        payload.validate().expect("valid break-glass payload");
    }

    /// Break-glass payload must reject an empty rationale — override
    /// without explanation is not a valid break-glass.
    #[test]
    fn break_glass_validate_rejects_empty_rationale() {
        let payload = TripwireBreakGlassPayload {
            event_type: TRIPWIRE_EVENT_BREAK_GLASS.to_owned(),
            task_id: "t".to_owned(),
            project_id: "p".to_owned(),
            pr_number: None,
            head_sha: "h".to_owned(),
            policy_revision: "org-policy:1".to_owned(),
            invoked_by: "u".to_owned(),
            invoked_by_role: "incident_commander".to_owned(),
            rationale: String::new(),
            overridden_findings: Vec::new(),
            idempotency_key: "k".to_owned(),
            invoked_at: None,
        };
        let err = payload.validate().expect_err("must reject empty rationale");
        assert!(err.contains("rationale"), "got: {err}");
    }

    // ── Severity wire literal ──────────────────────────────────────────────

    /// Severity must serialize to snake_case literals that the UI already
    /// keys on.
    #[test]
    fn severity_serde_round_trip() {
        for (sev, expected) in [
            (
                TripwireSeverity::HumanReviewRequired,
                "\"human_review_required\"",
            ),
            (TripwireSeverity::ReportOnly, "\"report_only\""),
        ] {
            let json = serde_json::to_string(&sev).unwrap();
            assert_eq!(json, expected);
            let back: TripwireSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(sev, back);
            assert_eq!(sev.as_str(), expected.trim_matches('"'));
        }
    }

    // ── Optional fields skip cleanly ───────────────────────────────────────

    /// Optional fields must be skipped (not emitted as `null`) on
    /// serialization so JSON shape stays tight.
    #[test]
    fn optional_fields_are_skipped_when_none() {
        let payload = TripwireGateDecisionPayload {
            event_type: TRIPWIRE_EVENT_GATE_PASSED.to_owned(),
            task_id: "t".to_owned(),
            project_id: "p".to_owned(),
            pr_number: None,
            head_sha: "h".to_owned(),
            base_sha: None,
            policy_revision: "org-policy:1".to_owned(),
            allowlist_revision: None,
            findings: Vec::new(),
            enforcement_finding_count: 0,
            report_only_finding_count: 0,
            idempotency_key: "k".to_owned(),
            decided_at: None,
        };
        let value = serde_json::to_value(&payload).unwrap();
        assert!(value.get("pr_number").is_none(), "got: {value}");
        assert!(value.get("base_sha").is_none(), "got: {value}");
        assert!(value.get("allowlist_revision").is_none(), "got: {value}");
        assert!(value.get("decided_at").is_none(), "got: {value}");
    }
}
