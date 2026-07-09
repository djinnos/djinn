//! Stable namespaced reason-code constants for tripwire findings.
//!
//! These constants are the **single source of truth** for the `tripwire.*`
//! reason-code namespace used by every gate, hold, release, policy-change,
//! tamper, and break-glass activity payload. They are deliberately local to
//! the coordinator today; if a sibling epic (`zxcj`) later introduces a
//! global reason-code registry, the existing constants migrate by aliasing
//! — the literals MUST NOT change, because persisted activity rows and audit
//! dashboards already key on them.
//!
//! Adding a new rule family? Add a new constant below, add the new variant
//! to [`TripwireRuleId`], and extend [`reason_code_for_rule`]. Then add the
//! matching per-rule config struct to [`super::policy`].

// Contract types are intentionally additive — sibling tasks (`gl0p`, `imb4`,
// `na0w`, `nptj`, `r3qg`, `zuir`, `cdb6`) consume them. Suppress dead_code
// warnings at the module level so consumers can land incrementally without
// this module churning.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ─── Stable reason-code constants ──────────────────────────────────────────

/// Migration file created, changed, or deleted.
pub const REASON_MIGRATION_CHANGE: &str = "tripwire.migration.changed";

/// External package identity added, removed, renamed, source-switched, or
/// major version bumped past configured threshold.
pub const REASON_DEPENDENCY_IDENTITY_CHANGED: &str = "tripwire.dependency.identity_changed";

/// New outbound host, protocol, SDK, webhook target, or network client
/// introduced.
pub const REASON_NETWORK_EGRESS_CHANGE: &str = "tripwire.network.egress_changed";

/// `unsafe` block or FFI / escape hatch added or materially changed.
pub const REASON_UNSAFE_CODE_CHANGE: &str = "tripwire.unsafe.code_changed";

/// Changed path matches auth, permission, secrets, deployment, billing, or
/// capability-boundary allowlist patterns.
pub const REASON_PATH_BOUNDARY_CHANGE: &str = "tripwire.path.boundary_changed";

/// Deletion or rewrite exceeds configured line, file, or percentage
/// thresholds after generated/vendor exclusions.
pub const REASON_LARGE_DELETE_OR_REWRITE: &str = "tripwire.large_delete_or_rewrite";

/// Workflow, CI, release, deploy, or automation policy files changed.
pub const REASON_CI_WORKFLOW_CHANGE: &str = "tripwire.ci.workflow_changed";

// ─── Rule ID enum + mapping ─────────────────────────────────────────────────

/// Stable identifier for each rule family the tripwire engine evaluates.
///
/// These IDs are what the engine emits in `rule_id` fields of findings and
/// what policy code uses to switch on per-rule config. They are NOT the
/// reason codes themselves — the reason code is the stable, user-facing
/// label that propagates to UI / audit / activity rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TripwireRuleId {
    /// Migration file created, changed, or deleted.
    MigrationChange,
    /// External package identity changed.
    DependencyIdentityChange,
    /// New outbound network surface introduced.
    NetworkEgressChange,
    /// Unsafe / FFI / escape hatch change.
    UnsafeCodeChange,
    /// Changed path matches boundary allowlist patterns.
    BoundaryPathChange,
    /// Large deletion / rewrite over configured threshold.
    LargeDeleteOrRewrite,
    /// Workflow / CI / release / deploy / automation policy change.
    /// Explicit `rename` because `snake_case` would expand the `CI` token
    /// to `c_i` which is not the proposal rule id we want on the wire.
    #[serde(rename = "ci_workflow_change")]
    CIWorkflowChange,
}

impl TripwireRuleId {
    /// Stable string identifier used in JSON payloads and idempotency
    /// keys. Engine code uses this literal, not the enum discriminant, so
    /// the wire format is decoupled from the Rust representation.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::MigrationChange => "migration_change",
            Self::DependencyIdentityChange => "dependency_identity_change",
            Self::NetworkEgressChange => "network_egress_change",
            Self::UnsafeCodeChange => "unsafe_code_change",
            Self::BoundaryPathChange => "boundary_path_change",
            Self::LargeDeleteOrRewrite => "large_delete_or_rewrite",
            Self::CIWorkflowChange => "ci_workflow_change",
        }
    }

    /// Parse a stable wire literal (the [`as_str`](Self::as_str) form, as
    /// persisted in activity payloads' `rule_id`) back into a
    /// [`TripwireRuleId`]. Returns `None` for an unrecognised literal.
    ///
    /// Used by the merge-boundary hold reconciler to recover the rule family
    /// from a persisted active-hold finding so it can consult the per-rule
    /// adjudication policy (arbiter vs human).
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "migration_change" => Self::MigrationChange,
            "dependency_identity_change" => Self::DependencyIdentityChange,
            "network_egress_change" => Self::NetworkEgressChange,
            "unsafe_code_change" => Self::UnsafeCodeChange,
            "boundary_path_change" => Self::BoundaryPathChange,
            "large_delete_or_rewrite" => Self::LargeDeleteOrRewrite,
            "ci_workflow_change" => Self::CIWorkflowChange,
            _ => return None,
        })
    }
}

/// Map a [`TripwireRuleId`] to its stable, namespaced reason code.
///
/// This is the canonical mapping used by:
/// - the deterministic rule engine when populating `reason_code` on a
///   finding;
/// - the gate-decision payload builder (each finding inherits its rule's
///   reason code);
/// - the hold-release payload (the reason code being released);
/// - the policy-change payload (rules whose `report_only` toggled).
///
/// Adding a rule family is impossible to forget because the match is
/// exhaustive — a new [`TripwireRuleId`] variant will fail to compile here
/// until a mapping is added.
pub const fn reason_code_for_rule(rule: TripwireRuleId) -> &'static str {
    match rule {
        TripwireRuleId::MigrationChange => REASON_MIGRATION_CHANGE,
        TripwireRuleId::DependencyIdentityChange => REASON_DEPENDENCY_IDENTITY_CHANGED,
        TripwireRuleId::NetworkEgressChange => REASON_NETWORK_EGRESS_CHANGE,
        TripwireRuleId::UnsafeCodeChange => REASON_UNSAFE_CODE_CHANGE,
        TripwireRuleId::BoundaryPathChange => REASON_PATH_BOUNDARY_CHANGE,
        TripwireRuleId::LargeDeleteOrRewrite => REASON_LARGE_DELETE_OR_REWRITE,
        TripwireRuleId::CIWorkflowChange => REASON_CI_WORKFLOW_CHANGE,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every reason-code constant must carry the `tripwire.` namespace
    /// prefix. This catches typos like `tripware.` or `tripwire-` that
    /// would silently desynchronize UI / audit dashboards.
    #[test]
    fn all_reason_codes_carry_tripwire_namespace_prefix() {
        let codes = [
            REASON_MIGRATION_CHANGE,
            REASON_DEPENDENCY_IDENTITY_CHANGED,
            REASON_NETWORK_EGRESS_CHANGE,
            REASON_UNSAFE_CODE_CHANGE,
            REASON_PATH_BOUNDARY_CHANGE,
            REASON_LARGE_DELETE_OR_REWRITE,
            REASON_CI_WORKFLOW_CHANGE,
        ];
        for code in codes {
            assert!(
                code.starts_with("tripwire."),
                "reason code {code:?} must start with `tripwire.` namespace prefix"
            );
        }
    }

    /// Reason codes must be unique across the seven rule families.
    /// Duplicates would collapse distinct findings in audit / UI.
    #[test]
    fn reason_codes_are_unique_across_rule_families() {
        let codes = [
            REASON_MIGRATION_CHANGE,
            REASON_DEPENDENCY_IDENTITY_CHANGED,
            REASON_NETWORK_EGRESS_CHANGE,
            REASON_UNSAFE_CODE_CHANGE,
            REASON_PATH_BOUNDARY_CHANGE,
            REASON_LARGE_DELETE_OR_REWRITE,
            REASON_CI_WORKFLOW_CHANGE,
        ];
        let unique: HashSet<&str> = codes.iter().copied().collect();
        assert_eq!(
            unique.len(),
            codes.len(),
            "reason codes must be unique; duplicates found: {codes:?}"
        );
    }

    /// Every [`TripwireRuleId`] variant must map to a non-empty reason
    /// code and that mapping must be deterministic (idempotent).
    #[test]
    fn rule_id_to_reason_code_mapping_is_total_and_stable() {
        let rules = [
            TripwireRuleId::MigrationChange,
            TripwireRuleId::DependencyIdentityChange,
            TripwireRuleId::NetworkEgressChange,
            TripwireRuleId::UnsafeCodeChange,
            TripwireRuleId::BoundaryPathChange,
            TripwireRuleId::LargeDeleteOrRewrite,
            TripwireRuleId::CIWorkflowChange,
        ];
        let mut seen: HashSet<(&'static str, &'static str)> = HashSet::new();
        for rule in rules {
            let code = reason_code_for_rule(rule);
            assert!(
                !code.is_empty(),
                "rule {rule:?} must map to a non-empty reason code"
            );
            assert!(
                code.starts_with("tripwire."),
                "mapped code {code:?} for rule {rule:?} must keep the namespace prefix"
            );
            // Pair (rule, code) must be unique — duplicate mappings would
            // mean two rules collapse to the same reason.
            assert!(
                seen.insert((rule.as_str(), code)),
                "duplicate (rule, code) pair: {} -> {code}",
                rule.as_str()
            );
        }
    }

    /// `TripwireRuleId::as_str` must produce snake_case wire literals
    /// matching the proposal rule families. Persisted activity rows may
    /// key on these literals so they must not change casually.
    #[test]
    fn rule_id_as_str_is_snake_case_and_stable() {
        assert_eq!(TripwireRuleId::MigrationChange.as_str(), "migration_change");
        assert_eq!(
            TripwireRuleId::DependencyIdentityChange.as_str(),
            "dependency_identity_change"
        );
        assert_eq!(
            TripwireRuleId::NetworkEgressChange.as_str(),
            "network_egress_change"
        );
        assert_eq!(
            TripwireRuleId::UnsafeCodeChange.as_str(),
            "unsafe_code_change"
        );
        assert_eq!(
            TripwireRuleId::BoundaryPathChange.as_str(),
            "boundary_path_change"
        );
        assert_eq!(
            TripwireRuleId::LargeDeleteOrRewrite.as_str(),
            "large_delete_or_rewrite"
        );
        assert_eq!(
            TripwireRuleId::CIWorkflowChange.as_str(),
            "ci_workflow_change"
        );
    }

    /// `TripwireRuleId` must round-trip through serde so the engine can
    /// emit `rule_id` as a JSON string and the UI can deserialize it back
    /// into the enum.
    #[test]
    fn rule_id_serde_round_trip() {
        let cases = [
            (TripwireRuleId::MigrationChange, "\"migration_change\""),
            (
                TripwireRuleId::DependencyIdentityChange,
                "\"dependency_identity_change\"",
            ),
            (
                TripwireRuleId::NetworkEgressChange,
                "\"network_egress_change\"",
            ),
            (TripwireRuleId::UnsafeCodeChange, "\"unsafe_code_change\""),
            (
                TripwireRuleId::BoundaryPathChange,
                "\"boundary_path_change\"",
            ),
            (
                TripwireRuleId::LargeDeleteOrRewrite,
                "\"large_delete_or_rewrite\"",
            ),
            (TripwireRuleId::CIWorkflowChange, "\"ci_workflow_change\""),
        ];
        for (rule, expected_json) in cases {
            let json = serde_json::to_string(&rule).expect("serialize rule id");
            assert_eq!(
                json, expected_json,
                "rule {rule:?} must serialize to {expected_json}"
            );
            let back: TripwireRuleId = serde_json::from_str(&json).expect("deserialize rule id");
            assert_eq!(rule, back);
        }
    }
}
