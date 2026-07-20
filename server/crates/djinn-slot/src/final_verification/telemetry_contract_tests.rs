//! Exact bounded-metric contract tests for final-verification coordinator funnels.

use super::*;

fn request() -> FinalVerificationCoordinatorRequest {
    FinalVerificationCoordinatorRequest {
        task_id: "task-id-must-not-be-a-label".into(),
        task_run_id: "run-id-must-not-be-a-label".into(),
        cancellation: CancellationToken::new(),
    }
}

fn evidence() -> Box<FinalVerificationSuccessEvidence> {
    Box::new(FinalVerificationSuccessEvidence {
        persisted_run_id: "candidate-id-must-not-be-a-label".into(),
        completed_at: "now".into(),
        ordered_commands: serde_json::json!(["command-must-not-be-a-label"]),
        covered_checks: serde_json::json!(["check-must-not-be-a-label"]),
        required_checks: vec!["check-must-not-be-a-label".into()],
        verification_input_fingerprint: "fingerprint-must-not-be-a-label".into(),
        manifest_version: "manifest-must-not-be-a-label".into(),
        environment_identity_digest: "identity-must-not-be-a-label".into(),
    })
}

fn samples(rendered: &str, metric: &str) -> Vec<&str> {
    rendered
        .lines()
        .filter(|line| line.starts_with(metric) && !line.starts_with("#"))
        .collect()
}

#[test]
fn lookup_funnel_has_exactly_one_single_label_sample_per_allowed_outcome() {
    let request = request();
    let (_, rendered) = djinn_telemetry::render_isolated(|| {
        for outcome in ["hit", "miss", "stale", "error", "disabled"] {
            emit_lookup_outcome(
                outcome,
                &request,
                "attempt-id-must-not-be-a-label",
                "reason-must-not-be-a-label",
                "detail-must-not-be-a-label",
            );
        }
    });
    let observed = samples(&rendered, "verify_cache_lookup_total");
    assert_eq!(observed.len(), 5, "one sample for every allowed lookup outcome");
    for outcome in ["hit", "miss", "stale", "error", "disabled"] {
        assert!(observed.iter().any(|line| *line == format!("verify_cache_lookup_total{{outcome=\"{outcome}\"}} 1")));
    }
    for secret in ["task-id", "run-id", "attempt-id", "reason-must", "detail-must"] {
        assert!(!rendered.contains(secret), "audit value leaked into labels: {secret}");
    }
}

#[test]
fn recording_funnel_has_exactly_one_single_label_sample_per_writer_outcome() {
    let request = request();
    let (_, rendered) = djinn_telemetry::render_isolated(|| {
        emit_outcome(&request, FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id: "attempt".into(), verify_run_id: "verify-run".into(), evidence: evidence(),
        });
        emit_outcome(&request, FinalVerificationRecordingOutcome::Ineligible {
            verification_attempt_id: "attempt".into(), reason: "reason-must-not-be-a-label".into(),
        });
        emit_outcome(&request, FinalVerificationRecordingOutcome::Error {
            verification_attempt_id: "attempt".into(), detail: "detail-must-not-be-a-label".into(),
        });
        emit_outcome(&request, FinalVerificationRecordingOutcome::Reused {
            verification_attempt_id: "attempt".into(), evidence: evidence(),
        });
    });
    let observed = samples(&rendered, "verify_run_record_total");
    assert_eq!(observed.len(), 3, "reuse must not create a stored-write sample");
    for outcome in ["stored", "ineligible", "error"] {
        assert!(observed.iter().any(|line| *line == format!("verify_run_record_total{{outcome=\"{outcome}\"}} 1")));
    }
    for secret in ["task-id", "candidate-id", "command-must", "check-must", "fingerprint-must", "manifest-must", "identity-must", "reason-must", "detail-must"] {
        assert!(!rendered.contains(secret), "audit value leaked into labels: {secret}");
    }
}

#[test]
fn injected_telemetry_failure_preserves_terminal_decisions_without_retrying() {
    let request = request();
    let (_, rendered) = djinn_telemetry::render_isolated(|| {
        djinn_telemetry::final_verification::fail_next_emission_for_test();
        let hit = lookup_none::<()>("hit", &request, "attempt", "hit", "");
        assert_eq!(hit, None, "failed telemetry cannot change reuse decision");

        djinn_telemetry::final_verification::fail_next_emission_for_test();
        let stored = emit_outcome(&request, FinalVerificationRecordingOutcome::Stored {
            verification_attempt_id: "attempt".into(), verify_run_id: "run".into(), evidence: evidence(),
        });
        assert!(matches!(stored, FinalVerificationRecordingOutcome::Stored { .. }), "failed telemetry cannot change persistence eligibility");

        emit_lookup_outcome("miss", &request, "attempt", "fallback", "");
        let fallback = emit_outcome(&request, FinalVerificationRecordingOutcome::Ineligible {
            verification_attempt_id: "attempt".into(), reason: "fallback execution decision".into(),
        });
        assert!(matches!(fallback, FinalVerificationRecordingOutcome::Ineligible { .. }), "failed telemetry cannot change fallback completion decision");
    });
    assert_eq!(samples(&rendered, "verify_cache_lookup_total").len(), 1, "failed hit was not retried; fallback emitted once");
    assert_eq!(samples(&rendered, "verify_run_record_total").len(), 1, "failed stored emission was not retried; fallback terminal writer outcome emitted once");
}
