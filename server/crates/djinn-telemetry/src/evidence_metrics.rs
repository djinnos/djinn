//! Bounded, identifier-free rollout counters for refinement evidence.
//!
//! Only closed enums reach the metric facade, preventing proposal, task,
//! session, plan, invocation, commit, and worktree identifiers from labels.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceStage { Demand, Plan, Invocation, Terminal, Rejection }
impl EvidenceStage { pub const fn label(self) -> &'static str { match self { Self::Demand => "demand", Self::Plan => "plan", Self::Invocation => "invocation", Self::Terminal => "terminal", Self::Rejection => "rejection" } } }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceOutcome { Accepted, Captured, Attempted, Ok, Degraded, Timeout, Error, Resolved, Partial, Unresolved, Failed }
impl EvidenceOutcome { pub const fn label(self) -> &'static str { match self { Self::Accepted => "accepted", Self::Captured => "captured", Self::Attempted => "attempted", Self::Ok => "ok", Self::Degraded => "degraded", Self::Timeout => "timeout", Self::Error => "error", Self::Resolved => "resolved", Self::Partial => "partial", Self::Unresolved => "unresolved", Self::Failed => "failed" } } }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceRejection { Validation, Persistence, NoFrozenPlan, AlreadyFinalized, UnknownCheck, MethodMismatch, InvalidAnchor }
impl EvidenceRejection { pub const fn label(self) -> &'static str { match self { Self::Validation => "validation", Self::Persistence => "persistence", Self::NoFrozenPlan => "no_frozen_plan", Self::AlreadyFinalized => "already_finalized", Self::UnknownCheck => "unknown_check", Self::MethodMismatch => "method_mismatch", Self::InvalidAnchor => "invalid_anchor" } } }

/// Every legal metric label set. This is not an independent-axis list: a
/// nonsensical pair such as `demand/timeout` has no series in this contract.
pub const LABEL_CONTRACT: &[&[(&str, &str)]] = &[
    &[("stage", "demand"), ("outcome", "accepted")],
    &[("stage", "plan"), ("outcome", "captured")],
    &[("stage", "invocation"), ("outcome", "attempted")],
    &[("stage", "invocation"), ("outcome", "ok")],
    &[("stage", "invocation"), ("outcome", "degraded")],
    &[("stage", "invocation"), ("outcome", "timeout")],
    &[("stage", "invocation"), ("outcome", "error")],
    &[("stage", "terminal"), ("outcome", "resolved")],
    &[("stage", "terminal"), ("outcome", "partial")],
    &[("stage", "terminal"), ("outcome", "unresolved")],
    &[("stage", "rejection"), ("reason", "validation")],
    &[("stage", "rejection"), ("reason", "persistence")],
    &[("stage", "rejection"), ("reason", "no_frozen_plan")],
    &[("stage", "rejection"), ("reason", "already_finalized")],
    &[("stage", "rejection"), ("reason", "unknown_check")],
    &[("stage", "rejection"), ("reason", "method_mismatch")],
    &[("stage", "rejection"), ("reason", "invalid_anchor")],
];

pub fn record(stage: EvidenceStage, outcome: EvidenceOutcome) { metrics::counter!("djinn_evidence_rollout_total", "stage" => stage.label(), "outcome" => outcome.label()).increment(1); }
pub fn reject(reason: EvidenceRejection) { metrics::counter!("djinn_evidence_rollout_total", "stage" => EvidenceStage::Rejection.label(), "reason" => reason.label()).increment(1); }

/// Call only after a new durable lifecycle receipt was inserted. In particular,
/// callers must not invoke this for an `AlreadyRecorded` recovery result.
pub fn terminal(outcome: EvidenceOutcome) {
    debug_assert!(matches!(outcome, EvidenceOutcome::Resolved | EvidenceOutcome::Partial | EvidenceOutcome::Unresolved));
    record(EvidenceStage::Terminal, outcome);
}

/// Records the bounded result of a distinct durable invocation attempt.
pub fn invocation_result(timed_out: bool, runner_failed: bool, exit_code: Option<i32>) {
    let outcome = if timed_out { EvidenceOutcome::Timeout }
    else if runner_failed { EvidenceOutcome::Error }
    else if exit_code == Some(0) { EvidenceOutcome::Ok }
    else { EvidenceOutcome::Degraded };
    record(EvidenceStage::Invocation, outcome);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_contract_is_closed_and_identifier_free() {
        let expected: Vec<Vec<(String, String)>> = serde_json::from_str(include_str!(
            "../tests/fixtures/evidence_metrics_labels.json"
        ))
        .expect("metrics label fixture is valid");
        let actual: Vec<Vec<(String, String)>> = LABEL_CONTRACT
            .iter()
            .map(|labels| {
                labels
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect()
            })
            .collect();
        assert_eq!(actual, expected, "fixture enumerates every legal series");
        let identity_bearing = [
            "proposal_id",
            "task_id",
            "session_id",
            "plan_id",
            "invocation_id",
            "commit_sha",
            "worktree_fingerprint",
        ];
        for labels in LABEL_CONTRACT {
            assert_eq!(labels.iter().filter(|(key, _)| *key == "stage").count(), 1);
            for (key, value) in *labels {
                assert!(!identity_bearing.iter().any(|forbidden| key.contains(forbidden)));
                assert!(!identity_bearing.iter().any(|forbidden| value.contains(forbidden)));
                assert!(!value.contains('='));
                assert!(!value.chars().any(|character| character.is_ascii_digit()));
            }
        }
    }
}
