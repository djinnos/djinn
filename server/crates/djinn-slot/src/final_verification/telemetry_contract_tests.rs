use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);
struct CapturedLogsWriter(Arc<Mutex<Vec<u8>>>);
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogsWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CapturedLogsWriter(Arc::clone(&self.0))
    }
}
impl Write for CapturedLogsWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn capture_events(f: impl FnOnce()) -> String {
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_target(false)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let guard = tracing::dispatcher::set_default(&dispatch);
    f();
    drop(guard);
    String::from_utf8(logs.0.lock().unwrap().clone()).unwrap()
}
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
fn samples<'a>(rendered: &'a str, metric: &str) -> Vec<&'a str> {
    rendered
        .lines()
        .filter(|line| line.starts_with(metric) && !line.starts_with('#'))
        .collect()
}

#[test]
fn lookup_metrics_are_exactly_bounded_and_audit_fields_are_structured() {
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
    assert_eq!(observed.len(), 5);
    for outcome in ["hit", "miss", "stale", "error", "disabled"] {
        assert!(
            observed.iter().any(
                |line| *line == format!("verify_cache_lookup_total{{outcome=\"{outcome}\"}} 1")
            )
        );
    }
    for line in &observed {
        assert!(
            line.starts_with("verify_cache_lookup_total{outcome=\"")
                && line.matches('=').count() == 1,
            "unexpected lookup labels: {line}"
        );
    }
    let events = capture_events(|| {
        emit_lookup_outcome(
            "hit",
            &request,
            "attempt-id-must-not-be-a-label",
            "reason-must-not-be-a-label",
            "detail-must-not-be-a-label",
        )
    });
    for value in [
        "task-id-must-not-be-a-label",
        "run-id-must-not-be-a-label",
        "attempt-id-must-not-be-a-label",
        "reason-must-not-be-a-label",
        "detail-must-not-be-a-label",
    ] {
        assert!(
            events.contains(value),
            "missing structured audit value {value}: {events}"
        );
        assert!(
            !rendered.contains(value),
            "audit value leaked into metric: {value}"
        );
    }
}

#[test]
fn recording_metrics_are_exactly_bounded_and_evidence_is_structured() {
    let request = request();
    let (_, rendered) = djinn_telemetry::render_isolated(|| {
        emit_outcome(
            &request,
            FinalVerificationRecordingOutcome::Stored {
                verification_attempt_id: "attempt".into(),
                verify_run_id: "verify-run".into(),
                evidence: evidence(),
            },
        );
        emit_outcome(
            &request,
            FinalVerificationRecordingOutcome::Ineligible {
                verification_attempt_id: "attempt".into(),
                reason: "reason-must-not-be-a-label".into(),
            },
        );
        emit_outcome(
            &request,
            FinalVerificationRecordingOutcome::Error {
                verification_attempt_id: "attempt".into(),
                detail: "detail-must-not-be-a-label".into(),
            },
        );
        emit_outcome(
            &request,
            FinalVerificationRecordingOutcome::Reused {
                verification_attempt_id: "attempt".into(),
                evidence: evidence(),
            },
        );
    });
    let observed = samples(&rendered, "verify_run_record_total");
    assert_eq!(observed.len(), 3, "reuse must not be a stored write");
    for outcome in ["stored", "ineligible", "error"] {
        assert!(
            observed
                .iter()
                .any(|line| *line == format!("verify_run_record_total{{outcome=\"{outcome}\"}} 1"))
        );
    }
    for line in &observed {
        assert!(
            line.starts_with("verify_run_record_total{outcome=\"")
                && line.matches('=').count() == 1,
            "unexpected recording labels: {line}"
        );
    }
    let events = capture_events(|| {
        emit_outcome(
            &request,
            FinalVerificationRecordingOutcome::Stored {
                verification_attempt_id: "attempt-id-must-not-be-a-label".into(),
                verify_run_id: "verify-run".into(),
                evidence: evidence(),
            },
        );
    });
    for value in [
        "task-id-must-not-be-a-label",
        "run-id-must-not-be-a-label",
        "attempt-id-must-not-be-a-label",
        "candidate-id-must-not-be-a-label",
        "command-must-not-be-a-label",
        "check-must-not-be-a-label",
        "fingerprint-must-not-be-a-label",
        "manifest-must-not-be-a-label",
        "identity-must-not-be-a-label",
    ] {
        assert!(
            events.contains(value),
            "missing structured audit value {value}: {events}"
        );
        assert!(
            !rendered.contains(value),
            "audit value leaked into metric: {value}"
        );
    }
}
