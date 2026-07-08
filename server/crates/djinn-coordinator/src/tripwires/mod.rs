//! Tripwire foundation: policy schema, reason codes, and ADR-020-style activity
//! payloads.
//!
//! This module is the additive coordinator contract layer for proposal `hdlv`
//! (deterministic pre-merge tripwires + independent audit loop). It is **pure**
//! — no DB, GitHub, or LLM/provider calls — so it can be unit-tested in
//! isolation and consumed by:
//!
//! - the deterministic rule engine (sibling task `gl0p`);
//! - the rule fixtures (sibling tasks `imb4` and `na0w`);
//! - the enforcement epic (`nptj`) when wiring into PR polling and merge;
//! - the hold queue UI (`r3qg`);
//! - the audit sampler (`zuir`) and reporting epic (`cdb6`).
//!
//! All downstream code MUST consume the contracts defined here rather than
//! redefining reason codes, event shapes, or policy fields. If a sibling epic
//! (`zxcj`) later introduces a global reason-code registry, the
//! [`tripwire.*`](reason_codes) constants migrate by aliasing and the event
//! payload structs stay byte-compatible.
//!
//! See [`design/wuom-roadmap`] for the wave-1 decomposition and
//! [`design/nptj-roadmap`] for the enforcement follow-on.

pub mod activity_payloads;
pub mod engine;
pub mod policy;
pub mod reason_codes;

// ─── Re-exports for sibling tripwire modules ─────────────────────────────────
//
// Sibling modules (rules — added by follow-on tasks `imb4`, `na0w`) reach
// contract types via `use crate::tripwires::*`. The re-exports below are
// intentionally additive: this wave-1 task introduces the contract skeleton
// and engine primitives but does not yet consume them from another module.
// Downstream tasks reference these names; suppress the unused-import warning
// until those consumers land.

#[allow(unused_imports)]
pub use activity_payloads::{
    TRIPWIRE_EVENT_BREAK_GLASS, TRIPWIRE_EVENT_GATE_HELD, TRIPWIRE_EVENT_GATE_PASSED,
    TRIPWIRE_EVENT_GATE_REPORT_ONLY, TRIPWIRE_EVENT_HOLD_RELEASED, TRIPWIRE_EVENT_POLICY_CHANGED,
    TRIPWIRE_EVENT_TAMPER_LABEL_REMOVED, TripwireBreakGlassPayload, TripwireEvidenceSpan,
    TripwireFindingSummary, TripwireGateDecisionPayload, TripwireHoldReleasedPayload,
    TripwirePolicyChangedPayload, TripwireSeverity, TripwireTamperPayload,
};
#[allow(unused_imports)]
pub use engine::{
    ChangedFile, ChangedFileStatus, DiffHunk, EvidenceSpan, GateOutcome, RawFinding,
    TripwireEvaluationInput, TripwireFinding, TripwireFindingSeverity, TripwireGateDecision,
    build_finding_idempotency_key, build_gate_idempotency_key, evaluate, sort_findings,
};
#[allow(unused_imports)]
pub use policy::{
    BoundaryAllowlistRef, BoundaryPathRuleConfig, CIWorkflowRuleConfig,
    DependencyIdentityRuleConfig, GeneratedExclusions, LargeDeleteRewriteRuleConfig,
    MigrationRuleConfig, NetworkEgressRuleConfig, PolicySource, TripwirePolicy,
    UnsafeCodeRuleConfig, VendorExclusions,
};
#[allow(unused_imports)]
pub use reason_codes::{
    REASON_CI_WORKFLOW_CHANGE, REASON_DEPENDENCY_IDENTITY_CHANGED, REASON_LARGE_DELETE_OR_REWRITE,
    REASON_MIGRATION_CHANGE, REASON_NETWORK_EGRESS_CHANGE, REASON_PATH_BOUNDARY_CHANGE,
    REASON_UNSAFE_CODE_CHANGE, TripwireRuleId, reason_code_for_rule,
};
