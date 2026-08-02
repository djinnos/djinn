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

/// Exhaustive metric label contract; no identity-bearing label key is legal.
pub const LABEL_CONTRACT: &[(&str, &[&str])] = &[
    ("stage", &["demand", "plan", "invocation", "terminal", "rejection"]),
    ("outcome", &["accepted", "captured", "attempted", "ok", "degraded", "timeout", "error", "resolved", "partial", "unresolved", "failed"]),
    ("reason", &["validation", "persistence", "no_frozen_plan", "already_finalized", "unknown_check", "method_mismatch", "invalid_anchor"]),
];

pub fn record(stage: EvidenceStage, outcome: EvidenceOutcome) { metrics::counter!("djinn_evidence_rollout_total", "stage" => stage.label(), "outcome" => outcome.label()).increment(1); }
pub fn reject(reason: EvidenceRejection) { metrics::counter!("djinn_evidence_rollout_total", "stage" => EvidenceStage::Rejection.label(), "reason" => reason.label()).increment(1); }

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
        let expected = [
            ("stage", "demand,plan,invocation,terminal,rejection"),
            ("outcome", "accepted,captured,attempted,ok,degraded,timeout,error,resolved,partial,unresolved,failed"),
            ("reason", "validation,persistence,no_frozen_plan,already_finalized,unknown_check,method_mismatch,invalid_anchor"),
        ];
        let actual: Vec<(&str, String)> = LABEL_CONTRACT.iter().map(|(key, values)| (*key, values.join(","))).collect();
        assert_eq!(actual, expected.map(|(key, values)| (key, values.to_owned())));
        let forbidden_keys = [
            "proposal", "task", "session", "plan", "invocation", "commit", "worktree",
        ];
        for (key, values) in LABEL_CONTRACT {
            assert!(!forbidden_keys.contains(key));
            for value in *values {
                assert!(!value.contains('='));
                assert!(!value.chars().any(|character| character.is_ascii_digit()));
            }
        }
    }
}
